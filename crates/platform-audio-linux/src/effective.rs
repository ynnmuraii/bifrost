//! Read-only PipeWire control seam. Stable v0 SPA/PW interface prefixes are declared here
//! rather than requiring SDK headers at build time. No streams, nodes or setters are created.
use audio_backend::BackendError;
use libloading::Library;
use std::{
    ffi::{c_char, c_int, c_void, CStr, CString},
    mem::{size_of, transmute_copy},
    os::fd::IntoRawFd,
    os::unix::net::UnixStream,
    path::PathBuf,
    ptr,
    sync::LazyLock,
    time::{Duration, Instant},
};

const PROPS: u32 = 2;
const ASYNC: i32 = 1 << 30;
const LIMIT: u32 = 64;
const DEADLINE: Duration = Duration::from_secs(3);
const NODE_TYPE: &CStr = c"PipeWire:Interface:Node";
type Object = *mut c_void;
type AddListener = unsafe extern "C" fn(Object, *mut Hook, *const c_void, Object) -> c_int;
type Sync = unsafe extern "C" fn(Object, u32, c_int) -> c_int;
type GetRegistry = unsafe extern "C" fn(Object, u32, usize) -> Object;
type Bind = unsafe extern "C" fn(Object, u32, *const c_char, u32, usize) -> Object;
type Subscribe = unsafe extern "C" fn(Object, *const u32, u32) -> c_int;
type Enum = unsafe extern "C" fn(Object, c_int, u32, u32, u32, *const c_void) -> c_int;
type Enter = unsafe extern "C" fn(Object);
type Iterate = unsafe extern "C" fn(Object, c_int) -> c_int;

#[repr(C)]
struct Callbacks {
    functions: *const c_void,
    data: Object,
}
#[repr(C)]
struct Interface {
    kind: *const c_char,
    version: u32,
    callbacks: Callbacks,
}
#[repr(C)]
struct NativeLoop {
    system: Object,
    loop_: Object,
    control: Object,
}
#[repr(C)]
#[derive(Default)]
struct Hook {
    words: [usize; 6],
}
#[repr(C)]
struct DictItem {
    key: *const c_char,
    value: *const c_char,
}
#[repr(C)]
struct Dict {
    flags: u32,
    count: u32,
    items: *const DictItem,
}
#[repr(C)]
struct CoreInfo {
    id: u32,
    cookie: u32,
}
#[repr(C)]
struct NodeInfo {
    id: u32,
    max_input: u32,
    max_output: u32,
    change_mask: u64,
    inputs: u32,
    outputs: u32,
    state: c_int,
    error: *const c_char,
    props: *const Dict,
}
#[repr(C)]
struct CoreEvents {
    version: u32,
    info: Option<unsafe extern "C" fn(Object, *const CoreInfo)>,
    done: Option<unsafe extern "C" fn(Object, u32, c_int)>,
    ping: Option<unsafe extern "C" fn(Object, u32, c_int)>,
    error: Option<unsafe extern "C" fn(Object, u32, c_int, c_int, *const c_char)>,
    unused: [Option<unsafe extern "C" fn()>; 5],
}
#[repr(C)]
struct RegistryEvents {
    version: u32,
    global: Option<unsafe extern "C" fn(Object, u32, u32, *const c_char, u32, *const Dict)>,
    removed: Option<unsafe extern "C" fn(Object, u32)>,
}
#[repr(C)]
struct NodeEvents {
    version: u32,
    info: Option<unsafe extern "C" fn(Object, *const NodeInfo)>,
    param: Option<unsafe extern "C" fn(Object, c_int, u32, u32, u32, *const c_void)>,
}

struct Api {
    _library: Library,
    init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    main_new: unsafe extern "C" fn(*const Dict) -> Object,
    main_destroy: unsafe extern "C" fn(Object),
    main_loop: unsafe extern "C" fn(Object) -> *mut NativeLoop,
    properties: unsafe extern "C" fn(*const c_char) -> Object,
    context_new: unsafe extern "C" fn(*mut NativeLoop, Object, usize) -> Object,
    context_destroy: unsafe extern "C" fn(Object),
    connect_fd: unsafe extern "C" fn(Object, c_int, Object, usize) -> Object,
    disconnect: unsafe extern "C" fn(Object) -> c_int,
    destroy_proxy: unsafe extern "C" fn(Object),
}
impl Api {
    fn get() -> Result<&'static Self, BackendError> {
        static API: LazyLock<Result<Api, BackendError>> = LazyLock::new(|| unsafe {
            // Only the stable system SONAME; absence is genuine Unsupported.
            let library =
                Library::new("libpipewire-0.3.so.0").map_err(|_| BackendError::Unsupported)?;
            macro_rules! symbol {
                ($name:literal) => {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|_| BackendError::Unsupported)?
                };
            }
            let api = Api {
                init: symbol!("pw_init"),
                main_new: symbol!("pw_main_loop_new"),
                main_destroy: symbol!("pw_main_loop_destroy"),
                main_loop: symbol!("pw_main_loop_get_loop"),
                properties: symbol!("pw_properties_new_string"),
                context_new: symbol!("pw_context_new"),
                context_destroy: symbol!("pw_context_destroy"),
                connect_fd: symbol!("pw_context_connect_fd"),
                disconnect: symbol!("pw_core_disconnect"),
                destroy_proxy: symbol!("pw_proxy_destroy"),
                _library: library,
            };
            (api.init)(ptr::null_mut(), ptr::null_mut());
            Ok(api)
        });
        API.as_ref().map_err(|error| *error)
    }
}

// Methods are SPA interface dispatch, not exported C functions. All used slots are
// from the stable v0 Core/Registry/Node and loop-control prefixes (PW 0.3 ABI).
unsafe fn method<T: Copy>(object: Object, slot: usize) -> Result<(T, Object), BackendError> {
    unsafe {
        let interface = object
            .cast::<Interface>()
            .as_ref()
            .ok_or(BackendError::ServerUnavailable)?;
        let table = interface.callbacks.functions;
        if table.is_null() || size_of::<T>() != size_of::<usize>() {
            return Err(BackendError::Unsupported);
        }
        let address = table
            .cast::<u8>()
            .add(size_of::<usize>())
            .cast::<Object>()
            .add(slot)
            .read();
        if address.is_null() {
            return Err(BackendError::Unsupported);
        }
        Ok((transmute_copy(&address), interface.callbacks.data))
    }
}
struct Entered {
    leave: Enter,
    data: Object,
}
impl Drop for Entered {
    fn drop(&mut self) {
        unsafe { (self.leave)(self.data) };
    }
}
unsafe fn enter(loop_: *mut NativeLoop) -> Result<Entered, BackendError> {
    unsafe {
        let control = loop_
            .as_ref()
            .ok_or(BackendError::ServerUnavailable)?
            .control;
        let (enter, data) = method::<Enter>(control, 2)?;
        let (leave, _) = method::<Enter>(control, 3)?;
        enter(data);
        Ok(Entered { leave, data })
    }
}

struct Observation {
    name: CString,
    serial: u64,
    expected_cookie: u32,
    core: Object,
    registry: Object,
    node: Object,
    node_id: u32,
    cookie: Option<u32>,
    epoch: u64,
    enum_epoch: u64,
    requested: i32,
    returned: i32,
    fence: i32,
    done: i32,
    collecting: bool,
    pending: bool,
    providers: u32,
    all_false: bool,
    false_seen: bool,
    error: Option<BackendError>,
    core_hook: Hook,
    registry_hook: Hook,
    node_hook: Hook,
}
impl Observation {
    fn changed(&mut self) {
        self.pending = true;
        if let Some(epoch) = self.epoch.checked_add(1) {
            self.epoch = epoch;
        } else {
            self.error = Some(BackendError::StaleRevision);
        }
    }
    fn fail(&mut self, error: BackendError) {
        self.error = Some(error);
        self.changed();
    }
    fn ready(&self) -> bool {
        !self.node.is_null() && self.cookie == Some(self.expected_cookie) && self.error.is_none()
    }
    fn begin_collection(&mut self) {
        self.enum_epoch = self.epoch;
        self.providers = 0;
        self.all_false = true;
        self.false_seen = false;
        self.returned = -1;
        self.collecting = true;
    }
    fn false_complete(&self) -> bool {
        self.ready()
            && !self.collecting
            && self.returned >= 0
            && self.returned & ASYNC != 0
            && self.fence >= 0
            && self.done == self.fence
            && self.providers > 0
            && self.all_false
            && self.false_seen
            && self.enum_epoch == self.epoch
    }
}

pub(crate) struct EffectiveMonitor {
    api: &'static Api,
    main: Object,
    loop_: *mut NativeLoop,
    context: Object,
    observation: Box<Observation>,
}
// There is no native loop thread. Every exclusive control call enters/leaves its
// loop; the stable boxed callback data is never moved. This permits bfa-stop transfer.
unsafe impl Send for EffectiveMonitor {}
impl EffectiveMonitor {
    pub(crate) fn connect(
        name: &str,
        serial: u64,
        server_cookie: u32,
    ) -> Result<Self, BackendError> {
        if serial == 0 || server_cookie == 0 {
            return Err(BackendError::Unsupported);
        }
        let name = CString::new(name).map_err(|_| BackendError::InvalidArgument)?;
        let api = Api::get()?;
        let mut this = Self {
            api,
            main: ptr::null_mut(),
            loop_: ptr::null_mut(),
            context: ptr::null_mut(),
            observation: Box::new(Observation {
                name,
                serial,
                expected_cookie: server_cookie,
                core: ptr::null_mut(),
                registry: ptr::null_mut(),
                node: ptr::null_mut(),
                node_id: u32::MAX,
                cookie: None,
                epoch: 1,
                enum_epoch: 0,
                requested: 0,
                returned: -1,
                fence: -1,
                done: -1,
                collecting: false,
                pending: true,
                providers: 0,
                all_false: false,
                false_seen: false,
                error: None,
                core_hook: Hook::default(),
                registry_hook: Hook::default(),
                node_hook: Hook::default(),
            }),
        };
        unsafe {
            this.main = (api.main_new)(ptr::null());
            if this.main.is_null() {
                return Err(BackendError::ServerUnavailable);
            }
            this.loop_ = (api.main_loop)(this.main);
            let _entered = enter(this.loop_)?;
            let props = (api.properties)(c"config.name = client.conf".as_ptr());
            if props.is_null() {
                return Err(BackendError::ServerUnavailable);
            }
            this.context = (api.context_new)(this.loop_, props, 0);
            if this.context.is_null() {
                return Err(BackendError::ServerUnavailable);
            }
            let socket =
                UnixStream::connect(native_path()?).map_err(|_| BackendError::ServerUnavailable)?;
            this.observation.core =
                (api.connect_fd)(this.context, socket.into_raw_fd(), ptr::null_mut(), 0);
            let data = (&mut *this.observation as *mut Observation).cast();
            let (add, object) = method::<AddListener>(this.observation.core, 0)?;
            if add(
                object,
                &mut this.observation.core_hook,
                (&CORE_EVENTS as *const CoreEvents).cast(),
                data,
            ) < 0
            {
                return Err(BackendError::Unsupported);
            }
            let (registry, object) = method::<GetRegistry>(this.observation.core, 5)?;
            this.observation.registry = registry(object, 3, 0);
            let (add, object) = method::<AddListener>(this.observation.registry, 0)?;
            if add(
                object,
                &mut this.observation.registry_hook,
                (&REGISTRY_EVENTS as *const RegistryEvents).cast(),
                data,
            ) < 0
            {
                return Err(BackendError::Unsupported);
            }
        }
        Ok(this)
    }
    fn iterate(&mut self, timeout: i32) -> Result<(), BackendError> {
        unsafe {
            let control = (*self.loop_).control;
            let (iterate, data) = method::<Iterate>(control, 4)?;
            if iterate(data, timeout) < 0 {
                return Err(BackendError::ServerUnavailable);
            }
        }
        if let Some(error) = self.observation.error {
            Err(error)
        } else {
            Ok(())
        }
    }
    fn sync(&mut self) -> Result<i32, BackendError> {
        unsafe {
            let (sync, object) = method::<Sync>(self.observation.core, 2)?;
            let token = sync(object, 0, 0);
            if token < 0 {
                Err(BackendError::ServerUnavailable)
            } else {
                Ok(token)
            }
        }
    }
    fn wait(
        &mut self,
        until: Instant,
        ready: impl Fn(&Observation) -> bool,
        admitted: &dyn Fn() -> bool,
    ) -> Result<(), BackendError> {
        while !ready(&self.observation) {
            if !admitted() {
                return Err(BackendError::Stopped);
            }
            if Instant::now() >= until {
                return Err(BackendError::Timeout);
            }
            self.iterate(1)?;
        }
        Ok(())
    }
    fn query(&mut self, until: Instant, admitted: &dyn Fn() -> bool) -> Result<(), BackendError> {
        if !admitted() {
            return Err(BackendError::Stopped);
        }
        self.wait(until, Observation::ready, admitted)?;
        // Bind/subscription and queued notifications drain BEFORE epoch capture.
        self.observation.done = -1;
        let drain = self.sync()?;
        self.observation.fence = drain;
        self.wait(until, |state| state.done == drain, admitted)?;
        if !self.observation.ready() {
            return Err(BackendError::DeviceGone);
        }
        self.observation.requested = self
            .observation
            .requested
            .checked_add(1)
            .ok_or(BackendError::Busy)?;
        self.observation.begin_collection();
        unsafe {
            let (enumerate, data) = method::<Enum>(self.observation.node, 2)?;
            let returned = enumerate(
                data,
                self.observation.requested,
                PROPS,
                0,
                u32::MAX,
                ptr::null(),
            );
            self.observation.returned = returned;
            if returned < 0 || returned & ASYNC == 0 {
                return Err(BackendError::Unsupported);
            }
        }
        self.observation.done = -1;
        self.observation.fence = self.sync()?;
        self.wait(until, |state| state.done == state.fence, admitted)?;
        self.observation.collecting = false;
        if !self.observation.false_complete() {
            return Err(BackendError::Unsupported);
        }
        self.observation.pending = false;
        Ok(())
    }
    pub(crate) fn fresh_false(&mut self, admitted: &dyn Fn() -> bool) -> Result<(), BackendError> {
        let _entered = unsafe { enter(self.loop_)? };
        let result = self.query(Instant::now() + DEADLINE, admitted);
        if let Err(error) = result {
            if error != BackendError::Stopped {
                self.observation.fail(error);
            }
        }
        result
    }
    pub(crate) fn check(&mut self, admitted: &dyn Fn() -> bool) -> Result<(), BackendError> {
        if !admitted() {
            return Err(BackendError::Stopped);
        }
        let _entered = unsafe { enter(self.loop_)? };
        self.iterate(0)?;
        if self.observation.pending {
            // Exactly one complete strict query, not a first-false shortcut/retry.
            let result = self.query(Instant::now() + DEADLINE, admitted);
            if let Err(error) = result {
                if error != BackendError::Stopped {
                    self.observation.fail(error);
                }
            }
            result
        } else if self.observation.false_complete() {
            Ok(())
        } else {
            Err(BackendError::Unsupported)
        }
    }
}
impl Drop for EffectiveMonitor {
    fn drop(&mut self) {
        unsafe {
            if !self.loop_.is_null() {
                if let Ok(_entered) = enter(self.loop_) {
                    if !self.observation.node.is_null() {
                        (self.api.destroy_proxy)(self.observation.node);
                    }
                    if !self.observation.registry.is_null() {
                        (self.api.destroy_proxy)(self.observation.registry);
                    }
                    if !self.observation.core.is_null() {
                        (self.api.disconnect)(self.observation.core);
                    }
                    if !self.context.is_null() {
                        (self.api.context_destroy)(self.context);
                    }
                }
            }
            if !self.main.is_null() {
                (self.api.main_destroy)(self.main);
            }
        }
    }
}

fn native_path() -> Result<PathBuf, BackendError> {
    let remote = std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
    let remote = PathBuf::from(remote);
    if remote.is_absolute() {
        return Ok(remote);
    }
    if remote.components().count() != 1 || remote.as_os_str().is_empty() {
        return Err(BackendError::Unsupported);
    }
    let runtime = std::env::var_os("PIPEWIRE_RUNTIME_DIR")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .ok_or(BackendError::ServerUnavailable)?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        return Err(BackendError::Unsupported);
    }
    Ok(runtime.join(remote))
}
unsafe fn text<'a>(value: *const c_char, limit: usize) -> Option<&'a CStr> {
    if value.is_null() {
        return None;
    }
    unsafe {
        for len in 0..=limit {
            if *value.add(len) == 0 {
                return CStr::from_bytes_with_nul(std::slice::from_raw_parts(
                    value.cast(),
                    len + 1,
                ))
                .ok();
            }
        }
    }
    None
}
unsafe fn property<'a>(dict: *const Dict, key: &CStr) -> Option<&'a CStr> {
    unsafe {
        let dict = dict.as_ref()?;
        if dict.count > 4096 || dict.items.is_null() {
            return None;
        }
        for item in std::slice::from_raw_parts(dict.items, dict.count as usize) {
            if text(item.key, 128)? == key {
                return text(item.value, 512);
            }
        }
        None
    }
}
unsafe fn serial(dict: *const Dict) -> Option<u64> {
    unsafe {
        property(dict, c"object.serial")?
            .to_str()
            .ok()?
            .parse::<u64>()
            .ok()
            .filter(|value| *value != 0)
    }
}
unsafe extern "C" fn core_info(data: Object, info: *const CoreInfo) {
    unsafe {
        let state = &mut *data.cast::<Observation>();
        let Some(info) = info.as_ref() else {
            state.fail(BackendError::ServerUnavailable);
            return;
        };
        // PipeWire's Pulse GetServerInfo returns pw_manager_core_cookie(), not a
        // socket/name heuristic. Match the catalog's instance before Props admission.
        if info.cookie != state.expected_cookie
            || state.cookie.is_some_and(|cookie| cookie != info.cookie)
        {
            state.fail(BackendError::ServerUnavailable);
        } else {
            state.cookie = Some(info.cookie);
        }
    }
}
unsafe extern "C" fn core_done(data: Object, id: u32, token: i32) {
    unsafe {
        let state = &mut *data.cast::<Observation>();
        if id == 0 && token == state.fence {
            state.done = token;
        }
    }
}
unsafe extern "C" fn core_error(data: Object, _: u32, _: i32, _: i32, _: *const c_char) {
    unsafe {
        (*data.cast::<Observation>()).fail(BackendError::ServerUnavailable);
    }
}
unsafe extern "C" fn global(
    data: Object,
    id: u32,
    _: u32,
    kind: *const c_char,
    version: u32,
    props: *const Dict,
) {
    unsafe {
        let state = &mut *data.cast::<Observation>();
        if text(kind, 128) != Some(NODE_TYPE) || serial(props) != Some(state.serial) {
            return;
        }
        if property(props, c"node.name") != Some(state.name.as_c_str())
            || id == 0
            || id == u32::MAX
            || version < 3
        {
            state.fail(BackendError::DeviceGone);
            return;
        }
        if !state.node.is_null() {
            if state.node_id != id {
                state.fail(BackendError::DeviceGone);
            }
            return;
        }
        let registry = state.registry;
        let result = (|| -> Result<(), BackendError> {
            let (bind, object) = method::<Bind>(registry, 1)?;
            let node = bind(object, id, NODE_TYPE.as_ptr(), 3, 0);
            if node.is_null() {
                return Err(BackendError::Unsupported);
            }
            let state_ptr = data.cast::<Observation>();
            (*state_ptr).node = node;
            (*state_ptr).node_id = id;
            let (add, object) = method::<AddListener>(node, 0)?;
            if add(
                object,
                &raw mut (*state_ptr).node_hook,
                (&NODE_EVENTS as *const NodeEvents).cast(),
                data,
            ) < 0
            {
                return Err(BackendError::Unsupported);
            }
            let (subscribe, object) = method::<Subscribe>(node, 1)?;
            if subscribe(object, &PROPS, 1) < 0 {
                return Err(BackendError::Unsupported);
            }
            Ok(())
        })();
        if let Err(error) = result {
            (*data.cast::<Observation>()).fail(error);
        }
    }
}
unsafe extern "C" fn removed(data: Object, id: u32) {
    unsafe {
        let state = &mut *data.cast::<Observation>();
        if id == state.node_id {
            state.fail(BackendError::DeviceGone);
        }
    }
}
unsafe extern "C" fn node_info(data: Object, info: *const NodeInfo) {
    unsafe {
        let state = &mut *data.cast::<Observation>();
        let Some(info) = info.as_ref() else {
            state.fail(BackendError::DeviceGone);
            return;
        };
        if info.id != state.node_id || info.state < 0 {
            state.fail(BackendError::DeviceGone);
            return;
        }
        if info.change_mask & 8 != 0 {
            if serial(info.props) != Some(state.serial)
                || property(info.props, c"node.name") != Some(state.name.as_c_str())
            {
                state.fail(BackendError::DeviceGone);
                return;
            }
            state.changed();
        }
        if info.change_mask & 16 != 0 {
            state.changed();
        }
    }
}
unsafe extern "C" fn node_param(
    data: Object,
    token: i32,
    id: u32,
    _: u32,
    _: u32,
    pod: *const c_void,
) {
    unsafe {
        let state = &mut *data.cast::<Observation>();
        if !state.collecting || token != state.returned || id != PROPS {
            state.changed();
            return;
        }
        if state.providers >= LIMIT || pod.is_null() {
            state.fail(BackendError::Unsupported);
            return;
        }
        let length = pod.cast::<u32>().read_unaligned() as usize;
        if length > 65536 {
            state.fail(BackendError::Unsupported);
            return;
        }
        let bytes = std::slice::from_raw_parts(pod.cast::<u8>(), length + 8);
        state.providers += 1;
        match provider_flag(bytes) {
            Ok(Some(false)) => state.false_seen = true,
            Ok(None) => {} // A valid auxiliary Props object supplies no flag evidence.
            _ => {
                state.all_false = false;
                state.fail(BackendError::Unsupported);
            }
        }
    }
}
static CORE_EVENTS: CoreEvents = CoreEvents {
    version: 0,
    info: Some(core_info),
    done: Some(core_done),
    ping: None,
    error: Some(core_error),
    unused: [None; 5],
};
static REGISTRY_EVENTS: RegistryEvents = RegistryEvents {
    version: 0,
    global: Some(global),
    removed: Some(removed),
};
static NODE_EVENTS: NodeEvents = NodeEvents {
    version: 0,
    info: Some(node_info),
    param: Some(node_param),
};

fn number(bytes: &[u8], offset: usize) -> Result<u32, BackendError> {
    let raw = bytes
        .get(offset..offset.checked_add(4).ok_or(BackendError::Unsupported)?)
        .ok_or(BackendError::Unsupported)?;
    Ok(u32::from_ne_bytes(
        raw.try_into().map_err(|_| BackendError::Unsupported)?,
    ))
}
fn pod(bytes: &[u8], offset: usize) -> Result<(u32, &[u8], usize), BackendError> {
    let length = number(bytes, offset)? as usize;
    let kind = number(bytes, offset + 4)?;
    let start = offset.checked_add(8).ok_or(BackendError::Unsupported)?;
    let end = start.checked_add(length).ok_or(BackendError::Unsupported)?;
    let body = bytes.get(start..end).ok_or(BackendError::Unsupported)?;
    let next = end.checked_add(7).ok_or(BackendError::Unsupported)? & !7;
    Ok((kind, body, next))
}
/// Read actual flag evidence; unrelated well-formed Props never substitute a false.
fn provider_flag(bytes: &[u8]) -> Result<Option<bool>, BackendError> {
    let (kind, body, _) = pod(bytes, 0)?;
    if kind != 15 || number(body, 0)? != 0x40002 {
        return Err(BackendError::Unsupported);
    }
    let mut offset = 8;
    let mut keys = 0;
    while offset < body.len() {
        let key = number(body, offset)?;
        let (kind, params, next) = pod(body, offset + 8)?;
        offset = next;
        if key != 0x80001 {
            continue;
        }
        if kind != 14 {
            return Err(BackendError::Unsupported);
        }
        let mut item = 0;
        let mut count = 0;
        while item < params.len() {
            count += 1;
            if count > 128 {
                return Err(BackendError::Unsupported);
            }
            let (kind, name, next) = pod(params, item)?;
            if kind != 8 {
                return Err(BackendError::Unsupported);
            }
            let name = CStr::from_bytes_with_nul(name).map_err(|_| BackendError::Unsupported)?;
            let (kind, value, next) = pod(params, next)?;
            item = next;
            if name != c"monitor.channel-volumes" {
                continue;
            }
            keys += 1;
            if keys != 1 || kind != 2 || value.len() != 4 || number(value, 0)? != 0 {
                return Err(BackendError::Unsupported);
            }
        }
    }
    if keys == 1 {
        Ok(Some(false))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        result.extend_from_slice(&(body.len() as u32).to_ne_bytes());
        result.extend_from_slice(&kind.to_ne_bytes());
        result.extend_from_slice(body);
        while result.len() % 8 != 0 {
            result.push(0);
        }
        result
    }
    fn props(values: &[(u32, u32)]) -> Vec<u8> {
        named_props(b"monitor.channel-volumes\0", values)
    }
    fn named_props(name: &[u8], values: &[(u32, u32)]) -> Vec<u8> {
        let mut params = Vec::new();
        for (kind, value) in values {
            params.extend(encoded(8, name));
            params.extend(encoded(*kind, &value.to_ne_bytes()));
        }
        let mut body = Vec::new();
        body.extend_from_slice(&0x40002u32.to_ne_bytes());
        body.extend_from_slice(&PROPS.to_ne_bytes());
        body.extend_from_slice(&0x80001u32.to_ne_bytes());
        body.extend_from_slice(&0u32.to_ne_bytes());
        body.extend(encoded(14, &params));
        encoded(15, &body)
    }
    #[test]
    fn flag_evidence_rejects_true_wrong_duplicates_and_malformed() {
        assert_eq!(provider_flag(&props(&[(2, 0)])), Ok(Some(false)));
        assert_eq!(provider_flag(&props(&[])), Ok(None));
        for values in [vec![(2, 1)], vec![(4, 0)], vec![(2, 0), (2, 0)]] {
            assert_eq!(
                provider_flag(&props(&values)),
                Err(BackendError::Unsupported)
            );
        }
        let valid = props(&[(2, 0)]);
        assert!(provider_flag(&valid[..valid.len() - 9]).is_err());
    }
    #[test]
    fn native_callback_uses_full_return_token_and_fenced_epoch() {
        let mut state = Observation {
            name: CString::new("sink").unwrap(),
            serial: 42,
            core: ptr::null_mut(),
            expected_cookie: 5,
            registry: ptr::null_mut(),
            node: ptr::dangling_mut(),
            node_id: 9,
            cookie: Some(5),
            epoch: 1,
            enum_epoch: 1,
            requested: 1,
            returned: ASYNC | 37,
            fence: 13,
            done: -1,
            collecting: true,
            pending: false,
            providers: 0,
            all_false: true,
            false_seen: false,
            error: None,
            core_hook: Hook::default(),
            registry_hook: Hook::default(),
            node_hook: Hook::default(),
        };
        let data = (&mut state as *mut Observation).cast();
        let bytes = props(&[(2, 0)]);
        unsafe {
            node_param(data, ASYNC | 37, PROPS, 0, 1, bytes.as_ptr().cast());
            core_done(data, 0, 13);
        }
        state.collecting = false;
        assert!(state.false_complete());
        // A second instance may have the same name/serial and actual false.
        // Its core cookie must still reject the otherwise complete collection.
        state.expected_cookie = 6;
        assert!(!state.false_complete());
        state.expected_cookie = 5;
        let info = CoreInfo { id: 0, cookie: 6 };
        unsafe { core_info(data, &info) };
        assert_eq!(state.error, Some(BackendError::ServerUnavailable));
        assert!(!state.false_complete());
        state.error = None;
        state.enum_epoch = state.epoch;
        assert!(state.false_complete());
        unsafe {
            node_param(data, 1, PROPS, 0, 1, bytes.as_ptr().cast());
        }
        assert!(!state.false_complete());
    }
    // Representative PODs reconstructed from native JSON, not captured binary PODs.
    #[test]
    fn heterogeneous_props_require_fresh_actual_false_each_collection() {
        let mut state = Observation {
            name: CString::new("sink").unwrap(),
            serial: 42,
            expected_cookie: 5,
            core: ptr::null_mut(),
            registry: ptr::null_mut(),
            node: ptr::dangling_mut(),
            node_id: 9,
            cookie: Some(5),
            epoch: 1,
            enum_epoch: 1,
            requested: 1,
            returned: ASYNC | 37,
            fence: 13,
            done: -1,
            collecting: true,
            pending: false,
            providers: 0,
            all_false: true,
            false_seen: false,
            error: None,
            core_hook: Hook::default(),
            registry_hook: Hook::default(),
            node_hook: Hook::default(),
        };
        let main = props(&[(2, 0)]);
        let auxiliary = named_props(b"audio.channels\0", &[(4, 2)]);
        let collect = |state: &mut Observation, rows: &[&[u8]]| {
            state.begin_collection();
            state.returned = ASYNC | 37;
            state.fence = 13;
            state.done = -1;
            let data = (state as *mut Observation).cast();
            for (index, row) in rows.iter().enumerate() {
                unsafe {
                    node_param(
                        data,
                        ASYNC | 37,
                        PROPS,
                        index as u32,
                        index as u32 + 1,
                        row.as_ptr().cast(),
                    );
                }
            }
            unsafe {
                core_done(data, 0, 13);
            }
            state.collecting = false;
        };
        collect(&mut state, &[&main, &auxiliary]);
        assert!(
            state.false_complete(),
            "actual false plus unrelated valid Props must qualify"
        );
        collect(&mut state, &[&auxiliary]);
        assert!(
            !state.false_complete(),
            "prior false must not qualify a fresh all-missing collection"
        );
        let conflicting = props(&[(2, 1)]);
        collect(&mut state, &[&main, &conflicting]);
        assert!(
            !state.false_complete(),
            "a second actual true key must reject the whole collection"
        );
    }
}

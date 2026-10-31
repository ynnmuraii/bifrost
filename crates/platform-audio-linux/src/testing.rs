//! Explicit opt-in test/dev utility. Never creates defaults or targets an existing endpoint.
use crate::raw::{self, Config, Connection, Raw};
use audio_backend::{BackendError, DeviceId};
use pulseaudio::protocol as p;
use std::{
    io::Read,
    time::{Duration, Instant},
};

pub struct OwnedNullSink {
    module: Option<u32>,
    sink: DeviceId,
    raw: Raw,
    connection: Connection,
}
impl OwnedNullSink {
    pub fn new() -> Result<Self, BackendError> {
        let config = Config::from_env()?;
        let connection = config.client()?;
        let mut raw = config.raw()?;
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|_| BackendError::ServerUnavailable)?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let sink = DeviceId::new(format!("bifrost_test_{suffix}"))?;
        let arguments=format!("sink_name={} rate=48000 channels=2 format=float32le sink_properties='device.description=BifrostTest node.virtual=true priority.session=0'",sink.as_str());
        let module: p::LookupReply = raw.request(p::Command::LoadModule(p::LoadModuleParams {
            name: c"module-null-sink".to_owned(),
            arguments: Some(raw::name(&arguments)?),
        }))?;
        let mut owned = Self {
            module: Some(module.0),
            sink,
            raw,
            connection,
        };
        // Server/device-restore policy may initially mute new virtual sinks. Only this newly
        // owned, silent test endpoint is initialized; production streams never unmute endpoints.
        owned
            .raw
            .ack(p::Command::SetSinkMute(p::SetDeviceMuteParams {
                device_index: None,
                device_name: Some(raw::name(owned.sink.as_str())?),
                mute: false,
            }))?;
        if owned.muted()? {
            return Err(BackendError::Busy);
        }
        Ok(owned)
    }
    pub fn sink(&self) -> &DeviceId {
        &self.sink
    }
    pub fn muted(&self) -> Result<bool, BackendError> {
        raw::wait(
            self.connection
                .client
                .sink_info_by_name(raw::name(self.sink.as_str())?),
        )
        .map(|info| info.muted)
    }
    pub fn monitor(&self) -> Result<DeviceId, BackendError> {
        let sink = raw::wait(
            self.connection
                .client
                .sink_info_by_name(raw::name(self.sink.as_str())?),
        )?;
        let monitor = sink.monitor_source_name.ok_or(BackendError::Unsupported)?;
        DeviceId::new(
            monitor
                .to_str()
                .map_err(|_| BackendError::InvalidArgument)?,
        )
    }
    pub fn module_id(&self) -> Option<u32> {
        self.module
    }
    pub fn unload(&mut self) -> Result<(), BackendError> {
        let Some(module) = self.module else {
            return Ok(());
        };
        let modules = raw::wait(self.connection.client.list_modules())?;
        let Some(info) = modules.into_iter().find(|entry| entry.index == module) else {
            self.module = None;
            return Ok(());
        };
        let expected = format!("sink_name={}", self.sink.as_str());
        if info.name != c"module-null-sink"
            || info.argument.as_deref().is_none_or(|argument| {
                !argument
                    .to_string_lossy()
                    .split_whitespace()
                    .any(|word| word == expected)
            })
        {
            self.module = None;
            return Err(BackendError::OwnershipLost);
        }
        self.raw.ack(p::Command::UnloadModule(module))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !raw::wait(self.connection.client.list_modules())?
                .iter()
                .any(|entry| entry.index == module)
            {
                self.module = None;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BackendError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for OwnedNullSink {
    fn drop(&mut self) {
        if self.unload().is_err() {
            eprintln!("owned null-sink cleanup failed; inspect remaining_test_modules");
        }
    }
}
/// Lists only the helper's reserved module namespace, without mutating any module.
pub fn remaining_test_modules() -> Result<Vec<u32>, BackendError> {
    let config = Config::from_env()?;
    let connection = config.client()?;
    raw::wait(connection.client.list_modules()).map(|modules| {
        modules
            .into_iter()
            .filter(|module| {
                module.name == c"module-null-sink"
                    && module.argument.as_deref().is_some_and(|argument| {
                        argument
                            .to_string_lossy()
                            .contains("sink_name=bifrost_test_")
                    })
            })
            .map(|module| module.index)
            .collect()
    })
}

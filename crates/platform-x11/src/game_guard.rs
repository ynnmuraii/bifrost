//! EWMH observations only. Never probe grabs by acquiring one: that changes input.
use x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ConnectionExt, Window},
    rust_connection::RustConnection,
};

pub struct Detector {
    connection: RustConnection,
    root: Window,
    active: u32,
    state: u32,
    fullscreen: u32,
}
impl Detector {
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (connection, screen) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen].root;
        let active = connection
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        let state = connection
            .intern_atom(false, b"_NET_WM_STATE")?
            .reply()?
            .atom;
        let fullscreen = connection
            .intern_atom(false, b"_NET_WM_STATE_FULLSCREEN")?
            .reply()?
            .atom;
        Ok(Self {
            connection,
            root,
            active,
            state,
            fullscreen,
        })
    }
    pub fn observe(&self) -> Result<(bool, String), Box<dyn std::error::Error + Send + Sync>> {
        let active = self
            .connection
            .get_property(false, self.root, self.active, AtomEnum::WINDOW, 0, 1)?
            .reply()?;
        let Some(window) = active
            .value32()
            .and_then(|mut values| values.next())
            .filter(|window| *window != 0)
        else {
            return Ok((false, String::new()));
        };
        let state = self
            .connection
            .get_property(false, window, self.state, AtomEnum::ATOM, 0, 128)?
            .reply()?;
        let fullscreen = state
            .value32()
            .is_some_and(|mut values| values.any(|atom| atom == self.fullscreen));
        let class = self
            .connection
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 128)?
            .reply()?;
        let app = class
            .value
            .split(|byte| *byte == 0)
            .rfind(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .unwrap_or_default();
        if app.eq_ignore_ascii_case("bifrost") {
            return Ok((false, String::new()));
        }
        Ok((fullscreen, app))
    }
}

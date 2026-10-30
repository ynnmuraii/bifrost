//! Source-local foreground observations, independent of capture and network-free.
use protocol::ipc::GameGuardSignals;

pub struct Detector {
    enabled: bool,
    #[cfg(target_os = "linux")]
    x11: Option<platform_x11::game_guard::Detector>,
}
impl Detector {
    pub fn new(platform: &str, dry: bool) -> Self {
        Self {
            enabled: !dry && platform != "platform-wayland",
            #[cfg(target_os = "linux")]
            x11: (!dry && platform == "platform-x11")
                .then(|| platform_x11::game_guard::Detector::new().ok())
                .flatten(),
        }
    }
    pub fn observe(&self) -> GameGuardSignals {
        if !self.enabled {
            return GameGuardSignals::default();
        }
        #[cfg(windows)]
        if let Ok(observation) = platform_win::game_guard::observe() {
            return GameGuardSignals {
                available: true,
                fullscreen: observation.fullscreen,
                cursor_confined: observation.cursor_confined,
                presentation: observation.presentation,
                app: config::game_guard::sanitize_app(&observation.app).unwrap_or_default(),
            };
        }
        #[cfg(target_os = "linux")]
        if let Some(detector) = &self.x11 {
            if let Ok((fullscreen, app)) = detector.observe() {
                return GameGuardSignals {
                    available: true,
                    fullscreen,
                    app: config::game_guard::sanitize_app(&app).unwrap_or_default(),
                    ..Default::default()
                };
            }
        }
        GameGuardSignals::default()
    }
}

use audio_backend::{MediaCommands, MediaInfo, MediaSessionToken, PlaybackState};
use std::{collections::HashMap, time::Instant};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
pub(super) type Properties = HashMap<String, OwnedValue>;
#[derive(Default)]
struct Controls {
    control: bool,
    play: bool,
    pause: bool,
    next: bool,
    previous: bool,
}
pub(super) struct Player {
    pub owner: String,
    pub token: MediaSessionToken,
    pub revision: u64,
    pub playing_order: u64,
    pub next_poll: Instant,
    desktop: String,
    identity: String,
    title: String,
    subtitle: String,
    track: Option<String>,
    duration: Option<u64>,
    pub state: PlaybackState,
    rate: f64,
    position: Option<u64>,
    position_at: Instant,
    controls: Controls,
}
fn replace<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        false
    } else {
        *slot = value;
        true
    }
}
pub(super) fn truncate(mut text: String, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}
fn artists(values: Vec<String>) -> String {
    let mut output = String::with_capacity(256);
    for value in values {
        if !output.is_empty() {
            append(&mut output, ", ");
        }
        append(&mut output, &value);
        if output.len() == 256 {
            break;
        }
    }
    output
}
fn append(output: &mut String, text: &str) {
    let mut end = text.len().min(256 - output.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&text[..end]);
}
fn milliseconds(value: i64) -> Option<u64> {
    u64::try_from(value).ok().map(|value| value / 1000)
}
impl Player {
    pub fn new(owner: String, token: MediaSessionToken, now: Instant) -> Self {
        Self {
            owner,
            token,
            revision: 1,
            playing_order: 0,
            next_poll: now,
            desktop: String::new(),
            identity: String::new(),
            title: String::new(),
            subtitle: String::new(),
            track: None,
            duration: None,
            state: PlaybackState::Unknown,
            rate: 1.0,
            position: None,
            position_at: now,
            controls: Controls::default(),
        }
    }
    pub fn position(&self, now: Instant) -> Option<u64> {
        self.position.map(|position| {
            let delta = if self.state == PlaybackState::Playing {
                now.saturating_duration_since(self.position_at)
                    .as_secs_f64()
                    * 1000.0
                    * self.rate
            } else {
                0.0
            };
            let position = (position as f64 + delta).max(0.0) as u64;
            self.duration
                .map_or(position, |duration| position.min(duration))
        })
    }
    pub fn commands(&self) -> MediaCommands {
        let control = self.controls.control;
        MediaCommands {
            play: control && self.controls.play,
            pause: control && self.controls.pause,
            toggle: control && self.controls.pause,
            next: control && self.controls.next,
            previous: control && self.controls.previous,
        }
    }
    pub fn info(&self, now: Instant) -> MediaInfo {
        let mut info = MediaInfo::new(
            if self.desktop.is_empty() {
                self.identity.clone()
            } else {
                self.desktop.clone()
            },
            self.title.clone(),
            self.subtitle.clone(),
            self.token,
            self.revision,
            self.state,
            self.commands(),
        )
        .expect("metadata bounded at ingestion");
        info.position_ms = self.position(now);
        info.duration_ms = self.duration;
        info
    }
    pub fn apply(&mut self, interface: &str, mut properties: Properties, now: Instant) -> bool {
        let mut changed = false;
        if interface == super::ROOT {
            if let Some(value) = properties.remove("DesktopEntry") {
                changed |= replace(
                    &mut self.desktop,
                    truncate(String::try_from(value).unwrap_or_default(), 128),
                );
            }
            if let Some(value) = properties.remove("Identity") {
                changed |= replace(
                    &mut self.identity,
                    truncate(String::try_from(value).unwrap_or_default(), 128),
                );
            }
        } else if interface == super::PLAYER {
            let estimated = self.position(now);
            if let Some(value) = properties.remove("PlaybackStatus") {
                let value = String::try_from(value).unwrap_or_default();
                let state = match value.as_str() {
                    "Playing" => PlaybackState::Playing,
                    "Paused" => PlaybackState::Paused,
                    "Stopped" => PlaybackState::Stopped,
                    _ => PlaybackState::Unknown,
                };
                if replace(&mut self.state, state) {
                    self.position = estimated;
                    self.position_at = now;
                    changed = true;
                }
            }
            if let Some(value) = properties.remove("Rate") {
                let rate = f64::try_from(value)
                    .ok()
                    .filter(|rate| rate.is_finite())
                    .unwrap_or(0.0);
                if replace(&mut self.rate, rate) {
                    self.position = estimated;
                    self.position_at = now;
                    changed = true;
                }
            }
            for (name, slot) in [
                ("CanControl", &mut self.controls.control),
                ("CanPlay", &mut self.controls.play),
                ("CanPause", &mut self.controls.pause),
                ("CanGoNext", &mut self.controls.next),
                ("CanGoPrevious", &mut self.controls.previous),
            ] {
                if let Some(value) = properties.remove(name) {
                    changed |= replace(slot, bool::try_from(value).unwrap_or(false));
                }
            }
            if let Some(value) = properties.remove("Metadata") {
                let mut metadata = Properties::try_from(value).unwrap_or_default();
                changed |= replace(
                    &mut self.title,
                    truncate(
                        metadata
                            .remove("xesam:title")
                            .and_then(|value| String::try_from(value).ok())
                            .unwrap_or_default(),
                        256,
                    ),
                );
                changed |= replace(
                    &mut self.subtitle,
                    artists(
                        metadata
                            .remove("xesam:artist")
                            .and_then(|value| Vec::<String>::try_from(value).ok())
                            .unwrap_or_default(),
                    ),
                );
                changed |= replace(
                    &mut self.duration,
                    metadata
                        .remove("mpris:length")
                        .and_then(|value| i64::try_from(value).ok())
                        .and_then(milliseconds),
                );
                let track = metadata
                    .remove("mpris:trackid")
                    .and_then(|value| OwnedObjectPath::try_from(value).ok())
                    .filter(|track| track.as_str().len() <= 1024)
                    .map(|track| track.as_str().to_owned());
                // Oversized/missing track identity is uncertain: discard, never extrapolate a prior track.
                let uncertain = track.is_none();
                if replace(&mut self.track, track) || uncertain {
                    self.position = None;
                    self.position_at = now;
                    changed = true;
                }
            }
            if let Some(value) = properties.remove("Position") {
                changed |=
                    self.correct_position(i64::try_from(value).ok().and_then(milliseconds), now);
            }
        }
        changed
    }
    pub fn correct_position(&mut self, position: Option<u64>, now: Instant) -> bool {
        let changed = self.position(now) != position;
        self.position = position;
        self.position_at = now;
        changed
    }
    pub fn seek(&mut self, microseconds: i64, now: Instant) -> bool {
        self.correct_position(milliseconds(microseconds), now)
    }
    pub fn advance_revision(&mut self) -> bool {
        if let Some(next) = self.revision.checked_add(1) {
            self.revision = next;
            true
        } else {
            false
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn text(value: &str) -> OwnedValue {
        OwnedValue::try_from(zbus::zvariant::Value::from(value)).unwrap()
    }
    #[test]
    fn utf8_metadata_commands_and_unknown() {
        let now = Instant::now();
        let mut player = Player::new(":test".into(), MediaSessionToken(1), now);
        let mut root = Properties::new();
        root.insert("Identity".into(), text(&"界".repeat(100)));
        player.apply(super::super::ROOT, root, now);
        let mut metadata = Properties::new();
        metadata.insert("xesam:title".into(), text(&"🎵".repeat(100)));
        metadata.insert(
            "xesam:artist".into(),
            OwnedValue::try_from(zbus::zvariant::Value::from(vec!["One", "Two"])).unwrap(),
        );
        metadata.insert("mpris:length".into(), OwnedValue::from(1_234_567i64));
        let mut properties = Properties::new();
        properties.insert("Metadata".into(), metadata.into());
        properties.insert("CanControl".into(), true.into());
        properties.insert("CanPlay".into(), true.into());
        properties.insert("CanPause".into(), false.into());
        properties.insert("PlaybackStatus".into(), text("Alien"));
        player.apply(super::super::PLAYER, properties, now);
        let info = player.info(now);
        assert_eq!(info.app().len(), 126);
        assert_eq!(info.title().len(), 256);
        assert_eq!(info.subtitle(), "One, Two");
        assert_eq!(info.duration_ms, Some(1234));
        assert_eq!(info.state, PlaybackState::Unknown);
        assert!(info.commands.play);
        assert!(!info.commands.toggle);
    }
    #[test]
    fn timeline_rate_pause_seek_and_bounds() {
        let now = Instant::now();
        let mut player = Player::new(":test".into(), MediaSessionToken(1), now);
        player.state = PlaybackState::Playing;
        player.rate = 1.25;
        player.seek(2_000_000, now);
        assert_eq!(player.position(now + Duration::from_secs(2)), Some(4500));
        let mut properties = Properties::new();
        properties.insert("PlaybackStatus".into(), text("Paused"));
        player.apply(
            super::super::PLAYER,
            properties,
            now + Duration::from_secs(2),
        );
        assert_eq!(player.position(now + Duration::from_secs(20)), Some(4500));
        player.state = PlaybackState::Playing;
        player.rate = -2.0;
        player.seek(1000, now);
        assert_eq!(player.position(now + Duration::from_secs(1)), Some(0));
        player.rate = 2.0;
        player.duration = Some(3000);
        player.seek(2_000_000, now);
        assert_eq!(player.position(now + Duration::from_secs(10)), Some(3000));
        player.seek(-1, now);
        assert_eq!(player.position(now), None);
    }
}

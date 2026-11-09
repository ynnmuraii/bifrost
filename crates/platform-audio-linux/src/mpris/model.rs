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
    seek: bool,
}
pub(super) struct Player {
    pub owner: String,
    pub token: MediaSessionToken,
    pub revision: u64,
    pub playing_order: u64,
    pub next_poll: Instant,
    artwork_pending: bool,
    previous_artwork: Option<audio_backend::artwork::Artwork>,
    desktop: String,
    identity: String,
    title: String,
    subtitle: String,
    artwork_url: Option<String>,
    artwork: Option<audio_backend::artwork::Artwork>,
    track: Option<String>,
    duration: Option<u64>,
    pub position_refresh: bool,
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
            artwork_url: None,
            artwork: None,
            artwork_pending: false,
            previous_artwork: None,
            track: None,
            duration: None,
            state: PlaybackState::Unknown,
            rate: 1.0,
            position: None,
            position_at: now,
            position_refresh: false,
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
            seek: control
                && self.controls.seek
                && matches!((self.position, self.duration),
                    (Some(position), Some(duration)) if duration > 0 && position <= duration)
                && self.track.is_some(),
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
        info.artwork = self.artwork.clone();
        if info.commands.seek {
            info.seek_min_ms = Some(0);
            info.seek_max_ms = self.duration;
        }
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
                ("CanSeek", &mut self.controls.seek),
            ] {
                if let Some(value) = properties.remove(name) {
                    changed |= replace(slot, bool::try_from(value).unwrap_or(false));
                }
            }
            if let Some(value) = properties.remove("Metadata") {
                let mut metadata = Properties::try_from(value).unwrap_or_default();
                let old_title = self.title.clone();
                let old_track = self.track.clone();
                let artwork_url = metadata
                    .remove("mpris:artUrl")
                    .and_then(|value| String::try_from(value).ok())
                    .filter(|url| url.len() <= 2048);
                let artwork_changed = replace(&mut self.artwork_url, artwork_url);
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
                    .filter(|track| {
                        track.as_str().len() <= 1024
                            && track.as_str() != "/"
                            && track.as_str() != "/org/mpris/MediaPlayer2/TrackList/NoTrack"
                    })
                    .map(|track| track.as_str().to_owned());
                // Oversized/missing track identity is uncertain: discard, never extrapolate a prior track.
                let uncertain = track.is_none();
                if replace(&mut self.track, track) || uncertain {
                    self.position = None;
                    self.position_at = now;
                    self.position_refresh = true;
                    self.next_poll = now;
                    changed = true;
                }
                if artwork_changed || old_title != self.title || old_track != self.track {
                    self.previous_artwork = self.artwork.take();
                    self.artwork_pending = true;
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
        self.position_refresh = false;
        self.position = position;
        self.position_at = now;
        changed
    }
    pub fn seek(&mut self, microseconds: i64, now: Instant) -> bool {
        self.correct_position(milliseconds(microseconds), now)
    }
    pub fn seek_position(&self, position_ms: u64) -> Result<i64, audio_backend::BackendError> {
        use audio_backend::BackendError;
        if !self.commands().seek {
            return Err(BackendError::Unsupported);
        }
        if !self
            .duration
            .is_some_and(|duration| position_ms <= duration)
        {
            return Err(BackendError::InvalidArgument);
        }
        position_ms
            .checked_mul(1000)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(BackendError::InvalidArgument)
    }
    pub fn seek_target(
        &self,
        position_ms: u64,
    ) -> Result<(OwnedObjectPath, i64), audio_backend::BackendError> {
        use audio_backend::BackendError;
        let position = self.seek_position(position_ms)?;
        let track =
            OwnedObjectPath::try_from(self.track.as_deref().ok_or(BackendError::Unsupported)?)
                .map_err(|_| BackendError::Unsupported)?;
        Ok((track, position))
    }
    pub fn take_artwork(&mut self) -> Option<ArtworkRequest> {
        if !std::mem::take(&mut self.artwork_pending) {
            return None;
        }
        Some(ArtworkRequest {
            owner: self.owner.clone(),
            token: self.token,
            revision: self.revision,
            track: self.track.clone(),
            url: self.artwork_url.clone(),
            previous: self.previous_artwork.take(),
        })
    }
    pub fn publish_artwork(
        &mut self,
        request: &ArtworkRequest,
        artwork: Option<audio_backend::artwork::Artwork>,
    ) -> bool {
        if self.owner != request.owner
            || self.token != request.token
            || self.revision != request.revision
            || self.track != request.track
            || self.artwork_url != request.url
        {
            return false;
        }
        replace(&mut self.artwork, artwork)
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
pub(super) struct ArtworkRequest {
    owner: String,
    token: MediaSessionToken,
    revision: u64,
    track: Option<String>,
    url: Option<String>,
    previous: Option<audio_backend::artwork::Artwork>,
}
impl ArtworkRequest {
    pub fn load(&self) -> Option<audio_backend::artwork::Artwork> {
        self.url
            .as_deref()
            .and_then(|url| cached_artwork(url, self.previous.as_ref()))
    }
}
fn cached_artwork(
    url: &str,
    previous: Option<&audio_backend::artwork::Artwork>,
) -> Option<audio_backend::artwork::Artwork> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })?;
    cached_artwork_in(&cache, url, previous)
}
fn cached_artwork_in(
    cache: &std::path::Path,
    url: &str,
    previous: Option<&audio_backend::artwork::Artwork>,
) -> Option<audio_backend::artwork::Artwork> {
    use audio_backend::artwork::{normalize, MAX_NATIVE_BYTES};
    use std::{io::Read, os::unix::fs::MetadataExt, path::Path};
    let location = url.strip_prefix("file://")?;
    // Percent-encoded/remote/query locations are unsupported, never decoded or fetched.
    if !location.starts_with('/') || location.contains(['%', '?', '#', '\0']) {
        return None;
    }
    let path = Path::new(location);
    if !matches!(
        path.extension()?.to_str()?.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg"
    ) {
        return None;
    }
    // Hold the trust root and resolve every child relative to that fd. NOFOLLOW
    // on every directory prevents an intermediate rename-to-symlink escape.
    let root = std::fs::File::from(
        rustix::fs::open(
            cache,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .ok()?,
    );
    let relative = path.strip_prefix(cache).ok()?;
    let fd = confined_file(&root, relative)?;
    let file = std::fs::File::from(fd);
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != root.metadata().ok()?.uid()
        || metadata.len() > MAX_NATIVE_BYTES as u64
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_NATIVE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    normalize(&bytes, previous)
}
fn confined_file(root: &std::fs::File, relative: &std::path::Path) -> Option<std::os::fd::OwnedFd> {
    use rustix::fs::{openat, Mode, OFlags};
    use std::path::Component;
    let mut directory = None;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return None;
        };
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let flags = if components.peek().is_some() {
            flags | OFlags::DIRECTORY
        } else {
            flags
        };
        let fd = if let Some(parent) = &directory {
            openat(parent, name, flags, Mode::empty())
        } else {
            openat(root, name, flags, Mode::empty())
        }
        .ok()?;
        if components.peek().is_none() {
            return Some(fd);
        }
        directory = Some(fd);
    }
    None
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
    fn artwork_reads_only_bounded_regular_images_inside_owned_cache() {
        let root = std::env::temp_dir().join(format!(
            "bf-art-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let cache = root.join("cache");
        std::fs::create_dir(&cache).unwrap();
        let image = cache.join("cover.png");
        std::fs::write(
            &image,
            [
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0,
                2, 8, 2, 0, 0, 0, 253, 212, 154, 115, 0, 0, 0, 22, 73, 68, 65, 84, 120, 156, 99,
                80, 241, 152, 224, 49, 65, 133, 97, 130, 138, 135, 202, 4, 15, 0, 27, 122, 3, 241,
                244, 33, 109, 130, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
            ],
        )
        .unwrap();
        let url = format!("file://{}", image.display());
        assert!(cached_artwork_in(&cache, &url, None).is_some());
        assert!(cached_artwork_in(&cache, "https://example.invalid/private.jpg", None).is_none());
        let outside = root.join("private.jpg");
        std::fs::write(&outside, b"private bytes").unwrap();
        assert!(
            cached_artwork_in(&cache, &format!("file://{}", outside.display()), None).is_none()
        );
        let symlink = cache.join("alias.jpg");
        std::os::unix::fs::symlink(&outside, &symlink).unwrap();
        assert!(
            cached_artwork_in(&cache, &format!("file://{}", symlink.display()), None).is_none()
        );
        let album = cache.join("album");
        std::fs::create_dir(&album).unwrap();
        std::fs::copy(&image, album.join("cover.png")).unwrap();
        let outside_dir = root.join("outside");
        std::fs::create_dir(&outside_dir).unwrap();
        std::fs::copy(&image, outside_dir.join("cover.png")).unwrap();
        let cache_fd = std::fs::File::open(&cache).unwrap();
        // Swap an intermediate directory after pinning the root, before resolution.
        let moved = cache.join("moved");
        std::fs::rename(&album, &moved).unwrap();
        std::os::unix::fs::symlink(&outside_dir, &album).unwrap();
        assert!(confined_file(&cache_fd, std::path::Path::new("album/cover.png")).is_none());
        assert!(cached_artwork_in(
            &cache,
            &format!("file://{}", album.join("cover.png").display()),
            None,
        )
        .is_none());
        assert!(confined_file(&cache_fd, std::path::Path::new("../outside/cover.png")).is_none());
        assert!(confined_file(&cache_fd, std::path::Path::new("moved/cover.png")).is_some());
        std::fs::remove_file(album).unwrap();
        std::fs::remove_dir_all(moved).unwrap();
        std::fs::remove_dir_all(outside_dir).unwrap();
        std::fs::write(
            &image,
            vec![0; audio_backend::artwork::MAX_NATIVE_BYTES + 1],
        )
        .unwrap();
        assert!(cached_artwork_in(&cache, &url, None).is_none());
        std::fs::remove_file(symlink).unwrap();
        std::fs::remove_file(outside).unwrap();
        std::fs::remove_file(image).unwrap();
        std::fs::remove_dir(cache).unwrap();
        std::fs::remove_dir(root).unwrap();
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
    #[test]
    fn seek_requires_private_track_and_true_native_timeline_and_checked_units() {
        use audio_backend::BackendError;
        let now = Instant::now();
        let mut player = Player::new(":owned".into(), MediaSessionToken(1), now);
        player.controls.control = true;
        player.controls.seek = true;
        player.duration = Some(600_000);
        player.position = Some(20);
        assert!(!player.commands().seek);
        player.track = Some("/owned/track".into());
        assert!(player.commands().seek);
        assert_eq!(player.info(now).seek_range(), Some((0, 600_000)));
        let (track, position) = player.seek_target(123_456).unwrap();
        assert_eq!(track.as_str(), "/owned/track");
        assert_eq!(position, 123_456_000);
        assert_eq!(
            player.seek_target(600_001),
            Err(BackendError::InvalidArgument)
        );
        player.position = None;
        assert!(!player.commands().seek);
        player.position = Some(0);
        player.duration = Some(0);
        assert!(!player.commands().seek);
        player.duration = Some(u64::MAX);
        assert_eq!(
            player.seek_target(u64::MAX),
            Err(BackendError::InvalidArgument)
        );
        player.controls.control = false;
        assert_eq!(player.seek_target(0), Err(BackendError::Unsupported));
    }
}

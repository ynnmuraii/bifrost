//! Audio preferences contain no native handles and never grant trust by themselves.
//! Turn B maps `media_port` to GUI `udp_port`, `allow_send_to` to peer
//! `allow_system_send`, and `allow_system_audio_from` to `allow_system_receive`.
//! GUI peer records are the union of these lists and `peers`; absent permissions,
//! including all deferred microphone directions, stay false. `gain_percent` maps
//! to GUI `volume_percent`; `muted` is Listener-local state, not peer consent.
//! `auto` profile must be resolved before emitting a concrete IPC profile.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ConfigError;

/// Fixed-size identity, serialized as exactly twice N hexadecimal digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HexId<const N: usize>(pub [u8; N]);

pub type AudioNodeId = HexId<32>;
pub type AudioGroupId = HexId<16>;

impl<const N: usize> fmt::Display for HexId<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl<const N: usize> Serialize for HexId<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de, const N: usize> Deserialize<'de> for HexId<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() != N * 2 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom(format!(
                "expected {} hexadecimal digits",
                N * 2
            )));
        }
        let mut bytes = [0; N];
        for (out, pair) in bytes.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
            let digit = |b: u8| {
                if b <= b'9' {
                    b - b'0'
                } else {
                    b.to_ascii_lowercase() - b'a' + 10
                }
            };
            *out = digit(pair[0]) * 16 + digit(pair[1]);
        }
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioRole {
    #[default]
    Off,
    Source,
    Listener,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioProfile {
    #[default]
    Auto,
    Ethernet,
    Wifi,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    #[default]
    MixAll,
    FollowFocus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultDeviceRole {
    #[default]
    Multimedia,
    Communications,
}

/// Selection follows a default endpoint, never changes the OS default endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AudioDeviceSelector {
    StableId { id: String },
    FollowDefault { role: DefaultDeviceRole },
}

impl Default for AudioDeviceSelector {
    fn default() -> Self {
        Self::FollowDefault {
            role: DefaultDeviceRole::Multimedia,
        }
    }
}

/// Semantic default system monitor; not a microphone or an arbitrary output route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SystemCaptureSelector {
    DefaultMonitor {},
    StableId { id: String },
}

impl Default for SystemCaptureSelector {
    fn default() -> Self {
        Self::DefaultMonitor {}
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub enabled: bool,
    /// None preserves legacy allowlists; false disables only system audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_sharing: Option<bool>,
    pub role: AudioRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<AudioGroupId>,
    /// Absent legacy Source groups pin; explicit false follows the Listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id_override: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listener_node_id: Option<AudioNodeId>,
    pub media_port: u16,
    pub profile: AudioProfile,
    pub mode: AudioMode,
    pub allow_system_audio_from: Vec<AudioNodeId>,
    pub allow_send_to: Vec<AudioNodeId>,
    pub peers: Vec<AudioPeerConfig>,
    pub output_device: AudioDeviceSelector,
    pub output_fallback_to_default: bool,
    pub capture_device: SystemCaptureSelector,
    pub resume_system_audio: bool,
    #[serde(deserialize_with = "debounce")]
    pub focus_debounce_ms: u32,
    #[serde(deserialize_with = "ramp")]
    pub focus_ramp_ms: u32,
    #[serde(deserialize_with = "background_gain")]
    pub background_gain_db: f32,
    pub background_mute: bool,
    pub microphone: AudioMicrophoneConfig,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            system_sharing: None,
            role: AudioRole::Off,
            group_id: None,
            group_id_override: Some(false),
            listener_node_id: None,
            media_port: 47903,
            profile: AudioProfile::Auto,
            mode: AudioMode::MixAll,
            allow_system_audio_from: Vec::new(),
            allow_send_to: Vec::new(),
            peers: Vec::new(),
            output_device: AudioDeviceSelector::default(),
            output_fallback_to_default: false,
            capture_device: SystemCaptureSelector::default(),
            resume_system_audio: false,
            focus_debounce_ms: 250,
            focus_ramp_ms: 150,
            background_gain_db: -18.0,
            background_mute: false,
            microphone: AudioMicrophoneConfig::default(),
        }
    }
}

/// Listener-local gain/mute; media control is an independent Source permission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioPeerConfig {
    pub node_id: AudioNodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_send_override: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_receive_override: Option<bool>,
    #[serde(default)]
    pub allow_mic_control: bool,
    #[serde(default)]
    pub allow_media_control: bool,
    #[serde(default = "full_gain")]
    pub gain_percent: u32,
    #[serde(default)]
    pub muted: bool,
}

fn full_gain() -> u32 {
    100
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioMicrophoneConfig {
    pub control_enabled: bool,
    pub streaming_enabled: bool,
    pub experimental_acknowledged: bool,
    pub capture_device: AudioDeviceSelector,
    pub allow_default_mic_changes: bool,
    pub virtual_device: String,
    pub muted: bool,
    pub ptt_enabled: bool,
    pub ptt_hotkey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_target_node_id: Option<AudioNodeId>,
}

impl Default for AudioMicrophoneConfig {
    fn default() -> Self {
        Self {
            control_enabled: false,
            streaming_enabled: false,
            experimental_acknowledged: false,
            capture_device: AudioDeviceSelector::default(),
            allow_default_mic_changes: false,
            virtual_device: String::new(),
            muted: true,
            ptt_enabled: false,
            ptt_hotkey: String::new(),
            pinned_target_node_id: None,
        }
    }
}

fn invalid(key: &'static str, expected: &str) -> ConfigError {
    ConfigError::invalid(key, "invalid audio setting", expected)
}

fn text(value: &str, key: &'static str, empty: bool) -> Result<(), ConfigError> {
    if (!empty && value.is_empty()) || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(invalid(
            key,
            "at most 256 bytes, no control characters; nonempty device IDs",
        ));
    }
    Ok(())
}

impl AudioDeviceSelector {
    fn validate(&self, key: &'static str) -> Result<(), ConfigError> {
        if let Self::StableId { id } = self {
            text(id, key, false)?;
        }
        Ok(())
    }
}

impl AudioConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn effective_group_override(&self) -> bool {
        self.role == AudioRole::Source && self.group_id_override.unwrap_or(self.group_id.is_some())
    }

    /// System policy master gate, independent of microphone and media controls.
    /// This is configured intent, not native capture or playback activity.
    pub fn system_enabled(&self) -> bool {
        self.enabled && self.role != AudioRole::Off && self.system_sharing != Some(false)
    }

    /// Pure local policy. The caller MUST check current trust, authenticated
    /// Listener identity/group, and bilateral consent before sending.
    pub fn system_send_allowed(&self, node_id: AudioNodeId) -> bool {
        self.system_enabled()
            && self.role == AudioRole::Source
            && self.listener_node_id == Some(node_id)
            && self
                .peers
                .iter()
                .find(|peer| peer.node_id == node_id)
                .and_then(|peer| peer.system_send_override)
                .unwrap_or_else(|| {
                    self.system_sharing == Some(true) || self.allow_send_to.contains(&node_id)
                })
    }

    /// Pure local policy. The caller MUST check current trust, membership in
    /// this Listener's own authenticated group, and bilateral consent.
    pub fn system_receive_allowed(&self, node_id: AudioNodeId) -> bool {
        self.system_enabled()
            && self.role == AudioRole::Listener
            && self
                .peers
                .iter()
                .find(|peer| peer.node_id == node_id)
                .and_then(|peer| peer.system_receive_override)
                .unwrap_or_else(|| {
                    self.system_sharing == Some(true)
                        || self.allow_system_audio_from.contains(&node_id)
                })
    }

    /// Structural policy only. The caller MUST recheck current trust, self identity,
    /// Listener group ownership, runtime device availability and bilateral consent
    /// at activation. Pairing never populates either explicit consent list.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.group_id_override == Some(true)
            && (self.role != AudioRole::Source || self.group_id.is_none())
        {
            return Err(invalid(
                "audio.group_id_override",
                "Source role with a nonempty group_id",
            ));
        }
        if self.media_port == 0 || self.media_port == 47902 {
            return Err(invalid(
                "audio.media_port",
                "1..=65535 except discovery UDP 47902",
            ));
        }
        for (key, ids) in [
            ("audio.allow_send_to", &self.allow_send_to),
            (
                "audio.allow_system_audio_from",
                &self.allow_system_audio_from,
            ),
        ] {
            if ids.len() > 3 || ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
                return Err(invalid(key, "at most three unique node IDs"));
            }
        }
        let mut peers = BTreeSet::new();
        if self.peers.len() > 3 {
            return Err(invalid("audio.peers", "at most three unique peers"));
        }
        for peer in &self.peers {
            if !peers.insert(peer.node_id) {
                return Err(invalid("audio.peers", "unique node IDs"));
            }
            if peer.gain_percent > 100 {
                return Err(invalid("audio.peers.gain_percent", "integer 0..=100"));
            }
            if peer.system_send_override == Some(true)
                && (self.role != AudioRole::Source || Some(peer.node_id) != self.listener_node_id)
            {
                return Err(invalid(
                    "audio.peers.system_send_override",
                    "Source permission for selected Listener only",
                ));
            }
            if peer.system_receive_override == Some(true) && self.role != AudioRole::Listener {
                return Err(invalid(
                    "audio.peers.system_receive_override",
                    "Listener permission for a configured Source only",
                ));
            }
            // Modern SYSTEM target changes retain independent, inactive media
            // preferences. Execution still requires the selected live session.
            // Legacy manual configurations keep their selected-peer restriction.
            if peer.allow_media_control
                && (self.role != AudioRole::Source
                    || (self.system_sharing.is_none()
                        && Some(peer.node_id) != self.listener_node_id))
            {
                return Err(invalid(
                    "audio.peers.allow_media_control",
                    "Source permission for selected Listener only",
                ));
            }
            if peer.allow_mic_control && self.role != AudioRole::Listener {
                return Err(invalid(
                    "audio.peers.allow_mic_control",
                    "Listener permission for a configured Source only",
                ));
            }
            if self.role != AudioRole::Listener && (peer.gain_percent != 100 || peer.muted) {
                return Err(invalid("audio.peers", "gain/mute are Listener-local"));
            }
        }
        let all_peers = self
            .allow_send_to
            .iter()
            .chain(&self.allow_system_audio_from)
            .copied()
            .chain(self.peers.iter().map(|peer| peer.node_id))
            .collect::<BTreeSet<_>>();
        if all_peers.len() > 3 {
            return Err(invalid("audio", "at most three distinct configured peers"));
        }
        match self.role {
            AudioRole::Off => {
                if self.enabled
                    || self.system_sharing == Some(true)
                    || !self.allow_send_to.is_empty()
                    || !self.allow_system_audio_from.is_empty()
                    || self.microphone.control_enabled
                {
                    return Err(invalid(
                        "audio.role",
                        "off cannot enable audio or permissions",
                    ));
                }
            }
            AudioRole::Source => {
                if !self.allow_system_audio_from.is_empty()
                    || self
                        .allow_send_to
                        .iter()
                        .any(|id| Some(*id) != self.listener_node_id)
                {
                    return Err(invalid(
                        "audio.allow_send_to",
                        "Source sends only to its selected Listener and never receives",
                    ));
                }
                // Enabled is desired intent; activation still waits for authenticated group resolution.
                if self.enabled && self.listener_node_id.is_none() {
                    return Err(invalid("audio.enabled", "Source needs a selected Listener"));
                }
            }
            AudioRole::Listener => {
                if !self.allow_send_to.is_empty() {
                    return Err(invalid(
                        "audio.allow_send_to",
                        "Listener never sends system audio",
                    ));
                }
                // Missing group is pending explicit core minting, not permission to activate.
            }
        }
        if !(100..=2000).contains(&self.focus_debounce_ms)
            || !(20..=1000).contains(&self.focus_ramp_ms)
            || !self.background_gain_db.is_finite()
            || !(-60.0..=0.0).contains(&self.background_gain_db)
        {
            return Err(invalid(
                "audio.focus",
                "debounce 100..=2000, ramp 20..=1000, finite background gain -60..=0",
            ));
        }
        self.output_device.validate("audio.output_device")?;
        if let SystemCaptureSelector::StableId { id } = &self.capture_device {
            text(id, "audio.capture_device", false)?;
        }
        let mic = &self.microphone;
        mic.capture_device
            .validate("audio.microphone.capture_device")?;
        text(&mic.virtual_device, "audio.microphone.virtual_device", true)?;
        text(&mic.ptt_hotkey, "audio.microphone.ptt_hotkey", true)?;
        if mic.streaming_enabled {
            return Err(invalid(
                "audio.microphone.streaming_enabled",
                "false: microphone streaming is deferred",
            ));
        }
        if mic.ptt_enabled && !mic.control_enabled {
            return Err(invalid(
                "audio.microphone.ptt_enabled",
                "microphone control enabled (empty hotkey permits focused GUI PTT)",
            ));
        }
        if !mic.ptt_hotkey.is_empty() && crate::Hotkey::parse(&mic.ptt_hotkey).is_err() {
            return Err(invalid("audio.microphone.ptt_hotkey", "valid hotkey"));
        }
        if mic.pinned_target_node_id.is_some() && self.role != AudioRole::Listener {
            return Err(invalid(
                "audio.microphone.pinned_target_node_id",
                "only Listener selects a microphone target",
            ));
        }
        Ok(())
    }

    /// Optional activation preflight; live authorization still belongs to core.
    pub fn validate_identity(
        &self,
        local: AudioNodeId,
        trusted: &BTreeSet<AudioNodeId>,
    ) -> Result<(), ConfigError> {
        self.validate()?;
        let selected = self
            .allow_send_to
            .iter()
            .copied()
            .chain(self.allow_system_audio_from.iter().copied())
            .chain(self.peers.iter().map(|peer| peer.node_id))
            .chain(
                self.listener_node_id
                    .filter(|_| self.role != AudioRole::Listener),
            );
        for id in selected {
            if id == local || !trusted.contains(&id) {
                return Err(invalid(
                    "audio",
                    "other node IDs must be currently trusted and not self",
                ));
            }
        }
        if self
            .microphone
            .pinned_target_node_id
            .is_some_and(|id| id != local)
        {
            return Err(invalid(
                "audio.microphone.pinned_target_node_id",
                "only the Listener's own physical microphone is controlled",
            ));
        }
        if self.role == AudioRole::Listener && self.listener_node_id.is_some_and(|id| id != local) {
            return Err(invalid(
                "audio.listener_node_id",
                "Listener must select self or omit listener ID",
            ));
        }
        Ok(())
    }
}

fn debounce<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let value = u32::deserialize(d)?;
    let clamped = value.clamp(100, 2000);
    if value != clamped {
        tracing::warn!("audio.focus_debounce_ms clamped to 100..=2000");
    }
    Ok(clamped)
}

fn ramp<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let value = u32::deserialize(d)?;
    let clamped = value.clamp(20, 1000);
    if value != clamped {
        tracing::warn!("audio.focus_ramp_ms clamped to 20..=1000");
    }
    Ok(clamped)
}

fn background_gain<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    let value = f32::deserialize(d)?;
    if !value.is_finite() {
        return Err(serde::de::Error::custom("background gain must be finite"));
    }
    let clamped = value.clamp(-60.0, 0.0);
    if value != clamped {
        tracing::warn!("audio.background_gain_db clamped to -60..=0");
    }
    Ok(clamped)
}

/// Replace only audio under the existing cross-process read/modify/write lock.
/// Persistence does not activate capture, mint group IDs, or grant trust.
pub fn save_audio_locked(path: &std::path::Path, audio: AudioConfig) -> Result<(), ConfigError> {
    audio.validate()?;
    crate::update_locked(path, |config| config.audio = audio)
}

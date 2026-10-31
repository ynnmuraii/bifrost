//! Listener-only focus multipliers. All time is supplied by the caller.

const UNKNOWN_HOLD_MS: u64 = 2000;
// Protocol N_MAX is four; keep this dependency-free model bounded to the same limit.
const MAX_MEMBERS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub node: [u8; 32],
    pub serial: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub owner: Owner,
    pub revision: u64,
    pub active: Option<[u8; 32]>,
    pub lease: Option<[u8; 16]>,
    pub committed: bool,
}

#[derive(Clone, Copy)]
struct Gain {
    node: [u8; 32],
    from: f64,
}

pub struct FocusPolicy {
    owner: Option<Owner>,
    revision: u64,
    primary: Option<[u8; 32]>,
    pending: Option<([u8; 32], u64)>,
    unknown_since: Option<u64>,
    debounce_ms: u64,
    ramp_ms: u64,
    background_percent: u32,
    gains: [Option<Gain>; MAX_MEMBERS],
    background_from: f64,
    ramp_start: u64,
    ramp_duration: u64,
    now_ms: u64,
}

impl FocusPolicy {
    pub fn new(debounce_ms: u64, ramp_ms: u64, background_percent: u32) -> Self {
        Self {
            owner: None,
            revision: 0,
            primary: None,
            pending: None,
            unknown_since: None,
            debounce_ms,
            ramp_ms,
            background_percent: background_percent.min(100),
            gains: [None; MAX_MEMBERS],
            background_from: 100.0,
            ramp_start: 0,
            ramp_duration: 0,
            now_ms: 0,
        }
    }

    pub fn set_owner(&mut self, now_ms: u64, owner: Option<Owner>) {
        self.tick(now_ms);
        let owner = owner.filter(|owner| owner.serial > 0);
        if self.owner != owner {
            self.owner = owner;
            self.revision = 0;
            self.unknown(now_ms);
        }
    }

    /// Only fresh, authenticated snapshots in the current owner namespace are accepted.
    pub fn accept(
        &mut self,
        now_ms: u64,
        author: [u8; 32],
        snapshot: Snapshot,
        members: &[[u8; 32]],
    ) -> bool {
        if self.owner != Some(snapshot.owner)
            || author != snapshot.owner.node
            || snapshot.owner.serial == 0
            || snapshot.revision == 0
            || snapshot.revision <= self.revision
            || members.len() > MAX_MEMBERS
            || !members.contains(&snapshot.owner.node)
            || members
                .iter()
                .enumerate()
                .any(|(i, node)| members[..i].contains(node))
        {
            return false;
        }
        if snapshot.committed {
            let Some(active) = snapshot.active else {
                return false;
            };
            if !members.contains(&active)
                || (active == snapshot.owner.node) != snapshot.lease.is_none()
            {
                return false;
            }
        } else if snapshot.active.is_some() || snapshot.lease.is_some() {
            return false;
        }

        self.tick(now_ms);
        self.revision = snapshot.revision;
        // Preserve each surviving member's instantaneous gain across membership changes.
        let mut gains = [None; MAX_MEMBERS];
        for (slot, node) in gains.iter_mut().zip(members) {
            *slot = Some(Gain {
                node: *node,
                from: self.value(*node),
            });
        }
        let background = self.background_value();
        let remaining = self
            .ramp_duration
            .saturating_sub(self.now_ms.saturating_sub(self.ramp_start));
        self.gains = gains;
        self.background_from = background;
        self.ramp_start = self.now_ms;
        self.ramp_duration = remaining;

        if let Some(active) = snapshot.active {
            self.unknown_since = None;
            if self.primary == Some(active) {
                self.pending = None;
            } else if self.pending.map(|(node, _)| node) != Some(active) {
                self.pending = Some((active, self.now_ms));
            }
            self.tick(now_ms);
        } else {
            self.unknown(now_ms);
        }
        true
    }

    /// Repeated unknown reports do not renew the two-second hold.
    pub fn unknown(&mut self, now_ms: u64) {
        self.tick(now_ms);
        self.pending = None;
        self.unknown_since.get_or_insert(self.now_ms);
    }

    pub fn tick(&mut self, now_ms: u64) {
        self.now_ms = self.now_ms.max(now_ms);
        if let Some(since) = self.unknown_since {
            let deadline = since.saturating_add(UNKNOWN_HOLD_MS);
            if self.now_ms >= deadline && self.primary.is_some() {
                self.retarget(deadline, None);
            }
        } else if let Some((node, since)) = self.pending {
            let deadline = since.saturating_add(self.debounce_ms);
            if self.now_ms >= deadline {
                self.pending = None;
                self.retarget(deadline, Some(node));
            }
        }
    }

    pub fn primary(&self) -> Option<[u8; 32]> {
        self.primary
    }

    /// Focus multiplier only: the caller retains source user gain and mute independently.
    pub fn gain_percent(&self, source: [u8; 32]) -> u32 {
        self.value(source).round().clamp(0.0, 100.0) as u32
    }

    pub fn configure(
        &mut self,
        now_ms: u64,
        debounce_ms: u64,
        ramp_ms: u64,
        background_percent: u32,
    ) {
        self.tick(now_ms);
        let background_percent = background_percent.min(100);
        let changed = self.background_percent != background_percent;
        if changed {
            self.capture(self.now_ms);
        }
        self.debounce_ms = debounce_ms;
        self.ramp_ms = ramp_ms;
        self.background_percent = background_percent;
        if changed {
            self.ramp_start = self.now_ms;
            self.ramp_duration = ramp_ms;
        }
        self.tick(now_ms);
    }

    fn target(&self, node: Option<[u8; 32]>) -> f64 {
        if self.primary.is_none() || self.primary == node {
            100.0
        } else {
            f64::from(self.background_percent)
        }
    }

    fn interpolate(&self, from: f64, to: f64) -> f64 {
        let elapsed = self.now_ms.saturating_sub(self.ramp_start);
        if self.ramp_duration == 0 || elapsed >= self.ramp_duration {
            to
        } else {
            from + (to - from) * (elapsed as f64 / self.ramp_duration as f64)
        }
    }

    fn background_value(&self) -> f64 {
        self.interpolate(self.background_from, self.target(None))
    }

    fn value(&self, node: [u8; 32]) -> f64 {
        let from = self
            .gains
            .iter()
            .flatten()
            .find(|gain| gain.node == node)
            .map_or(self.background_from, |gain| gain.from);
        self.interpolate(from, self.target(Some(node)))
    }

    fn capture(&mut self, at_ms: u64) {
        let now_ms = self.now_ms;
        self.now_ms = at_ms;
        let mut gains = self.gains;
        for gain in gains.iter_mut().flatten() {
            gain.from = self.value(gain.node);
        }
        self.background_from = self.background_value();
        self.gains = gains;
        self.now_ms = now_ms;
    }

    fn retarget(&mut self, at_ms: u64, primary: Option<[u8; 32]>) {
        self.capture(at_ms);
        self.primary = primary;
        self.ramp_start = at_ms;
        self.ramp_duration = self.ramp_ms;
    }
}

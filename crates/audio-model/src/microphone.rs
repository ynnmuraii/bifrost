//! Listener-clock microphone authority. Transport and hardware acknowledgments remain external.
//! Producers encode a strictly increasing nonzero counter in the low 64 bits of
//! request/press IDs, retaining it across child resets until the TLS parent changes.
use crate::session::Parent;
use std::collections::VecDeque;

pub const LEASE_NS: u64 = 250_000_000;
pub const CHALLENGE_NS: u64 = 100_000_000;
const HISTORY: usize = 64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultCode {
    Ok,
    Disabled,
    ConsentRequired,
    StaleRevision,
    Unsupported,
    DeviceGone,
    Conflict,
    Expired,
    Busy,
    Timeout,
    Cancelled,
    Invalid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    SetMute(bool),
    Press,
    Renew,
    Release,
}

/// A replacement child has no guard history. Re-establish this base state before
/// advertising control; a PTT press is never a recovery command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryMode {
    Ptt,
    Persistent(bool),
}

impl RecoveryMode {
    pub fn idle_muted(self) -> bool {
        match self {
            Self::Ptt => true,
            Self::Persistent(muted) => muted,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control {
    pub request: [u8; 16],
    pub revision: u64,
    pub op: Op,
    pub press: [u8; 16],
    pub challenge: u64,
    pub expires_ns: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    pub parent: Parent,
    pub press: [u8; 16],
    pub expires_ns: u64,
    pub id: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    SetMute(bool),
    Grant(Grant),
    Revoke(u64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub result: ResultCode,
    pub effect: Effect,
}
#[derive(Default)]
struct History {
    requests: VecDeque<([u8; 16], Decision)>,
    request_high: u64,
    press_high: u64,
}
pub struct Authority {
    pub revision: u64,
    pub muted: bool,
    pub desired_muted: bool,
    pub ptt_enabled: bool,
    pub grant: Option<Grant>,
    pub recovery_mode: Option<RecoveryMode>,
    challenge: u64,
    challenges: VecDeque<(u64, u64)>,
    next_grant: u64,
    history: Vec<(Parent, History)>,
}
impl Default for Authority {
    fn default() -> Self {
        Self {
            revision: 1,
            muted: true,
            desired_muted: true,
            ptt_enabled: false,
            grant: None,
            recovery_mode: None,
            challenge: 0,
            challenges: VecDeque::new(),
            next_grant: 0,
            history: Vec::new(),
        }
    }
}
impl Authority {
    pub fn recovery_muted(&self) -> bool {
        self.recovery_mode
            .unwrap_or(if self.ptt_enabled {
                RecoveryMode::Ptt
            } else {
                RecoveryMode::Persistent(self.desired_muted)
            })
            .idle_muted()
    }
    pub fn complete(&mut self, parent: Parent, request: [u8; 16], result: ResultCode) {
        if let Some((_, history)) = self.history.iter_mut().find(|(p, _)| *p == parent) {
            if let Some((_, decision)) = history.requests.iter_mut().find(|(id, _)| *id == request)
            {
                decision.result = result;
            }
        }
    }
    fn history(&mut self, parent: Parent) -> &mut History {
        let index = match self.history.iter().position(|(p, _)| *p == parent) {
            Some(i) => i,
            None => {
                self.history.push((parent, History::default()));
                self.history.len() - 1
            }
        };
        &mut self.history[index].1
    }
    pub fn changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
    pub fn challenge(&mut self, now: u64) -> (u64, u64) {
        if let Some(&(id, expiry)) = self.challenges.back() {
            if now < expiry.saturating_sub(LEASE_NS - CHALLENGE_NS) {
                return (id, expiry);
            }
        }
        self.challenge = self.challenge.saturating_add(1);
        let pair = (self.challenge, now.saturating_add(LEASE_NS));
        self.challenges.retain(|&(_, expiry)| expiry > now);
        self.challenges.push_back(pair);
        pair
    }
    pub fn observe(&mut self, muted: bool) {
        if self.muted != muted {
            self.muted = muted;
            self.changed();
        }
    }
    pub fn revoke(&mut self) -> Effect {
        match self.grant.take() {
            Some(g) => {
                self.changed();
                Effect::Revoke(g.id)
            }
            None => Effect::None,
        }
    }
    pub fn expire(&mut self, now: u64) -> Effect {
        if self.grant.is_some_and(|g| g.expires_ns <= now) {
            self.revoke()
        } else {
            Effect::None
        }
    }
    pub fn reset(&mut self) {
        self.revoke();
        self.challenges.clear();
        self.muted = true;
        self.changed();
    }
    pub fn retain(&mut self, mut allowed: impl FnMut(Parent) -> bool) -> Effect {
        self.history.retain(|(p, _)| allowed(*p));
        if self.grant.is_some_and(|g| !allowed(g.parent)) {
            self.revoke()
        } else {
            Effect::None
        }
    }
    pub fn decide(
        &mut self,
        now: u64,
        parent: Parent,
        enabled: bool,
        consent: bool,
        c: Control,
    ) -> Decision {
        let reject = |result| Decision {
            result,
            effect: Effect::None,
        };
        // Authorization is rechecked even for cached requests after consent revocation.
        if !enabled {
            return reject(ResultCode::Disabled);
        }
        if !consent {
            return reject(ResultCode::ConsentRequired);
        }
        let sequence = u64::from_be_bytes(c.request[8..].try_into().unwrap());
        if sequence == 0 {
            return reject(ResultCode::Invalid);
        }
        if let Some((_, h)) = self.history.iter().find(|(p, _)| *p == parent) {
            if let Some((_, d)) = h.requests.iter().find(|(r, _)| *r == c.request) {
                return Decision {
                    result: d.result,
                    effect: Effect::None,
                };
            }
            if sequence <= h.request_high && c.op != Op::Release {
                return reject(ResultCode::Expired);
            }
        }
        if !self.history.iter().any(|(p, _)| *p == parent) && self.history.len() >= 4 {
            return reject(ResultCode::Busy);
        }
        let result = self.apply(now, parent, c);
        let h = self.history(parent);
        h.request_high = h.request_high.max(sequence);
        if h.requests.len() == HISTORY {
            h.requests.pop_front();
        }
        h.requests.push_back((c.request, result));
        result
    }
    fn apply(&mut self, now: u64, parent: Parent, c: Control) -> Decision {
        let reject = |result| Decision {
            result,
            effect: Effect::None,
        };
        // Release is revision-independent, but can only close this exact parent's press.
        if c.op == Op::Release {
            let sequence = u64::from_be_bytes(c.press[8..].try_into().unwrap());
            let h = self.history(parent);
            h.press_high = h.press_high.max(sequence);
            if self
                .grant
                .is_some_and(|g| g.parent == parent && g.press == c.press)
            {
                return Decision {
                    result: ResultCode::Ok,
                    effect: self.revoke(),
                };
            }
            return reject(ResultCode::Expired);
        }
        if c.revision != self.revision && c.op != Op::SetMute(true) {
            return reject(ResultCode::StaleRevision);
        }
        if let Op::SetMute(muted) = c.op {
            self.desired_muted = muted;
            self.changed();
            return Decision {
                result: ResultCode::Ok,
                effect: Effect::SetMute(muted),
            };
        }
        if !self.ptt_enabled || c.press == [0; 16] {
            return reject(ResultCode::Invalid);
        }
        if c.expires_ns <= now
            || !self
                .challenges
                .iter()
                .any(|&(id, end)| id == c.challenge && end == c.expires_ns)
        {
            return reject(ResultCode::Expired);
        }
        if let Some(g) = self.grant {
            if g.expires_ns <= now {
                return reject(ResultCode::Expired);
            }
            if g.parent != parent || g.press != c.press {
                return reject(ResultCode::Conflict);
            }
            if c.op != Op::Renew {
                return reject(ResultCode::Conflict);
            }
            let g = Grant {
                expires_ns: c.expires_ns.max(g.expires_ns),
                ..g
            };
            self.grant = Some(g);
            return Decision {
                result: ResultCode::Ok,
                effect: Effect::Grant(g),
            };
        }
        if c.op == Op::Renew {
            return reject(ResultCode::Expired);
        }
        let h = self.history(parent);
        let sequence = u64::from_be_bytes(c.press[8..].try_into().unwrap());
        if sequence == 0 || sequence <= h.press_high {
            return reject(ResultCode::Expired);
        }
        h.press_high = sequence;
        self.next_grant = self.next_grant.saturating_add(1).max(1);
        let g = Grant {
            parent,
            press: c.press,
            expires_ns: c.expires_ns,
            id: self.next_grant,
        };
        self.grant = Some(g);
        self.changed();
        Decision {
            result: ResultCode::Ok,
            effect: Effect::Grant(g),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parent(n: u8) -> Parent {
        Parent {
            peer: [n; 32],
            connection_id: [n; 16],
            generation: [n; 16],
        }
    }
    fn press(a: &mut Authority, now: u64, n: u8) -> Control {
        let (challenge, expires_ns) = a.challenge(now);
        Control {
            request: [n; 16],
            revision: a.revision,
            op: Op::Press,
            press: [n; 16],
            challenge,
            expires_ns,
        }
    }
    #[test]
    fn delayed_press_and_expired_renew_do_not_get_fresh_ttl() {
        let mut a = Authority {
            ptt_enabled: true,
            ..Authority::default()
        };
        let mut c = press(&mut a, 0, 1);
        assert_eq!(
            a.decide(LEASE_NS, parent(1), true, true, c).result,
            ResultCode::Expired
        );
        c.request = [2; 16];
        c.expires_ns = a.challenge(LEASE_NS).1;
        c.challenge = a.challenge;
        assert_eq!(
            a.decide(LEASE_NS, parent(1), true, true, c).result,
            ResultCode::Ok
        );
        let expiry = a.grant.unwrap().expires_ns;
        c.request = [3; 16];
        c.op = Op::Renew;
        c.revision = a.revision;
        assert_eq!(
            a.decide(expiry, parent(1), true, true, c).result,
            ResultCode::Expired
        );
    }
    #[test]
    fn release_is_parent_and_press_fenced_and_not_replayable() {
        let mut a = Authority {
            ptt_enabled: true,
            ..Authority::default()
        };
        let c = press(&mut a, 0, 1);
        a.decide(0, parent(1), true, true, c);
        let mut release = c;
        release.request = [2; 16];
        release.op = Op::Release;
        assert_eq!(
            a.decide(1, parent(2), true, true, release).result,
            ResultCode::Expired
        );
        assert!(a.grant.is_some());
        a.decide(1, parent(1), true, true, release);
        assert!(a.grant.is_none());
        let mut replay = c;
        replay.request = [3; 16];
        replay.revision = a.revision;
        assert_eq!(
            a.decide(2, parent(1), true, true, replay).result,
            ResultCode::Expired
        );
    }
    #[test]
    fn consent_and_hardware_are_not_inferred_from_write() {
        let mut a = Authority::default();
        let c = Control {
            op: Op::SetMute(false),
            ..press(&mut a, 0, 1)
        };
        assert_eq!(
            a.decide(0, parent(1), true, false, c).result,
            ResultCode::ConsentRequired
        );
        assert!(a.desired_muted);
        a.decide(0, parent(1), true, true, c);
        assert!(!a.desired_muted);
        assert!(a.muted);
        a.observe(false);
        assert!(!a.muted);
    }
    #[test]
    fn duplicate_request_does_not_repeat_hardware_effect() {
        let mut a = Authority {
            ptt_enabled: true,
            ..Authority::default()
        };
        let c = press(&mut a, 0, 1);
        assert!(matches!(
            a.decide(0, parent(1), true, true, c).effect,
            Effect::Grant(_)
        ));
        assert_eq!(a.decide(1, parent(1), true, true, c).effect, Effect::None);
        assert!(matches!(a.retain(|_| false), Effect::Revoke(_)));
        assert!(a.grant.is_none());
    }
    #[test]
    fn release_before_press_and_reset_keep_high_water_marks() {
        let mut a = Authority {
            ptt_enabled: true,
            ..Authority::default()
        };
        let c = press(&mut a, 0, 3);
        let release = Control {
            request: [4; 16],
            op: Op::Release,
            ..c
        };
        a.decide(0, parent(1), true, true, release);
        let replay = Control {
            request: [5; 16],
            ..c
        };
        assert_eq!(
            a.decide(1, parent(1), true, true, replay).result,
            ResultCode::Expired
        );
        a.reset();
        let (challenge, expires_ns) = a.challenge(2);
        let replay = Control {
            request: [6; 16],
            revision: a.revision,
            challenge,
            expires_ns,
            ..c
        };
        assert_eq!(
            a.decide(2, parent(1), true, true, replay).result,
            ResultCode::Expired
        );
    }
    #[test]
    fn more_than_sixty_four_presses_need_no_reconnect() {
        let mut a = Authority {
            ptt_enabled: true,
            ..Authority::default()
        };
        for n in 1..=100u64 {
            let mut press_id = [0; 16];
            press_id[8..].copy_from_slice(&n.to_be_bytes());
            let mut request = [0; 16];
            request[8..].copy_from_slice(&(n * 2).to_be_bytes());
            let (challenge, expires_ns) = a.challenge(0);
            let c = Control {
                request,
                revision: a.revision,
                op: Op::Press,
                press: press_id,
                challenge,
                expires_ns,
            };
            assert_eq!(a.decide(0, parent(1), true, true, c).result, ResultCode::Ok);
            request[8..].copy_from_slice(&(n * 2 + 1).to_be_bytes());
            assert_eq!(
                a.decide(
                    0,
                    parent(1),
                    true,
                    true,
                    Control {
                        request,
                        op: Op::Release,
                        ..c
                    }
                )
                .result,
                ResultCode::Ok
            );
        }
    }
    #[test]
    fn cached_completion_reports_native_refusal_without_retrying() {
        let mut a = Authority {
            ptt_enabled: true,
            ..Authority::default()
        };
        let c = press(&mut a, 0, 1);
        a.decide(0, parent(1), true, true, c);
        a.complete(parent(1), c.request, ResultCode::Conflict);
        let d = a.decide(1, parent(1), true, true, c);
        assert_eq!(d.result, ResultCode::Conflict);
        assert_eq!(d.effect, Effect::None);
    }
    #[test]
    fn child_loss_preserves_base_mode_without_replaying_talk() {
        for mode in [
            RecoveryMode::Ptt,
            RecoveryMode::Persistent(true),
            RecoveryMode::Persistent(false),
        ] {
            let mut authority = Authority {
                ptt_enabled: true,
                recovery_mode: Some(mode),
                ..Authority::default()
            };
            let control = press(&mut authority, 0, 1);
            authority.decide(0, parent(1), true, true, control);
            authority.observe(false);
            authority.reset();
            assert!(authority.grant.is_none());
            assert_eq!(authority.recovery_mode, Some(mode));
            assert_eq!(
                authority.recovery_muted(),
                mode != RecoveryMode::Persistent(false)
            );
            assert_eq!(
                authority.decide(1, parent(1), true, true, control).effect,
                Effect::None
            );
        }
    }
}

//! Transient media facts are authorized by the live system session, never by metadata.
use crate::{
    session::{Parent, Phase, SessionView},
    status::{MediaCommand, MediaInfo},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub view: SessionView,
    pub current_parent: bool,
    pub consented: bool,
    pub capable: bool,
    pub worker_generation: [u8; 16],
}
impl Scope {
    pub fn live(self) -> bool {
        self.current_parent
            && self.consented
            && self.capable
            && self.view.phase == Phase::Active
            && self.view.worker_generation == self.worker_generation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Inactive,
    ConsentRequired,
    StaleRevision,
    Unsupported,
}

#[derive(Default)]
pub struct Relay {
    scope: Option<Scope>,
    revision: u64,
    info: Option<MediaInfo>,
}
impl Relay {
    /// A changed parent, worker, session, or withdrawn authorization erases all facts.
    pub fn scope(&mut self, scope: Option<Scope>) {
        let scope = scope.filter(|s| s.live());
        if self.scope != scope {
            self.info = None;
            self.revision = 0;
            self.scope = scope;
        }
    }
    pub fn update(
        &mut self,
        parent: Parent,
        session: [u8; 16],
        revision: u64,
        info: Option<MediaInfo>,
    ) -> bool {
        if !self
            .scope
            .is_some_and(|s| s.view.parent == parent && s.view.session == session)
            || revision == 0
            || revision <= self.revision
            || info
                .as_ref()
                .is_some_and(|i| i.session_token == 0 || i.revision == 0 || i.validate().is_err())
        {
            return false;
        }
        self.revision = revision;
        self.info = info;
        true
    }
    pub fn view(&self) -> Option<SessionView> {
        self.scope.map(|s| s.view)
    }
    pub fn info(&self) -> Option<&MediaInfo> {
        self.info.as_ref()
    }
    pub fn authorize(
        &self,
        allowed: bool,
        token: u64,
        revision: u64,
        command: MediaCommand,
    ) -> Result<(), Refusal> {
        if self.scope.is_none() {
            return Err(Refusal::Inactive);
        }
        if !allowed {
            return Err(Refusal::ConsentRequired);
        }
        let info = self.info.as_ref().ok_or(Refusal::StaleRevision)?;
        if token != info.session_token || revision != info.revision {
            return Err(Refusal::StaleRevision);
        }
        let supported = match command {
            MediaCommand::Play => info.commands.play,
            MediaCommand::Pause => info.commands.pause,
            MediaCommand::Toggle => info.commands.toggle,
            MediaCommand::Next => info.commands.next,
            MediaCommand::Previous => info.commands.previous,
            MediaCommand::SeekTo { position_ms } => info
                .seek_range()
                .is_some_and(|(min, max)| (min..=max).contains(&position_ms)),
        };
        if supported {
            Ok(())
        } else {
            Err(Refusal::Unsupported)
        }
    }
}

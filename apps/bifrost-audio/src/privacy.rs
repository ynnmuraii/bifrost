use protocol::audio_worker::{self as w, audio_frame::Msg};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};

pub(crate) enum Reduction {
    Mute(u64, u64),
    Revoke(u64, u64),
    Release(u64),
    ReleaseAll(u64),
}

// Admission never waits for native control or for a mutex held by another thread.
pub(crate) struct Privacy {
    pub epoch: AtomicU64,
    pub talk_epoch: AtomicU64,
    pub grant_highwater: AtomicU64,
    pub revoked_through: AtomicU64,
    pub native_epoch: AtomicU64,
    pub native_fence_ns: AtomicU64,
    pub release_epoch: AtomicU64,
    pub failed: AtomicBool,
    pub active: AtomicBool,
    sender: mpsc::SyncSender<Reduction>,
    receiver: Mutex<mpsc::Receiver<Reduction>>,
    wake: mpsc::SyncSender<()>,
    wakes: Mutex<mpsc::Receiver<()>>,
}
impl Privacy {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel(16);
        let (wake, wakes) = mpsc::sync_channel(1);
        Arc::new(Self {
            epoch: AtomicU64::new(0),
            talk_epoch: AtomicU64::new(0),
            grant_highwater: AtomicU64::new(0),
            revoked_through: AtomicU64::new(0),
            native_epoch: AtomicU64::new(0),
            native_fence_ns: AtomicU64::new(0),
            release_epoch: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            active: AtomicBool::new(false),
            sender,
            receiver: Mutex::new(receiver),
            wake,
            wakes: Mutex::new(wakes),
        })
    }
    fn fence_talk(&self, now_ns: Option<u64>) -> u64 {
        let epoch = self.talk_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.revoked_through.fetch_max(
            self.grant_highwater.load(Ordering::Acquire),
            Ordering::AcqRel,
        );
        self.native_fence_ns
            .store(now_ns.unwrap_or(u64::MAX), Ordering::Release);
        self.native_epoch.store(epoch, Ordering::Release);
        epoch
    }
    /// True means queued to an owned guard; false is an already-closed, guardless release.
    pub fn admit(&self, id: u64, message: &Msg) -> Option<Result<bool, i32>> {
        self.admit_at(id, message, crate::clock::now_ns())
    }
    pub fn admit_at(
        &self,
        id: u64,
        message: &Msg,
        now_ns: Option<u64>,
    ) -> Option<Result<bool, i32>> {
        let reduction = match message {
            Msg::PttGrant(request) => {
                self.grant_highwater
                    .fetch_max(request.grant_id, Ordering::AcqRel);
                return None;
            }
            Msg::PttReleaseAll(_) => {
                let epoch = self.fence_talk(now_ns);
                self.release_epoch.store(epoch, Ordering::Release);
                Reduction::ReleaseAll(id)
            }
            Msg::SetMicMute(w::SetMicMute {
                muted: true,
                expected_generation,
            }) => {
                self.epoch.fetch_add(1, Ordering::AcqRel);
                self.fence_talk(now_ns);
                Reduction::Mute(id, *expected_generation)
            }
            Msg::PttRevoke(request) => {
                self.talk_epoch.fetch_add(1, Ordering::AcqRel);
                Reduction::Revoke(id, request.grant_id)
            }
            Msg::ReleaseMicMute(_) => {
                self.epoch.fetch_add(1, Ordering::AcqRel);
                Reduction::Release(id)
            }
            _ => return None,
        };
        if !self.active.load(Ordering::Acquire) {
            if matches!(reduction, Reduction::ReleaseAll(_)) {
                return Some(Ok(false));
            }
            return Some(Err(6));
        }
        let release_all = matches!(reduction, Reduction::ReleaseAll(_));
        let accepted = self.sender.try_send(reduction).is_ok();
        if !accepted && !release_all {
            self.failed.store(true, Ordering::Release);
        }
        self.wake();
        Some(if accepted { Ok(true) } else { Err(3) })
    }
    pub fn wake(&self) {
        let _ = self.wake.try_send(());
    }
    pub fn next(&self) -> Option<Reduction> {
        super::lock(&self.receiver).try_recv().ok()
    }
    pub fn wait(&self, timeout: Option<std::time::Duration>) {
        let wakes = super::lock(&self.wakes);
        match timeout {
            Some(timeout) => {
                let _ = wakes.recv_timeout(timeout);
            }
            None => {
                let _ = wakes.recv();
            }
        }
    }
}

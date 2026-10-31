use protocol::audio_worker::{self as w, audio_frame::Msg};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};

pub(crate) enum Reduction {
    Mute(u64, u64),
    Revoke(u64, u64),
    Release(u64),
}

// Admission never waits for native control or for a mutex held by another thread.
pub(crate) struct Privacy {
    pub epoch: AtomicU64,
    pub talk_epoch: AtomicU64,
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
            failed: AtomicBool::new(false),
            active: AtomicBool::new(false),
            sender,
            receiver: Mutex::new(receiver),
            wake,
            wakes: Mutex::new(wakes),
        })
    }
    pub fn admit(&self, id: u64, message: &Msg) -> Option<Result<(), i32>> {
        let reduction = match message {
            Msg::SetMicMute(w::SetMicMute {
                muted: true,
                expected_generation,
            }) => {
                self.epoch.fetch_add(1, Ordering::AcqRel);
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
            return Some(Err(6));
        }
        let accepted = self.sender.try_send(reduction).is_ok();
        if !accepted {
            self.failed.store(true, Ordering::Release);
        }
        self.wake();
        Some(if accepted { Ok(()) } else { Err(3) })
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

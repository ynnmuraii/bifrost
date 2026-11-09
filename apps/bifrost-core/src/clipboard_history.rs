//! Session-only clipboard history. Full text stays in memory and secret items are never retained.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::ipc;

const CAPACITY: usize = 30;
const PREVIEW_CHARS: usize = 120;

/// Immutable UTF-8, validated once and shared with carry, agent handoff and wire transfers.
#[derive(Clone)]
pub struct SharedText(Arc<Vec<u8>>);

impl SharedText {
    pub fn from_string(text: String) -> Self {
        Self(Arc::new(text.into_bytes()))
    }

    pub fn from_payload(payload: Arc<Vec<u8>>) -> Result<Self, std::str::Utf8Error> {
        std::str::from_utf8(&payload)?;
        Ok(Self(payload))
    }

    pub fn payload(&self) -> Arc<Vec<u8>> {
        Arc::clone(&self.0)
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: both constructors establish UTF-8; the shared bytes are never mutable.
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }
}

impl std::fmt::Debug for SharedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedText")
            .field("bytes", &self.0.len())
            .finish()
    }
}

struct Item {
    entry: ipc::ClipboardHistoryEntry,
    text: SharedText,
    sha256: [u8; 32],
}

/// The last 30 transferred text items, newest first; owned by the core session.
#[derive(Default)]
pub struct History {
    items: VecDeque<Item>,
    last_id: u64,
}

impl History {
    /// Retains a completed transfer, unless secret-marked or the ID space is exhausted.
    /// Repeated and empty texts are transfers too; each receives its own stable ID.
    pub fn record_prepared(
        &mut self,
        direction: ipc::ClipboardHistoryDirection,
        peer_name: String,
        text: SharedText,
        sha256: [u8; 32],
        secret: bool,
    ) -> Option<u64> {
        if secret {
            return None;
        }
        let id = self.last_id.checked_add(1)?;
        let time_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let entry = ipc::ClipboardHistoryEntry {
            id,
            direction: direction as i32,
            peer_name,
            time_unix_ms,
            size_bytes: text.as_str().len() as u64,
            preview: text.as_str().chars().take(PREVIEW_CHARS).collect(),
        };
        if self.items.len() == CAPACITY {
            self.items.pop_back();
        }
        self.items.push_front(Item {
            entry,
            text,
            sha256,
        });
        self.last_id = id;
        Some(id)
    }

    #[cfg(test)]
    fn record(
        &mut self,
        direction: ipc::ClipboardHistoryDirection,
        peer_name: String,
        text: String,
        secret: bool,
    ) -> Option<u64> {
        let text = SharedText::from_string(text);
        let sha256 = clipboard::sha256(text.as_str().as_bytes())
            .try_into()
            .expect("SHA-256 length");
        self.record_prepared(direction, peer_name, text, sha256, secret)
    }

    /// Reuses worker-prepared bytes and digest without hashing or copying on the session.
    pub fn prepared(&self, id: u64) -> Option<(Arc<Vec<u8>>, [u8; 32])> {
        self.items
            .iter()
            .find(|item| item.entry.id == id)
            .map(|item| (item.text.payload(), item.sha256))
    }

    /// Returns metadata only, newest first; full payloads are fetched separately.
    pub fn entries(&self) -> ipc::ClipboardHistory {
        ipc::ClipboardHistory {
            items: self.items.iter().map(|item| item.entry.clone()).collect(),
        }
    }

    /// Borrows the full text if the entry has not been evicted or cleared.
    pub fn text(&self, id: u64) -> Option<&str> {
        self.items
            .iter()
            .find(|item| item.entry.id == id)
            .map(|item| item.text.as_str())
    }

    /// Drops all retained payloads without reusing IDs for later transfers.
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_text_rejects_invalid_utf8_and_history_outlives_transfer_and_carry() {
        assert!(SharedText::from_payload(Arc::new(vec![0xff])).is_err());
        let text = SharedText::from_string("Текст retained".to_owned());
        let mut carry = text.payload();
        let digest = clipboard::sha256(&carry).try_into().unwrap();
        let mut history = History::default();
        let id = history
            .record_prepared(
                ipc::ClipboardHistoryDirection::Sent,
                "LAPTOP".into(),
                text,
                digest,
                false,
            )
            .unwrap();
        // A newer copy mutates only a detached allocation, never the retained UTF-8.
        Arc::make_mut(&mut carry).clear();
        drop(carry);
        assert_eq!(history.text(id), Some("Текст retained"));
        let (transfer, hash) = history.prepared(id).unwrap();
        assert_eq!(hash, digest);
        history.clear();
        assert_eq!(transfer.as_slice(), "Текст retained".as_bytes());
        assert_eq!(history.text(id), None);
    }

    #[test]
    fn retains_the_latest_thirty_transfers_newest_first() {
        let mut history = History::default();
        for index in 0..31 {
            assert_eq!(
                history.record(
                    ipc::ClipboardHistoryDirection::Sent,
                    "peer".into(),
                    index.to_string(),
                    false,
                ),
                Some(index + 1)
            );
        }
        let entries = history.entries().items;
        assert_eq!(entries.len(), CAPACITY);
        assert_eq!(entries.first().unwrap().id, 31);
        assert_eq!(entries.last().unwrap().id, 2);
        assert!(entries.windows(2).all(|pair| pair[0].id > pair[1].id));
        assert_eq!(history.text(1), None);
        assert_eq!(history.text(31), Some("30"));
    }

    #[test]
    fn excludes_secrets_without_evicting_or_consuming_ids() {
        let mut history = History::default();
        for _ in 0..CAPACITY {
            history.record(
                ipc::ClipboardHistoryDirection::Received,
                "peer".into(),
                "public".into(),
                false,
            );
        }
        assert_eq!(
            history.record(
                ipc::ClipboardHistoryDirection::Sent,
                "secret peer".into(),
                "secret".into(),
                true,
            ),
            None
        );
        assert_eq!(history.entries().items.len(), CAPACITY);
        assert_eq!(history.text(1), Some("public"));
        assert_eq!(history.last_id, CAPACITY as u64);
    }

    #[test]
    fn preserves_full_unicode_text_and_reports_metadata() {
        let mut history = History::default();
        let text = "🦀".repeat(121);
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let id = history
            .record(
                ipc::ClipboardHistoryDirection::Received,
                "desktop".into(),
                text.clone(),
                false,
            )
            .unwrap();
        let entry = history.entries().items.remove(0);
        assert_eq!(
            entry.direction,
            ipc::ClipboardHistoryDirection::Received as i32
        );
        assert_eq!(entry.peer_name, "desktop");
        assert!(entry.time_unix_ms >= before);
        assert_eq!(entry.size_bytes, text.len() as u64);
        assert_eq!(entry.preview, "🦀".repeat(PREVIEW_CHARS));
        assert_eq!(history.text(id), Some(text.as_str()));
        assert_eq!(history.text(id + 1), None);
    }

    #[test]
    fn clear_drops_payloads_and_does_not_reuse_ids() {
        let mut history = History::default();
        let first = history
            .record(
                ipc::ClipboardHistoryDirection::Sent,
                "peer".into(),
                "".into(),
                false,
            )
            .unwrap();
        assert_eq!(history.text(first), Some(""));
        history.clear();
        assert!(history.entries().items.is_empty());
        assert_eq!(history.text(first), None);
        let second = history
            .record(
                ipc::ClipboardHistoryDirection::Sent,
                "peer".into(),
                "next".into(),
                false,
            )
            .unwrap();
        assert!(second > first);
    }
}

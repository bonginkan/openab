//! Durable queue spool.
//!
//! `dispatch::Dispatcher` buffers each arrival event in a per-thread tokio mpsc
//! until a consumer batches it into an ACP turn. Those buffered messages live
//! only in memory: on shutdown they are dropped (`Dispatcher::shutdown` logs
//! `buffered_lost`), and on `kill -9` they vanish with no log. Restarting a bot
//! therefore loses whatever was still queued.
//!
//! This spool is the durable side of that queue. A message is persisted the
//! moment it is buffered and removed the moment a consumer picks it up for
//! dispatch, so the on-disk set is exactly "queued but not yet started". On
//! startup the survivors are replayed back through `submit`, giving at-least-once
//! delivery of the backlog across restarts.
//!
//! It mirrors the atomic tmp+rename JSON pattern already used by
//! `acp::pool::SessionPool` (`thread_map.json`) and `remind::ReminderStore`
//! (`reminders.json`). `$HOME/.openab` is shared by every bot process on the
//! host (they all run under one `HOME`), so — unlike the thread-keyed cache —
//! the spool is keyed by a caller-supplied bot slug to keep one bot from
//! clobbering another bot's queue file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::acp::ContentBlock;
use crate::adapter::{ChannelRef, MessageRef};

/// One queued arrival event, persisted so it survives a restart.
///
/// This is the serialisable subset of `dispatch::BufferedMessage` plus the
/// routing context needed to re-submit it (`thread_key`, `thread_channel`,
/// `adapter_kind`). `arrived_at: Instant` is monotonic and not persistable; on
/// reload the rebuilt message takes a fresh `Instant::now()`, and
/// `enqueued_at_ms` preserves the wall-clock order for observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedMessage {
    pub id: u64,
    pub thread_key: String,
    pub thread_channel: ChannelRef,
    pub adapter_kind: String,
    pub sender_json: String,
    pub sender_name: String,
    pub prompt: String,
    pub extra_blocks: Vec<ContentBlock>,
    pub trigger_msg: MessageRef,
    pub estimated_tokens: usize,
    pub other_bot_present: bool,
    pub enqueued_at_ms: i64,
}

/// Wall-clock epoch millis. Saturates to 0 before the epoch (never in practice).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn openab_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join(".openab")
}

/// Derive a stable per-bot slug from the `--config` source (path or URL), so
/// each bot process gets its own `queue-<slug>.json` under the shared
/// `$HOME/.openab`. Uses the last path segment with a trailing `.toml` stripped
/// (e.g. `.../config.reina.dev.toml` -> `config.reina.dev`).
pub fn slug_from_config_source(src: &str) -> String {
    let last = src.rsplit(['/', '\\']).next().unwrap_or(src);
    let stem = last.strip_suffix(".toml").unwrap_or(last);
    sanitize(stem)
}

/// Reduce a bot slug to a filename-safe token so `queue-<slug>.json` is stable
/// and cannot escape the spool directory.
fn sanitize(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

/// Durable, per-bot queue store. Share via `Arc`; all methods take `&self`.
pub struct QueueSpool {
    path: PathBuf,
    // std::Mutex: every critical section is sync (serde + fs), never held across .await.
    inner: Mutex<Vec<PersistedMessage>>,
    next_id: AtomicU64,
}

impl QueueSpool {
    /// Open (and load) the spool for a bot slug under `$HOME/.openab`.
    /// A corrupt or missing file starts empty.
    pub fn open(bot_slug: &str) -> Self {
        let dir = openab_dir();
        let _ = std::fs::create_dir_all(&dir);
        Self::open_at(dir.join(format!("queue-{}.json", sanitize(bot_slug))))
    }

    /// Open at an explicit path (used by tests).
    pub fn open_at(path: PathBuf) -> Self {
        let entries = Self::load(&path);
        let next = entries.iter().map(|e| e.id).max().map(|m| m + 1).unwrap_or(0);
        Self {
            path,
            inner: Mutex::new(entries),
            next_id: AtomicU64::new(next),
        }
    }

    fn load(path: &Path) -> Vec<PersistedMessage> {
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "corrupt queue spool, starting fresh");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        }
    }

    fn persist_locked(&self, entries: &[PersistedMessage]) {
        let data = match serde_json::to_string_pretty(entries) {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "failed to serialize queue spool");
                return;
            }
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &data).and_then(|_| std::fs::rename(&tmp, &self.path)) {
            warn!(path = %self.path.display(), error = %e, "failed to persist queue spool");
        }
    }

    /// Assign an id, append, and persist. Returns the id to later `remove`.
    /// Any `id` field on the input is overwritten with the freshly assigned one.
    pub fn append(&self, mut msg: PersistedMessage) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        msg.id = id;
        let mut entries = self.inner.lock().unwrap();
        entries.push(msg);
        self.persist_locked(&entries);
        id
    }

    /// Remove an entry by id (once a consumer has picked it up) and persist.
    /// A no-op for an unknown id (idempotent).
    pub fn remove(&self, id: u64) {
        let mut entries = self.inner.lock().unwrap();
        let before = entries.len();
        entries.retain(|e| e.id != id);
        if entries.len() != before {
            self.persist_locked(&entries);
        }
    }

    /// Remove every entry matching `pred` and persist. Used when a whole thread's
    /// buffer is cancelled (`/reset`, `/cancel-all`): those messages leave the
    /// mpsc without a consumer pickup, so they must be purged here or they would
    /// replay on the next restart.
    pub fn remove_matching(&self, pred: impl Fn(&PersistedMessage) -> bool) {
        let mut entries = self.inner.lock().unwrap();
        let before = entries.len();
        entries.retain(|e| !pred(e));
        if entries.len() != before {
            self.persist_locked(&entries);
        }
    }

    /// Snapshot the currently-queued entries (for startup replay), oldest first.
    pub fn entries(&self) -> Vec<PersistedMessage> {
        self.inner.lock().unwrap().clone()
    }

    /// Number of messages currently queued on disk.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> ChannelRef {
        ChannelRef {
            platform: "discord".to_string(),
            channel_id: "chan-1".to_string(),
            thread_id: Some("thread-1".to_string()),
            parent_id: None,
            origin_event_id: None,
        }
    }

    fn sample(prompt: &str) -> PersistedMessage {
        PersistedMessage {
            id: 0,
            thread_key: "discord:thread-1".to_string(),
            thread_channel: channel(),
            adapter_kind: "discord".to_string(),
            sender_json: r#"{"sender_id":"u1"}"#.to_string(),
            sender_name: "user".to_string(),
            prompt: prompt.to_string(),
            extra_blocks: vec![ContentBlock::Text { text: "img-caption".to_string() }],
            trigger_msg: MessageRef {
                channel: channel(),
                message_id: "msg-1".to_string(),
            },
            estimated_tokens: 42,
            other_bot_present: false,
            enqueued_at_ms: now_ms(),
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("openab-spool-{tag}-{nanos}.json"))
    }

    #[test]
    fn append_persists_and_survives_reopen() {
        let path = temp_path("roundtrip");
        let spool = QueueSpool::open_at(path.clone());
        let id0 = spool.append(sample("first"));
        let id1 = spool.append(sample("second"));
        assert_eq!((id0, id1), (0, 1));
        assert_eq!(spool.len(), 2);

        // Reopen from disk: entries and ids survive, next id continues.
        let reopened = QueueSpool::open_at(path.clone());
        let entries = reopened.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].prompt, "first");
        assert_eq!(entries[1].prompt, "second");
        assert_eq!(entries[0].thread_channel.channel_id, "chan-1");
        assert_eq!(entries[1].id, 1);
        assert_eq!(reopened.append(sample("third")), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_is_persistent_and_idempotent() {
        let path = temp_path("remove");
        let spool = QueueSpool::open_at(path.clone());
        let id0 = spool.append(sample("a"));
        let _id1 = spool.append(sample("b"));
        spool.remove(id0);
        spool.remove(id0); // idempotent
        assert_eq!(spool.len(), 1);

        let reopened = QueueSpool::open_at(path.clone());
        let entries = reopened.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].prompt, "b");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_starts_empty() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{ this is not json").unwrap();
        let spool = QueueSpool::open_at(path.clone());
        assert_eq!(spool.len(), 0);
        // Still usable after a corrupt load.
        spool.append(sample("recover"));
        assert_eq!(spool.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sanitize_keeps_filenames_safe() {
        assert_eq!(sanitize("reina"), "reina");
        assert_eq!(sanitize("config.reina.dev"), "config_reina_dev");
        assert_eq!(sanitize("../escape"), "___escape");
        assert_eq!(sanitize(""), "default");
    }

    #[test]
    fn slug_from_config_source_is_per_bot() {
        assert_eq!(slug_from_config_source("/home/x/dev/config/config.reina.dev.toml"), "config_reina_dev");
        assert_eq!(slug_from_config_source("config.hana.dev.toml"), "config_hana_dev");
        assert_eq!(slug_from_config_source("https://example.test/config.northstar.toml"), "config_northstar");
        // distinct bots get distinct files
        assert_ne!(
            slug_from_config_source("config.reina.dev.toml"),
            slug_from_config_source("config.hana.dev.toml"),
        );
    }

    #[test]
    fn remove_matching_purges_a_thread() {
        let path = temp_path("purge");
        let spool = QueueSpool::open_at(path.clone());
        let a = sample("keep");
        let mut b = sample("drop");
        b.thread_key = "discord:other".to_string();
        spool.append(a);
        spool.append(b);
        spool.remove_matching(|e| e.thread_key == "discord:other");
        let entries = spool.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].prompt, "keep");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extra_blocks_roundtrip_both_variants() {
        let path = temp_path("blocks");
        let spool = QueueSpool::open_at(path.clone());
        let mut msg = sample("with-image");
        msg.extra_blocks = vec![
            ContentBlock::Text { text: "caption".to_string() },
            ContentBlock::Image { media_type: "image/png".to_string(), data: "AAAA".to_string() },
        ];
        spool.append(msg);

        let reopened = QueueSpool::open_at(path.clone());
        let blocks = &reopened.entries()[0].extra_blocks;
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "AAAA");
            }
            _ => panic!("expected image block"),
        }
        let _ = std::fs::remove_file(&path);
    }
}

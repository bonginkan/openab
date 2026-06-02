use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, warn};

use crate::acp::{classify_notification, AcpEvent, ContentBlock, SessionPool};
use crate::config::{AttachmentsConfig, ReactionsConfig, ToolDisplay};
use crate::error_display::{format_coded_error, format_user_error};
use crate::format;
use crate::markdown::{self, TableMode};
use crate::reactions::StatusReactionController;

// --- Output directive parsing ---

/// Parsed directives from agent output header block.
/// Consecutive `[[key:value]]` lines at the start of output are directives.
#[derive(Default, Debug)]
pub struct OutputDirectives {
    /// Message ID to reply to (Discord: message_reference)
    pub reply_to: Option<String>,
    /// Local image files produced by the agent and safe-listed for outbound upload.
    pub attach_images: Vec<String>,
}

/// Parse `[[key:value]]` directives from the beginning of agent output.
/// Returns parsed directives and the remaining content (directives stripped).
pub fn parse_output_directives(content: &str) -> (OutputDirectives, String) {
    let mut directives = OutputDirectives::default();
    let mut content_start = 0;
    let mut trailing_content: Option<&str> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        // Try to match [[key:value]] at the start of the line (lenient: allows trailing content)
        if let Some(after_open) = trimmed.strip_prefix("[[") {
            if let Some(close_pos) = after_open.find("]]") {
                let inner = &after_open[..close_pos];
                if let Some((key, value)) = inner.split_once(':') {
                    match key.trim() {
                        "reply_to" => {
                            let v = value.trim();
                            // Validate: non-empty, reasonable length, no whitespace/control chars
                            if !v.is_empty()
                                && v.len() <= 64
                                && v.chars().all(|c| {
                                    c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'
                                })
                            {
                                directives.reply_to = Some(v.to_string());
                            }
                        }
                        "attach_image" => {
                            let v = value.trim();
                            if is_valid_attachment_directive_path(v) {
                                directives.attach_images.push(v.to_string());
                            }
                        }
                        _ => {
                            tracing::debug!(key = key.trim(), "unknown output directive ignored");
                        }
                    }
                    // Check for trailing content after ]]
                    let remainder = after_open[close_pos + 2..].trim();
                    if !remainder.is_empty() {
                        trailing_content = Some(remainder);
                        // Advance past this line
                        content_start += line.len();
                        if content.as_bytes().get(content_start) == Some(&b'\r') {
                            content_start += 1;
                        }
                        if content.as_bytes().get(content_start) == Some(&b'\n') {
                            content_start += 1;
                        }
                        break; // Trailing content ends directive header
                    }
                    // Advance past this line + its line ending (handles both \n and \r\n)
                    content_start += line.len();
                    if content.as_bytes().get(content_start) == Some(&b'\r') {
                        content_start += 1;
                    }
                    if content.as_bytes().get(content_start) == Some(&b'\n') {
                        content_start += 1;
                    }
                } else {
                    // [[X]] without colon — not a directive, stop parsing
                    break;
                }
            } else {
                // No closing ]] found — not a directive, stop parsing
                break;
            }
        } else {
            break;
        }
    }

    let remaining = if let Some(trailing) = trailing_content {
        if content_start < content.len() {
            format!("{}\n{}", trailing, &content[content_start..])
        } else {
            trailing.to_string()
        }
    } else if content_start < content.len() {
        content[content_start..].to_string()
    } else {
        String::new()
    };
    (directives, remaining)
}

fn is_valid_attachment_directive_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.contains('\0')
        && !value.chars().any(|c| c.is_control() && c != '\t')
}

// --- Platform-agnostic types ---

/// Identifies a channel or thread across platforms.
///
/// Used for **routing**: `channel_id` is the ID the adapter sends messages to.
/// For Discord threads, this is the thread's own channel ID (Discord API
/// requires it for `say`/`edit`). Use `parent_id` to find the parent channel.
///
/// Compare with `SenderContext`, which is **metadata for the agent**: there
/// `channel_id` is the parent channel and `thread_id` is the thread,
/// matching Slack's model for cross-platform consistency.
#[derive(Clone, Debug)]
pub struct ChannelRef {
    pub platform: String,
    pub channel_id: String,
    /// Thread within a channel (e.g. Slack thread_ts, Telegram topic_id).
    /// For Discord, threads are separate channels so this is None.
    pub thread_id: Option<String>,
    /// Parent channel if this is a thread-as-channel (Discord).
    pub parent_id: Option<String>,
    /// Originating gateway event ID, propagated back in `GatewayReply.reply_to`
    /// so the gateway can correlate replies with inbound events (e.g. LINE reply tokens).
    /// Excluded from Hash/Eq — two ChannelRefs pointing to the same channel are
    /// equal regardless of which event they originated from.
    pub origin_event_id: Option<String>,
}

impl PartialEq for ChannelRef {
    fn eq(&self, other: &Self) -> bool {
        self.platform == other.platform
            && self.channel_id == other.channel_id
            && self.thread_id == other.thread_id
            && self.parent_id == other.parent_id
    }
}

impl Eq for ChannelRef {}

impl std::hash::Hash for ChannelRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.platform.hash(state);
        self.channel_id.hash(state);
        self.thread_id.hash(state);
        self.parent_id.hash(state);
    }
}

/// Identifies a message across platforms.
#[derive(Clone, Debug)]
pub struct MessageRef {
    pub channel: ChannelRef,
    pub message_id: String,
}

/// Bundles per-message parameters for `AdapterRouter::handle_message`.
///
/// Introduced to reduce parameter count and make the signature extensible
/// (e.g. streaming policy, rate limit hints) without breaking call sites.
pub struct MessageContext {
    pub thread_channel: ChannelRef,
    pub sender_json: String,
    pub prompt: String,
    pub extra_blocks: Vec<ContentBlock>,
    pub trigger_msg: MessageRef,
    pub other_bot_present: bool,
}

/// Sender identity injected into prompts for downstream agent context.
///
/// This is **metadata for the agent** — `channel_id` always refers to the
/// logical parent channel, and `thread_id` identifies the thread (if any).
/// This convention is consistent across platforms (Slack, Discord, Telegram).
///
/// Compare with `ChannelRef`, which is used for **routing**: there
/// `channel_id` is the ID the adapter sends messages to (for Discord
/// threads, that's the thread's own channel ID, not the parent).
#[derive(Clone, Debug, Serialize)]
pub struct SenderContext {
    pub schema: String,
    pub sender_id: String,
    pub sender_name: String,
    pub display_name: String,
    pub channel: String,
    pub channel_id: String,
    /// Thread identifier, if the message is inside a thread.
    /// Slack: thread_ts. Discord: thread channel ID (channel_id holds the parent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub is_bot: bool,
    /// Platform message creation time (ISO 8601 UTC), if available.
    /// Discord/Slack: platform timestamp. Gateway: broker receive time (best-effort).
    /// Additive optional field — schema version stays openab.sender.v1 (no consumer
    /// breakage). If future additions require breaking changes, bump to v1.1+.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Platform message ID. Agents can use this to reply to a specific message
    /// via the `[[reply_to:<message_id>]]` output directive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// The platform user ID of the receiving bot/agent.
    /// Enables agents to identify themselves when multiple agents share the same backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver_id: Option<String>,
}

// --- ChatAdapter trait ---

#[async_trait]
pub trait ChatAdapter: Send + Sync + 'static {
    /// Platform name for logging and session key namespacing.
    fn platform(&self) -> &'static str;

    /// Maximum message length for this platform (e.g. 2000 for Discord, 4000 for Slack).
    fn message_limit(&self) -> usize;

    /// Send a new message, returns a reference to the sent message.
    async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef>;

    /// Create a thread from a trigger message, returns the thread channel ref.
    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        title: &str,
    ) -> Result<ChannelRef>;

    /// Add a reaction/emoji to a message.
    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()>;

    /// Remove a reaction/emoji from a message.
    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()>;

    /// Edit an existing message in-place (for streaming updates).
    /// Default: unsupported (send-once only).
    async fn edit_message(&self, _msg: &MessageRef, _content: &str) -> Result<()> {
        Err(anyhow::anyhow!("edit_message not supported"))
    }

    /// Send a message as a reply to a specific message (Discord: message_reference).
    /// Default: falls back to plain send_message (ignores reply_to).
    async fn send_message_with_reply(
        &self,
        channel: &ChannelRef,
        content: &str,
        reply_to_message_id: &str,
    ) -> Result<MessageRef> {
        let _ = reply_to_message_id; // unused in default impl
        self.send_message(channel, content).await
    }

    /// Send a message with one or more file attachments.
    /// Default: unsupported; individual adapters opt in where the platform API
    /// is wired and permission requirements are known.
    async fn send_attachments(
        &self,
        _channel: &ChannelRef,
        _content: &str,
        _attachments: Vec<OutboundAttachment>,
        _reply_to_message_id: Option<&str>,
    ) -> Result<MessageRef> {
        Err(anyhow::anyhow!(
            "outbound attachments are not supported for {}",
            self.platform()
        ))
    }

    /// Delete a message. Used to remove streaming placeholders when reply_to is set.
    /// Default: edits to zero-width space (fallback for platforms without delete support).
    async fn delete_message(&self, msg: &MessageRef) -> Result<()> {
        self.edit_message(msg, "\u{200b}").await
    }

    /// Whether this adapter should use streaming edit (true) or send-once (false).
    /// `other_bot_present` indicates if another bot has posted in the current thread.
    /// Streaming should be disabled in multi-bot threads to avoid edit interference.
    /// NOTE: Slight race window exists — the multibot cache is checked before
    /// handle_message, so a bot arriving between the check and the response will
    /// not be detected until the next message. This is acceptable: the first
    /// response may stream, but subsequent ones will correctly use send-once.
    fn use_streaming(&self, other_bot_present: bool) -> bool;
}

/// Validated outbound file payload ready for a platform adapter to upload.
#[derive(Debug, Clone)]
pub struct OutboundAttachment {
    pub filename: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct OutboundAttachments {
    enabled: bool,
    allowed_dirs: Vec<PathBuf>,
    agent_working_dir: PathBuf,
    max_bytes: u64,
    max_files: usize,
}

impl OutboundAttachments {
    fn new(config: AttachmentsConfig, agent_working_dir: impl Into<PathBuf>) -> Self {
        let agent_working_dir = agent_working_dir.into();
        let allowed_dirs = if config.allowed_dirs.is_empty() {
            vec![agent_working_dir.clone()]
        } else {
            config.allowed_dirs.into_iter().map(PathBuf::from).collect()
        };
        Self {
            enabled: config.enabled,
            allowed_dirs,
            agent_working_dir,
            max_bytes: config.max_bytes,
            max_files: config.max_files,
        }
    }

    async fn load_images(&self, paths: &[String]) -> (Vec<OutboundAttachment>, Vec<String>) {
        let mut warnings = Vec::new();
        if paths.is_empty() {
            return (Vec::new(), warnings);
        }
        if !self.enabled {
            warnings.push(
                "⚠️ Outbound image attachment skipped because [attachments].enabled is false."
                    .to_string(),
            );
            return (Vec::new(), warnings);
        }

        let mut attachments = Vec::new();
        for raw_path in paths.iter().take(self.max_files) {
            match self.load_one_image(raw_path).await {
                Ok(file) => attachments.push(file),
                Err(e) => warnings.push(format!(
                    "⚠️ Outbound image attachment skipped for `{}`: {e}",
                    sanitize_attachment_label(raw_path)
                )),
            }
        }
        if paths.len() > self.max_files {
            warnings.push(format!(
                "⚠️ Outbound image attachment limit reached; skipped {} file(s).",
                paths.len() - self.max_files
            ));
        }
        (attachments, warnings)
    }

    async fn load_one_image(&self, raw_path: &str) -> Result<OutboundAttachment> {
        let candidate = self.resolve_agent_path(raw_path);
        let canonical = tokio::fs::canonicalize(&candidate)
            .await
            .map_err(|e| anyhow::anyhow!("cannot resolve path {}: {e}", candidate.display()))?;
        self.ensure_allowed_path(&canonical).await?;

        let metadata = tokio::fs::metadata(&canonical).await?;
        anyhow::ensure!(metadata.is_file(), "path is not a regular file");
        anyhow::ensure!(
            metadata.len() <= self.max_bytes,
            "file is {} bytes, over the {} byte limit",
            metadata.len(),
            self.max_bytes
        );

        let data = tokio::fs::read(&canonical).await?;
        anyhow::ensure!(
            data.len() as u64 <= self.max_bytes,
            "file is {} bytes, over the {} byte limit",
            data.len(),
            self.max_bytes
        );
        ensure_supported_image(&data)?;

        let filename = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("path has no valid filename"))?
            .to_string();
        Ok(OutboundAttachment { filename, data })
    }

    fn resolve_agent_path(&self, raw_path: &str) -> PathBuf {
        let path = Path::new(raw_path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.agent_working_dir.join(path)
        }
    }

    async fn ensure_allowed_path(&self, canonical_file: &Path) -> Result<()> {
        for dir in &self.allowed_dirs {
            if let Ok(canonical_dir) = tokio::fs::canonicalize(dir).await {
                if canonical_file.starts_with(&canonical_dir) {
                    return Ok(());
                }
            }
        }
        Err(anyhow::anyhow!(
            "path is outside attachments.allowed_dirs (default: agent.working_dir)"
        ))
    }
}

fn ensure_supported_image(data: &[u8]) -> Result<()> {
    let reader = image::ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let format = reader.format();
    match format {
        Some(
            image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Gif
            | image::ImageFormat::WebP,
        ) => {
            let _ = reader.decode()?;
            Ok(())
        }
        _ => Err(anyhow::anyhow!(
            "file is not a supported image (png, jpg, jpeg, gif, webp)"
        )),
    }
}

fn sanitize_attachment_label(value: &str) -> String {
    let flat = value.replace(['\r', '\n'], " ");
    format::truncate_chars_tail(&flat, 160)
}

// --- AdapterRouter ---

/// Shared logic for routing messages to ACP agents, managing sessions,
/// streaming edits, and controlling reactions. Platform-independent.
pub struct AdapterRouter {
    pool: Arc<SessionPool>,
    reactions_config: ReactionsConfig,
    table_mode: TableMode,
    prompt_hard_timeout: std::time::Duration,
    /// Polling cadence for the recv-loop liveness check (#732).
    liveness_check_interval: std::time::Duration,
    /// Session keys whose next text chunk should start a new visible response
    /// section because a mid-turn steer prompt was accepted.
    pending_steer_separators: Arc<Mutex<HashSet<String>>>,
    /// Streaming post controllers keyed by session. A mid-turn steer cannot move
    /// a Discord message, so the controller creates a fresh active post after the
    /// latest user message and points future edits at it.
    active_posts: Arc<Mutex<HashMap<String, Arc<StreamingPostController>>>>,
    outbound_attachments: OutboundAttachments,
}

struct StreamingPostInner {
    adapter: Arc<dyn ChatAdapter>,
    thread_channel: ChannelRef,
    current_msgs: Vec<MessageRef>,
    latest_display: String,
    message_limit: usize,
}

struct StreamingPostController {
    inner: Mutex<StreamingPostInner>,
}

impl StreamingPostController {
    fn new(
        adapter: Arc<dyn ChatAdapter>,
        thread_channel: ChannelRef,
        current_msg: MessageRef,
        initial_display: String,
        message_limit: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(StreamingPostInner {
                adapter,
                thread_channel,
                current_msgs: vec![current_msg],
                latest_display: initial_display,
                message_limit,
            }),
        }
    }

    async fn edit(&self, content: &str) {
        let (adapter, thread_channel, current_msgs, chunks) = {
            let mut inner = self.inner.lock().await;
            inner.latest_display = content.to_string();
            (
                inner.adapter.clone(),
                inner.thread_channel.clone(),
                inner.current_msgs.clone(),
                split_streaming_display(content, inner.message_limit),
            )
        };

        let mut updated_msgs = Vec::with_capacity(chunks.len());
        for (idx, chunk) in chunks.iter().enumerate() {
            if let Some(msg) = current_msgs.get(idx) {
                let _ = adapter.edit_message(msg, chunk).await;
                updated_msgs.push(msg.clone());
            } else if let Ok(msg) = adapter.send_message(&thread_channel, chunk).await {
                updated_msgs.push(msg);
            }
        }

        for msg in current_msgs.iter().skip(chunks.len()) {
            if let Err(e) = adapter.delete_message(msg).await {
                tracing::warn!(error = ?e, "delete excess streaming post failed");
            }
        }

        let mut inner = self.inner.lock().await;
        inner.current_msgs = updated_msgs;
    }

    async fn move_after_latest(&self) -> Result<()> {
        let (adapter, thread_channel, old_msgs, chunks) = {
            let inner = self.inner.lock().await;
            (
                inner.adapter.clone(),
                inner.thread_channel.clone(),
                inner.current_msgs.clone(),
                split_streaming_display(&inner.latest_display, inner.message_limit),
            )
        };

        let mut new_msgs = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            new_msgs.push(adapter.send_message(&thread_channel, chunk).await?);
        }
        {
            let mut inner = self.inner.lock().await;
            inner.current_msgs = new_msgs;
        }
        for msg in &old_msgs {
            if let Err(e) = adapter.delete_message(msg).await {
                tracing::warn!(error = ?e, "delete superseded streaming post failed");
            }
        }
        Ok(())
    }

    async fn delete_current_messages(&self) {
        let (adapter, msgs) = {
            let inner = self.inner.lock().await;
            (inner.adapter.clone(), inner.current_msgs.clone())
        };
        for msg in msgs {
            if let Err(e) = adapter.delete_message(&msg).await {
                tracing::warn!(error = ?e, "delete streaming post failed");
            }
        }
    }
}

struct ActivePostGuard {
    map: Arc<Mutex<HashMap<String, Arc<StreamingPostController>>>>,
    session_key: String,
    controller: Arc<StreamingPostController>,
}

impl ActivePostGuard {
    async fn enter(
        map: Arc<Mutex<HashMap<String, Arc<StreamingPostController>>>>,
        session_key: String,
        controller: Arc<StreamingPostController>,
    ) -> Self {
        map.lock()
            .await
            .insert(session_key.clone(), controller.clone());
        Self {
            map,
            session_key,
            controller,
        }
    }
}

impl Drop for ActivePostGuard {
    fn drop(&mut self) {
        let map = self.map.clone();
        let session_key = self.session_key.clone();
        let controller = self.controller.clone();
        tokio::spawn(async move {
            let mut map = map.lock().await;
            if let Some(current) = map.get(&session_key) {
                if Arc::ptr_eq(current, &controller) {
                    map.remove(&session_key);
                }
            }
        });
    }
}

impl AdapterRouter {
    pub fn new(
        pool: Arc<SessionPool>,
        reactions_config: ReactionsConfig,
        table_mode: TableMode,
        prompt_hard_timeout_secs: u64,
        liveness_check_secs: u64,
        attachments_config: AttachmentsConfig,
        agent_working_dir: String,
    ) -> Self {
        if liveness_check_secs >= prompt_hard_timeout_secs {
            warn!(
                liveness_check_secs,
                prompt_hard_timeout_secs,
                "pool.liveness_check_secs >= pool.prompt_hard_timeout_secs; \
                 the hard ceiling will only fire after the next liveness tick \
                 and may be effectively bypassed. Lower liveness_check_secs."
            );
        }
        Self {
            pool,
            reactions_config,
            table_mode,
            prompt_hard_timeout: std::time::Duration::from_secs(prompt_hard_timeout_secs),
            liveness_check_interval: std::time::Duration::from_secs(liveness_check_secs),
            pending_steer_separators: Arc::new(Mutex::new(HashSet::new())),
            active_posts: Arc::new(Mutex::new(HashMap::new())),
            outbound_attachments: OutboundAttachments::new(attachments_config, agent_working_dir),
        }
    }

    /// Access the underlying session pool (e.g. for config option queries).
    pub fn pool(&self) -> &Arc<SessionPool> {
        &self.pool
    }

    /// Access the reactions config (used by dispatch.rs).
    pub fn reactions_config(&self) -> &ReactionsConfig {
        &self.reactions_config
    }

    /// Pack one arrival event into ContentBlocks. Per-arrival layout:
    ///   Text { "<sender_context>\n{json}\n</sender_context>\n\n" } <- delimiter
    ///   [Text blocks from extra_blocks (e.g. STT transcripts)]
    ///   Text { "{prompt}" }                                       <- omitted if empty
    ///   [non-Text blocks from extra_blocks (e.g. Image)]
    ///
    /// The sender_context block stands alone so it can serve as a structural
    /// delimiter between arrivals in batched dispatch — agents can scan for
    /// `<sender_context>` openers to find arrival boundaries. Within an arrival,
    /// transcript text precedes the typed prompt to match pre-batching adapter
    /// behavior (voice content first), and images trail the prompt as before.
    /// This is the single packing code path for both per-message and batched
    /// dispatch (ADR §3.5). For a batch of N messages, call this N times and
    /// concatenate.
    pub fn pack_arrival_event(
        sender_json: &str,
        prompt: &str,
        extra_blocks: Vec<ContentBlock>,
    ) -> Vec<ContentBlock> {
        let header = format!("<sender_context>\n{}\n</sender_context>\n\n", sender_json);
        let (texts, others): (Vec<_>, Vec<_>) = extra_blocks
            .into_iter()
            .partition(|b| matches!(b, ContentBlock::Text { .. }));
        let mut blocks = Vec::with_capacity(2 + texts.len() + others.len());
        blocks.push(ContentBlock::Text { text: header });
        blocks.extend(texts);
        if !prompt.is_empty() {
            blocks.push(ContentBlock::Text {
                text: prompt.to_string(),
            });
        }
        blocks.extend(others);
        blocks
    }

    /// Handle an incoming user message. The adapter is responsible for
    /// filtering, resolving the thread, and building the SenderContext.
    /// This method handles sender context injection, session management, and streaming.
    pub async fn handle_message(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        ctx: MessageContext,
    ) -> Result<()> {
        tracing::debug!(platform = adapter.platform(), "processing message");

        let content_blocks =
            Self::pack_arrival_event(&ctx.sender_json, &ctx.prompt, ctx.extra_blocks);

        let thread_key = format!(
            "{}:{}",
            adapter.platform(),
            ctx.thread_channel
                .thread_id
                .as_deref()
                .unwrap_or(&ctx.thread_channel.channel_id)
        );

        if let Err(e) = self.pool.get_or_create(&thread_key).await {
            let msg = format_user_error(&e.to_string());
            let _ = adapter
                .send_message(&ctx.thread_channel, &format!("⚠️ {msg}"))
                .await;
            error!("pool error: {e}");
            return Err(e);
        }

        let reactions = Arc::new(StatusReactionController::new(
            self.reactions_config.enabled,
            adapter.clone(),
            ctx.trigger_msg.clone(),
            self.reactions_config.emojis.clone(),
            self.reactions_config.timing.clone(),
        ));
        reactions.set_queued().await;

        let result = self
            .stream_prompt(
                adapter,
                &thread_key,
                content_blocks,
                &ctx.thread_channel,
                reactions.clone(),
                ctx.other_bot_present,
            )
            .await;

        match &result {
            Ok(()) => reactions.set_done().await,
            Err(_) => reactions.set_error().await,
        }

        let hold_ms = if result.is_ok() {
            self.reactions_config.timing.done_hold_ms
        } else {
            self.reactions_config.timing.error_hold_ms
        };
        if self.reactions_config.remove_after_reply {
            let reactions = reactions;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                reactions.clear().await;
            });
        }

        if let Err(ref e) = result {
            let _ = adapter
                .send_message(&ctx.thread_channel, &format!("⚠️ {e}"))
                .await;
        }

        result
    }

    /// Forward a steer prompt to the in-flight turn's session without taking the
    /// per-connection mutex. Used by the dispatcher's immediate-steer path: the
    /// running turn holds the connection lock for its whole duration, so the steer
    /// is written straight to the agent's stdin (lock-free) and the patched
    /// codex-acp fork injects it into the running turn. The steer's output streams
    /// back through the in-flight turn's subscriber, so there is no separate recv
    /// loop or reaction lifecycle here.
    pub async fn steer_prompt_blocks(
        &self,
        thread_key: &str,
        content_blocks: Vec<ContentBlock>,
    ) -> Result<()> {
        {
            let mut pending = self.pending_steer_separators.lock().await;
            pending.insert(thread_key.to_string());
        }

        if let Err(e) = self.pool.steer_session(thread_key, &content_blocks).await {
            let mut pending = self.pending_steer_separators.lock().await;
            pending.remove(thread_key);
            return Err(e);
        }

        let active_post = {
            let posts = self.active_posts.lock().await;
            posts.get(thread_key).cloned()
        };
        if let Some(post) = active_post {
            if let Err(e) = post.move_after_latest().await {
                warn!(error = ?e, "failed to move streaming post after latest message");
            }
        }

        Ok(())
    }

    async fn stream_prompt(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        thread_key: &str,
        content_blocks: Vec<ContentBlock>,
        thread_channel: &ChannelRef,
        reactions: Arc<StatusReactionController>,
        other_bot_present: bool,
    ) -> Result<()> {
        self.stream_prompt_blocks(
            adapter,
            thread_key,
            content_blocks,
            thread_channel,
            reactions,
            other_bot_present,
        )
        .await
    }

    /// Drive one ACP turn with the given pre-packed ContentBlocks.
    /// Called by both `handle_message` (per-message mode) and `dispatch::dispatch_batch`
    /// (batched mode).
    pub async fn stream_prompt_blocks(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        thread_key: &str,
        content_blocks: Vec<ContentBlock>,
        thread_channel: &ChannelRef,
        reactions: Arc<StatusReactionController>,
        other_bot_present: bool,
    ) -> Result<()> {
        let adapter = adapter.clone();
        let thread_channel = thread_channel.clone();
        let message_limit = adapter.message_limit();
        let streaming = adapter.use_streaming(other_bot_present);
        let table_mode = self.table_mode;
        let tool_display = self.reactions_config.tool_display;
        let prompt_hard_timeout = self.prompt_hard_timeout;
        let liveness_check_interval = self.liveness_check_interval;
        let pending_steer_separators = self.pending_steer_separators.clone();
        let thread_key_for_separator = thread_key.to_string();
        let active_posts = self.active_posts.clone();
        let thread_key_for_post = thread_key.to_string();
        let outbound_attachments = self.outbound_attachments.clone();

        self.pool
            .with_connection(thread_key, |conn| {
                let content_blocks = content_blocks.clone();
                Box::pin(async move {
                    let reset = conn.session_reset;
                    conn.session_reset = false;

                    let (mut rx, request_id) = conn.session_prompt(content_blocks).await?;
                    reactions.set_thinking().await;

                    let mut text_buf = String::new();
                    let mut tool_lines: Vec<ToolEntry> = Vec::new();

                    if reset {
                        text_buf.push_str("⚠️ _Session expired, starting fresh..._\n\n");
                    }

                    // Streaming edit: send placeholder, spawn edit loop
                    let (buf_tx, placeholder_post, _active_post_guard) = if streaming {
                        let initial = if reset {
                            "⚠️ _Session expired, starting fresh..._\n\n…".to_string()
                        } else {
                            "…".to_string()
                        };
                        let msg = adapter.send_message(&thread_channel, &initial).await?;
                        let (tx, rx) = tokio::sync::watch::channel(initial);
                        let post = Arc::new(StreamingPostController::new(
                            adapter.clone(),
                            thread_channel.clone(),
                            msg,
                            rx.borrow().clone(),
                            message_limit,
                        ));
                        let active_post_guard = ActivePostGuard::enter(
                            active_posts.clone(),
                            thread_key_for_post.clone(),
                            post.clone(),
                        )
                        .await;
                        let placeholder_post = active_post_guard.controller.clone();
                        let mut buf_rx = rx;
                        tokio::spawn(async move {
                            let mut last = String::new();
                            loop {
                                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                if buf_rx.has_changed().unwrap_or(false) {
                                    let content = buf_rx.borrow_and_update().clone();
                                    if content != last {
                                        post.edit(&content).await;
                                        last = content;
                                    }
                                }
                                if buf_rx.has_changed().is_err() {
                                    break;
                                }
                            }
                        });
                        (Some(tx), Some(placeholder_post), Some(active_post_guard))
                    } else {
                        (None, None, None)
                    };

                    // (#732) Liveness-aware recv loop. Filters stale id-bearing
                    // messages and abandons cleanly on dead agent / hard ceiling
                    // so late responses cannot leak into the next prompt.
                    let mut response_error: Option<String> = None;
                    let prompt_start = tokio::time::Instant::now();
                    loop {
                        let notification = tokio::select! {
                            msg = rx.recv() => match msg {
                                Some(n) => n,
                                // Reader saw EOF and already drained pending; nothing to abandon.
                                None => break,
                            },
                            _ = tokio::time::sleep(liveness_check_interval) => {
                                if !conn.alive() {
                                    response_error = Some("Agent process died".into());
                                    conn.abandon_request(request_id).await;
                                    break;
                                }
                                if prompt_start.elapsed() > prompt_hard_timeout {
                                    response_error = Some(format!(
                                        "Agent exceeded hard timeout ({}s)",
                                        prompt_hard_timeout.as_secs(),
                                    ));
                                    conn.abandon_request(request_id).await;
                                    break;
                                }
                                continue;
                            }
                        };
                        if let Some(notification_id) = notification.id {
                            if notification_id != request_id {
                                // Stale response from a previously-abandoned prompt.
                                // No automated test seam: this path only triggers when a
                                // real subprocess emits a late response after the broker
                                // already called abandon_request — covered by manual
                                // repro against a live agent (see #732 PR description).
                                continue;
                            }
                            if let Some(ref err) = notification.error {
                                response_error = Some(format_coded_error(err.code, &err.message, err.data_message()));
                            }
                            break;
                        }

                        if let Some(event) = classify_notification(&notification) {
                            match event {
                                AcpEvent::Text(t) => {
                                    let separate = {
                                        let mut pending = pending_steer_separators.lock().await;
                                        pending.remove(&thread_key_for_separator)
                                    };
                                    append_text_chunk(&mut text_buf, &t, separate);
                                    if let Some(tx) = &buf_tx {
                                        let _ = tx.send(compose_display(
                                            &tool_lines,
                                            &text_buf,
                                            true,
                                            tool_display,
                                        ));
                                    }
                                }
                                AcpEvent::Thinking => {
                                    reactions.set_thinking().await;
                                }
                                AcpEvent::ToolStart { id, title } if !title.is_empty() => {
                                    reactions.set_tool(&title).await;
                                    let title = sanitize_title(&title);
                                    if let Some(slot) = tool_lines.iter_mut().find(|e| e.id == id) {
                                        slot.title = title;
                                        slot.state = ToolState::Running;
                                    } else {
                                        tool_lines.push(ToolEntry {
                                            id,
                                            title,
                                            state: ToolState::Running,
                                        });
                                    }
                                    if let Some(tx) = &buf_tx {
                                        let _ = tx.send(compose_display(
                                            &tool_lines,
                                            &text_buf,
                                            true,
                                            tool_display,
                                        ));
                                    }
                                }
                                AcpEvent::ToolDone { id, title, status } => {
                                    reactions.set_thinking().await;
                                    let new_state = if status == "completed" {
                                        ToolState::Completed
                                    } else {
                                        ToolState::Failed
                                    };
                                    if let Some(slot) = tool_lines.iter_mut().find(|e| e.id == id) {
                                        if !title.is_empty() {
                                            slot.title = sanitize_title(&title);
                                        }
                                        slot.state = new_state;
                                    } else if !title.is_empty() {
                                        tool_lines.push(ToolEntry {
                                            id,
                                            title: sanitize_title(&title),
                                            state: new_state,
                                        });
                                    }
                                    if let Some(tx) = &buf_tx {
                                        let _ = tx.send(compose_display(
                                            &tool_lines,
                                            &text_buf,
                                            true,
                                            tool_display,
                                        ));
                                    }
                                }
                                AcpEvent::ConfigUpdate { options } => {
                                    conn.config_options = options;
                                }
                                _ => {}
                            }
                        }
                    }

                    conn.prompt_done().await;
                    // Stop the edit loop
                    drop(buf_tx);

                    // Parse output directives from raw text_buf BEFORE compose_display.
                    // Directives are agent meta-layer, not content — must be stripped
                    // before tool lines are composed into the display output.
                    let (directives, stripped_text) = parse_output_directives(&text_buf);
                    let has_attachment_directives = !directives.attach_images.is_empty();
                    let (attachments, attachment_warnings) = outbound_attachments
                        .load_images(&directives.attach_images)
                        .await;
                    let text_buf = stripped_text;

                    // Build final content
                    let final_content =
                        compose_display(&tool_lines, &text_buf, false, tool_display);
                    let mut final_content = if final_content.is_empty() {
                        if let Some(err) = response_error {
                            format!("⚠️ {err}")
                        } else if has_attachment_directives {
                            String::new()
                        } else {
                            "_(no response)_".to_string()
                        }
                    } else if let Some(err) = response_error {
                        format!("⚠️ {err}\n\n{final_content}")
                    } else {
                        final_content
                    };
                    if !attachment_warnings.is_empty() {
                        if !final_content.is_empty() {
                            final_content.push_str("\n\n");
                        }
                        final_content.push_str(&attachment_warnings.join("\n"));
                    }

                    let final_content = if final_content.is_empty() {
                        String::new()
                    } else {
                        markdown::convert_tables(&final_content, table_mode)
                    };
                    let chunks = if final_content.is_empty() {
                        Vec::new()
                    } else {
                        format::split_message(&final_content, message_limit)
                    };
                    if let Some(post) = placeholder_post {
                        if let Some(ref reply_id) = directives.reply_to {
                            // reply_to directive: send reply first, then delete placeholder.
                            // Only delete if send succeeds — preserves placeholder on failure.
                            let mut send_ok = false;
                            let mut first = true;
                            for chunk in &chunks {
                                if first {
                                    match adapter.send_message_with_reply(
                                        &thread_channel,
                                        chunk,
                                        reply_id,
                                    ).await {
                                        Ok(_) => { send_ok = true; }
                                        Err(e) => {
                                            tracing::warn!(error = ?e, "reply_to send failed; preserving placeholder");
                                        }
                                    }
                                } else {
                                    let _ = adapter.send_message(&thread_channel, chunk).await;
                                }
                                first = false;
                            }
                            if !attachments.is_empty() {
                                match send_outbound_attachments(
                                    &adapter,
                                    &thread_channel,
                                    attachments,
                                    None,
                                    if chunks.is_empty() { Some(reply_id.as_str()) } else { None },
                                )
                                .await
                                {
                                    Ok(()) => {
                                        if chunks.is_empty() {
                                            send_ok = true;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = ?e, "outbound attachment send failed");
                                        if chunks.is_empty() {
                                            post.edit(&format!("⚠️ Failed to send attachment: {e}")).await;
                                        } else {
                                            let _ = adapter
                                                .send_message(
                                                    &thread_channel,
                                                    &format!("⚠️ Failed to send attachment: {e}"),
                                                )
                                                .await;
                                        }
                                    }
                                }
                            }
                            if send_ok {
                                post.delete_current_messages().await;
                            }
                        } else {
                            // Normal streaming: edit the placeholder post set into
                            // the final split content. No truncation: long content
                            // remains visible across multiple Discord messages.
                            if !final_content.is_empty() {
                                post.edit(&final_content).await;
                            }
                            if !attachments.is_empty() {
                                match send_outbound_attachments(
                                    &adapter,
                                    &thread_channel,
                                    attachments,
                                    None,
                                    None,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        if chunks.is_empty() {
                                            post.delete_current_messages().await;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = ?e, "outbound attachment send failed");
                                        if chunks.is_empty() {
                                            post.edit(&format!("⚠️ Failed to send attachment: {e}")).await;
                                        } else {
                                            let _ = adapter
                                                .send_message(
                                                    &thread_channel,
                                                    &format!("⚠️ Failed to send attachment: {e}"),
                                                )
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        if !attachments.is_empty() {
                            let first_content = chunks.first().map(String::as_str);
                            match send_outbound_attachments(
                                &adapter,
                                &thread_channel,
                                attachments,
                                first_content,
                                directives.reply_to.as_deref(),
                            )
                            .await
                            {
                                Ok(()) => {
                                    for chunk in chunks.iter().skip(1) {
                                        let _ = adapter.send_message(&thread_channel, chunk).await;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = ?e, "outbound attachment send failed");
                                    // Preserve the textual response even if attachment upload fails.
                                    let mut first = true;
                                    for chunk in &chunks {
                                        if first {
                                            if let Some(ref reply_id) = directives.reply_to {
                                                let _ = adapter
                                                    .send_message_with_reply(
                                                        &thread_channel,
                                                        chunk,
                                                        reply_id,
                                                    )
                                                    .await;
                                            } else {
                                                let _ = adapter
                                                    .send_message(&thread_channel, chunk)
                                                    .await;
                                            }
                                        } else {
                                            let _ = adapter.send_message(&thread_channel, chunk).await;
                                        }
                                        first = false;
                                    }
                                    let _ = adapter
                                        .send_message(
                                            &thread_channel,
                                            &format!("⚠️ Failed to send attachment: {e}"),
                                        )
                                        .await;
                                }
                            }
                        } else {
                            // Send-once: all chunks as new messages
                            // First chunk uses reply_to directive if present
                            let mut first = true;
                            for chunk in &chunks {
                                if first {
                                    if let Some(ref reply_id) = directives.reply_to {
                                        let _ = adapter.send_message_with_reply(
                                            &thread_channel,
                                            chunk,
                                            reply_id,
                                        ).await;
                                    } else {
                                        let _ = adapter.send_message(&thread_channel, chunk).await;
                                    }
                                } else {
                                    let _ = adapter.send_message(&thread_channel, chunk).await;
                                }
                                first = false;
                            }
                        }
                    }

                    Ok(())
                })
            })
            .await
    }
}

async fn send_outbound_attachments(
    adapter: &Arc<dyn ChatAdapter>,
    thread_channel: &ChannelRef,
    mut attachments: Vec<OutboundAttachment>,
    first_content: Option<&str>,
    reply_to_message_id: Option<&str>,
) -> Result<()> {
    const MAX_FILES_PER_MESSAGE: usize = 10;

    let mut batch_index = 0;
    while !attachments.is_empty() {
        let take = attachments.len().min(MAX_FILES_PER_MESSAGE);
        let batch: Vec<_> = attachments.drain(..take).collect();
        let content = if batch_index == 0 {
            first_content.unwrap_or("")
        } else {
            ""
        };
        let reply_to = if batch_index == 0 {
            reply_to_message_id
        } else {
            None
        };
        adapter
            .send_attachments(thread_channel, content, batch, reply_to)
            .await?;
        batch_index += 1;
    }
    Ok(())
}

fn split_streaming_display(content: &str, message_limit: usize) -> Vec<String> {
    if content.is_empty() {
        return vec!["\u{200b}".to_string()];
    }
    let chunks = format::split_message(content, message_limit);
    if chunks.is_empty() {
        vec!["\u{200b}".to_string()]
    } else {
        chunks
    }
}

fn append_text_chunk(text_buf: &mut String, chunk: &str, separate_response: bool) {
    if separate_response {
        ensure_response_separator(text_buf, chunk);
    } else if should_separate_stream_chunk(text_buf, chunk) {
        text_buf.push('\n');
    }
    text_buf.push_str(chunk);
}

fn should_separate_stream_chunk(text_buf: &str, chunk: &str) -> bool {
    if text_buf.is_empty() || chunk.is_empty() {
        return false;
    }
    if chunk.chars().next().is_some_and(char::is_whitespace)
        || text_buf
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        return false;
    }

    let Some(prev) = text_buf.trim_end().chars().next_back() else {
        return false;
    };
    let Some(next) = chunk.chars().next() else {
        return false;
    };

    if is_inside_inline_code_span(text_buf) {
        return false;
    }

    is_sentence_terminal(prev) && is_text_start(next)
}

fn is_inside_inline_code_span(text: &str) -> bool {
    let line = text.rsplit('\n').next().unwrap_or(text);
    let mut open_tick_run: Option<usize> = None;
    let mut chars = line.chars().peekable();
    let mut preceding_backslashes = 0usize;

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            preceding_backslashes += 1;
            continue;
        }

        if ch == '`' && preceding_backslashes.is_multiple_of(2) {
            let mut len = 1usize;
            while chars.peek() == Some(&'`') {
                chars.next();
                len += 1;
            }
            match open_tick_run {
                Some(open_len) if open_len == len => open_tick_run = None,
                None => open_tick_run = Some(len),
                _ => {}
            }
        }
        preceding_backslashes = 0;
    }

    open_tick_run.is_some()
}

fn is_sentence_terminal(ch: char) -> bool {
    matches!(
        ch,
        '.' | '!' | '?' | ':' | ';' | '。' | '！' | '？' | '：' | '；'
    )
}

fn is_text_start(ch: char) -> bool {
    ch.is_alphanumeric()
}

fn ensure_response_separator(text_buf: &mut String, next_chunk: &str) {
    if text_buf.is_empty() || next_chunk.starts_with('\n') || text_buf.ends_with("\n\n") {
        return;
    }
    if text_buf.ends_with('\n') {
        text_buf.push('\n');
    } else {
        text_buf.push_str("\n\n");
    }
}

/// Flatten a tool-call title into a single line safe for inline-code spans.
fn sanitize_title(title: &str) -> String {
    title
        .replace('\r', "")
        .replace('\n', " ; ")
        .replace('`', "'")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolState {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
struct ToolEntry {
    id: String,
    title: String,
    state: ToolState,
}

impl ToolEntry {
    fn render(&self) -> String {
        let icon = match self.state {
            ToolState::Running => "🔧",
            ToolState::Completed => "✅",
            ToolState::Failed => "❌",
        };
        let suffix = if self.state == ToolState::Running {
            "..."
        } else {
            ""
        };
        format!("{icon} `{}`{}", self.title, suffix)
    }
}

/// Maximum number of finished tool entries to show individually
/// during streaming before collapsing into a summary line.
const TOOL_COLLAPSE_THRESHOLD: usize = 3;

fn compose_display(
    tool_lines: &[ToolEntry],
    text: &str,
    streaming: bool,
    tool_display: ToolDisplay,
) -> String {
    let mut out = String::new();
    if !tool_lines.is_empty() && tool_display != ToolDisplay::None {
        let done = tool_lines
            .iter()
            .filter(|e| e.state == ToolState::Completed)
            .count();
        let failed = tool_lines
            .iter()
            .filter(|e| e.state == ToolState::Failed)
            .count();
        let running = tool_lines
            .iter()
            .filter(|e| e.state == ToolState::Running)
            .count();
        let finished = done + failed;

        match tool_display {
            ToolDisplay::Compact => {
                // Always show count summary, never per-tool details
                let mut parts = Vec::new();
                if done > 0 {
                    parts.push(format!("✅ {done}"));
                }
                if failed > 0 {
                    parts.push(format!("❌ {failed}"));
                }
                if running > 0 {
                    parts.push(format!("🔧 {running}"));
                }
                if !parts.is_empty() {
                    out.push_str(&format!("{} tool(s)\n", parts.join(" · ")));
                }
            }
            ToolDisplay::Full => {
                if streaming {
                    let running_entries: Vec<_> = tool_lines
                        .iter()
                        .filter(|e| e.state == ToolState::Running)
                        .collect();

                    if finished <= TOOL_COLLAPSE_THRESHOLD {
                        for entry in tool_lines.iter().filter(|e| e.state != ToolState::Running) {
                            out.push_str(&entry.render());
                            out.push('\n');
                        }
                    } else {
                        let mut parts = Vec::new();
                        if done > 0 {
                            parts.push(format!("✅ {done}"));
                        }
                        if failed > 0 {
                            parts.push(format!("❌ {failed}"));
                        }
                        out.push_str(&format!("{} tool(s) completed\n", parts.join(" · ")));
                    }

                    if running_entries.len() <= TOOL_COLLAPSE_THRESHOLD {
                        for entry in &running_entries {
                            out.push_str(&entry.render());
                            out.push('\n');
                        }
                    } else {
                        let hidden = running_entries.len() - TOOL_COLLAPSE_THRESHOLD;
                        out.push_str(&format!("🔧 {hidden} more running\n"));
                        for entry in running_entries.iter().skip(hidden) {
                            out.push_str(&entry.render());
                            out.push('\n');
                        }
                    }
                } else {
                    for entry in tool_lines {
                        out.push_str(&entry.render());
                        out.push('\n');
                    }
                }
            }
            ToolDisplay::None => {} // guarded above, but safe no-op
        }
        if !out.is_empty() {
            out.push('\n');
        }
    }
    out.push_str(text.trim_end());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time regression guard: use_streaming() is a required trait method
    /// (no default). Any adapter that forgets to implement it will fail to compile.
    /// This test documents the contract — see PR #503 / issue #502 for context.
    #[test]
    fn use_streaming_is_required_method() {
        // If use_streaming() had a default impl, this test module would still
        // compile even if an adapter forgot to override it. The real guard is
        // the trait definition itself — this test exists as documentation and
        // to catch if someone re-adds a default.
        struct TestAdapter;

        #[async_trait]
        impl ChatAdapter for TestAdapter {
            fn platform(&self) -> &'static str {
                "test"
            }
            fn message_limit(&self) -> usize {
                2000
            }
            async fn send_message(&self, _: &ChannelRef, _: &str) -> Result<MessageRef> {
                unimplemented!()
            }
            async fn create_thread(
                &self,
                _: &ChannelRef,
                _: &MessageRef,
                _: &str,
            ) -> Result<ChannelRef> {
                unimplemented!()
            }
            async fn add_reaction(&self, _: &MessageRef, _: &str) -> Result<()> {
                Ok(())
            }
            async fn remove_reaction(&self, _: &MessageRef, _: &str) -> Result<()> {
                Ok(())
            }
            // use_streaming() MUST be declared — removing this line should fail compilation
            fn use_streaming(&self, _other_bot_present: bool) -> bool {
                false
            }
        }

        let adapter = TestAdapter;
        // Verify the method is callable and returns the declared value
        assert!(!adapter.use_streaming(false));
    }

    #[test]
    fn origin_event_id_excluded_from_eq() {
        let a = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_aaa".into()),
        };
        let b = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_bbb".into()),
        };
        assert_eq!(a, b, "same channel with different event IDs must be equal");
    }

    #[test]
    fn origin_event_id_excluded_from_hash() {
        use std::collections::HashMap;
        let a = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_aaa".into()),
        };
        let b = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_bbb".into()),
        };
        let mut map = HashMap::new();
        map.insert(a, "first");
        // b should hit the same bucket and overwrite
        map.insert(b, "second");
        assert_eq!(map.len(), 1);
        assert_eq!(map.values().next(), Some(&"second"));
    }

    #[test]
    fn origin_event_id_survives_clone() {
        let ch = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_abc".into()),
        };
        // Simulates create_thread propagation: clone preserves origin_event_id
        let thread_ch = ChannelRef {
            thread_id: Some("topic_1".into()),
            origin_event_id: ch.origin_event_id.clone(),
            ..ch.clone()
        };
        assert_eq!(thread_ch.origin_event_id.as_deref(), Some("evt_abc"));
    }

    fn tool(id: &str, title: &str, state: ToolState) -> ToolEntry {
        ToolEntry {
            id: id.into(),
            title: title.into(),
            state,
        }
    }

    #[test]
    fn compose_display_full_shows_complete_title() {
        let tools = vec![tool(
            "1",
            "curl -s https://example.com",
            ToolState::Completed,
        )];
        let out = compose_display(&tools, "done", false, ToolDisplay::Full);
        assert!(out.contains("`curl -s https://example.com`"));
    }

    #[test]
    fn compose_display_compact_shows_count_summary() {
        let tools = vec![
            tool("1", "curl -s https://example.com", ToolState::Completed),
            tool("2", "grep -r pattern src/", ToolState::Completed),
            tool("3", "cat /etc/hosts", ToolState::Failed),
        ];
        let out = compose_display(&tools, "done", false, ToolDisplay::Compact);
        assert!(out.contains("✅ 2"), "expected completed count: {out}");
        assert!(out.contains("❌ 1"), "expected failed count: {out}");
        assert!(out.contains("tool(s)"), "expected tool(s) label: {out}");
        // Must NOT contain individual tool names
        assert!(!out.contains("curl"), "should not show tool names: {out}");
        assert!(!out.contains("grep"), "should not show tool names: {out}");
    }

    #[test]
    fn compose_display_compact_shows_running_count() {
        let tools = vec![
            tool("1", "curl", ToolState::Completed),
            tool("2", "npm install", ToolState::Running),
        ];
        let out = compose_display(&tools, "", true, ToolDisplay::Compact);
        assert!(out.contains("✅ 1"), "expected completed count: {out}");
        assert!(out.contains("🔧 1"), "expected running count: {out}");
    }

    #[test]
    fn compose_display_none_hides_tools() {
        let tools = vec![tool(
            "1",
            "curl -s https://example.com",
            ToolState::Completed,
        )];
        let out = compose_display(&tools, "response text", false, ToolDisplay::None);
        assert_eq!(out, "response text");
    }

    #[test]
    fn append_text_chunk_separates_steered_response() {
        let mut text = "first response".to_string();
        append_text_chunk(&mut text, "second response", true);
        assert_eq!(text, "first response\n\nsecond response");
    }

    #[test]
    fn append_text_chunk_does_not_separate_normal_stream_delta() {
        let mut text = "first".to_string();
        append_text_chunk(&mut text, " response", false);
        assert_eq!(text, "first response");
    }

    #[test]
    fn append_text_chunk_separates_standalone_stream_comments() {
        let mut text = "調査する。".to_string();
        append_text_chunk(&mut text, "次に確認する。", false);
        assert_eq!(text, "調査する。\n次に確認する。");
    }

    #[test]
    fn append_text_chunk_does_not_separate_inside_inline_code() {
        let mut text = "Use `foo.".to_string();
        append_text_chunk(&mut text, "bar` now", false);
        assert_eq!(text, "Use `foo.bar` now");
    }

    #[test]
    fn append_text_chunk_does_not_separate_inside_multi_tick_inline_code() {
        let mut text = "Use ``foo.".to_string();
        append_text_chunk(&mut text, "bar`` now", false);
        assert_eq!(text, "Use ``foo.bar`` now");
    }

    #[test]
    fn append_text_chunk_separates_after_closed_inline_code() {
        let mut text = "Use `foo`.".to_string();
        append_text_chunk(&mut text, "Next step.", false);
        assert_eq!(text, "Use `foo`.\nNext step.");
    }

    #[test]
    fn append_text_chunk_preserves_mid_sentence_stream_delta() {
        let mut text = "調査".to_string();
        append_text_chunk(&mut text, "します。", false);
        assert_eq!(text, "調査します。");
    }

    #[test]
    fn append_text_chunk_preserves_chunk_leading_newline() {
        let mut text = "first response".to_string();
        append_text_chunk(&mut text, "\nsecond response", true);
        assert_eq!(text, "first response\nsecond response");
    }

    #[test]
    fn split_streaming_display_splits_without_truncating() {
        let text = "a".repeat(4500);
        let chunks = split_streaming_display(&text, 2000);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), text);
        assert!(
            chunks.iter().all(|chunk| chunk.chars().count() <= 2000),
            "all streaming chunks must respect platform limit"
        );
    }

    #[test]
    fn split_streaming_display_empty_uses_zero_width_space() {
        let chunks = split_streaming_display("", 2000);
        assert_eq!(chunks, vec!["\u{200b}".to_string()]);
    }

    fn test_png_bytes() -> Vec<u8> {
        let img = image::RgbImage::new(1, 1);
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn outbound_attachments_loads_image_from_allowed_dir() {
        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("generated.png");
        std::fs::write(&image_path, test_png_bytes()).unwrap();
        let cfg = AttachmentsConfig {
            enabled: true,
            allowed_dirs: vec![dir.path().to_string_lossy().to_string()],
            max_bytes: 1024 * 1024,
            max_files: 10,
        };
        let outbound = OutboundAttachments::new(cfg, dir.path().to_string_lossy().to_string());

        let (files, warnings) = outbound
            .load_images(&[image_path.to_string_lossy().to_string()])
            .await;

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "generated.png");
        assert!(!files[0].data.is_empty());
    }

    #[tokio::test]
    async fn outbound_attachments_rejects_path_outside_allowed_dir() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let image_path = outside.path().join("secret.png");
        std::fs::write(&image_path, test_png_bytes()).unwrap();
        let cfg = AttachmentsConfig {
            enabled: true,
            allowed_dirs: vec![allowed.path().to_string_lossy().to_string()],
            max_bytes: 1024 * 1024,
            max_files: 10,
        };
        let outbound = OutboundAttachments::new(cfg, allowed.path().to_string_lossy().to_string());

        let (files, warnings) = outbound
            .load_images(&[image_path.to_string_lossy().to_string()])
            .await;

        assert!(files.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("outside attachments.allowed_dirs"));
    }

    #[tokio::test]
    async fn outbound_attachments_relative_path_uses_agent_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("relative.png"), test_png_bytes()).unwrap();
        let cfg = AttachmentsConfig {
            enabled: true,
            allowed_dirs: Vec::new(),
            max_bytes: 1024 * 1024,
            max_files: 10,
        };
        let outbound = OutboundAttachments::new(cfg, dir.path().to_string_lossy().to_string());

        let (files, warnings) = outbound.load_images(&["relative.png".to_string()]).await;

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "relative.png");
    }

    #[tokio::test]
    async fn outbound_attachments_disabled_warns_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AttachmentsConfig {
            enabled: false,
            allowed_dirs: vec![dir.path().to_string_lossy().to_string()],
            max_bytes: 1024 * 1024,
            max_files: 10,
        };
        let outbound = OutboundAttachments::new(cfg, dir.path().to_string_lossy().to_string());

        let (files, warnings) = outbound.load_images(&["missing.png".to_string()]).await;

        assert!(files.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("enabled is false"));
    }
}

#[cfg(test)]
mod directive_tests {
    use super::parse_output_directives;

    #[test]
    fn parse_reply_to_directive() {
        let input = "[[reply_to:1502606076451885136]]\nHello world";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1502606076451885136".to_string()));
        assert_eq!(content, "Hello world");
    }

    #[test]
    fn parse_no_directives() {
        let input = "Just plain content\nwith multiple lines";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, input);
    }

    #[test]
    fn parse_multiple_directives() {
        let input = "[[reply_to:123456]]\n[[unknown_key:value]]\nContent here";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("123456".to_string()));
        assert!(directives.attach_images.is_empty());
        assert_eq!(content, "Content here");
    }

    #[test]
    fn parse_attach_image_directive() {
        let input = "[[attach_image:/home/node/.codex/generated_images/out.png]]\nDone";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(
            directives.attach_images,
            vec!["/home/node/.codex/generated_images/out.png".to_string()]
        );
        assert_eq!(content, "Done");
    }

    #[test]
    fn parse_multiple_attach_image_directives() {
        let input = "[[attach_image:one.png]]\n[[attach_image:two.webp]]\nDone";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.attach_images, vec!["one.png", "two.webp"]);
        assert_eq!(content, "Done");
    }

    #[test]
    fn parse_attach_image_rejects_empty_path() {
        let input = "[[attach_image:]]\nDone";
        let (directives, content) = parse_output_directives(input);
        assert!(directives.attach_images.is_empty());
        assert_eq!(content, "Done");
    }

    #[test]
    fn parse_invalid_reply_to_rejects_whitespace() {
        let input = "[[reply_to:has spaces]]\nContent";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, "Content");
    }

    #[test]
    fn parse_slack_ts_format_accepted() {
        let input = "[[reply_to:1234567890.123456]]\nContent";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1234567890.123456".to_string()));
        assert_eq!(content, "Content");
    }

    #[test]
    fn parse_empty_reply_to() {
        let input = "[[reply_to:]]\nContent";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, "Content");
    }

    #[test]
    fn parse_crlf_line_endings() {
        let input = "[[reply_to:999]]\r\nContent with CRLF";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("999".to_string()));
        assert_eq!(content, "Content with CRLF");
    }

    #[test]
    fn parse_directive_only_no_content() {
        let input = "[[reply_to:123]]";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("123".to_string()));
        assert_eq!(content, "");
    }

    #[test]
    fn parse_non_directive_line_stops_parsing() {
        let input = "Normal first line\n[[reply_to:123]]\nMore content";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, input);
    }

    #[test]
    fn parse_duplicate_reply_to_last_wins() {
        let input = "[[reply_to:111]]\n[[reply_to:222]]\nContent";
        let (directives, content) = parse_output_directives(input);
        // Last value wins
        assert_eq!(directives.reply_to, Some("222".to_string()));
        assert_eq!(content, "Content");
    }

    #[test]
    fn parse_crlf_multiple_directives() {
        let input = "[[reply_to:456]]\r\n[[unknown:x]]\r\nContent after CRLF";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("456".to_string()));
        assert_eq!(content, "Content after CRLF");
    }

    #[test]
    fn parse_bracket_without_colon_preserved() {
        // [[Note]] has no colon — not a directive, preserved as content
        let input = "[[Summary]]\nThis is body text";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, input);
    }

    #[test]
    fn parse_reply_to_with_inline_content() {
        // Agent puts content on same line as directive — should still parse
        let input = "[[reply_to:1502724086474870926]]  @BOT I'm on standby";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1502724086474870926".to_string()));
        assert_eq!(content, "@BOT I'm on standby");
    }

    #[test]
    fn parse_reply_to_inline_with_more_lines() {
        let input = "[[reply_to:123]]  First line\nSecond line\nThird line";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("123".to_string()));
        assert_eq!(content, "First line\nSecond line\nThird line");
    }

    #[test]
    fn parse_reply_to_no_space_before_content() {
        // No space between ]] and content
        let input = "[[reply_to:1502724086474870926]]收到";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1502724086474870926".to_string()));
        assert_eq!(content, "收到");
    }

    #[test]
    fn parse_reply_to_inline_with_mention() {
        // Real-world case: directive followed by Discord mention
        let input = "[[reply_to:1502724086474870926]]  <@1490365068863606784> 我 standby";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1502724086474870926".to_string()));
        assert_eq!(content, "<@1490365068863606784> 我 standby");
    }

    #[test]
    fn parse_reply_to_inline_only_spaces() {
        // Trailing spaces only — no real content, should be empty
        let input = "[[reply_to:123]]   ";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("123".to_string()));
        assert_eq!(content, "");
    }

    #[test]
    fn parse_reply_to_with_brackets_in_content() {
        // Content after ]] contains brackets — should not confuse parser
        let input = "[[reply_to:456]]  看看 [[這個]] 怎麼樣";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("456".to_string()));
        assert_eq!(content, "看看 [[這個]] 怎麼樣");
    }
}

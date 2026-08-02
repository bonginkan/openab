use crate::acp::protocol::ConfigOption;
use crate::acp::ContentBlock;
use crate::adapter::{
    AdapterRouter, ChannelRef, ChatAdapter, MessageRef, OutboundAttachment, SenderContext,
};
use crate::bot_turns::{BotTurnTracker, TurnAction, TurnSeverity, BOT_TURN_LIMIT_WARNING_PREFIX};
use crate::config::{AllowBots, AllowUsers, ContextRecoveryConfig, SttConfig};
use crate::context_recovery::{
    discord_message_links, ContextCollector, DiscordMessageLink, RecoveredContext, RecoveredMessage,
};
use crate::format;
use crate::ingress_audit::{IngressAuditGuard, IngressAuditRecord, IngressDecision};
use crate::media;
use crate::remind::{self, ReminderStore};
use async_trait::async_trait;
use serenity::builder::{
    CreateActionRow, CreateAttachment, CreateButton, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, CreateThread, EditMessage,
    GetMessages,
};
use serenity::http::Http;
use serenity::model::application::ButtonStyle;
use serenity::model::application::{
    Command, CommandOptionType, ComponentInteractionDataKind, Interaction,
};
use serenity::model::channel::{AutoArchiveDuration, Message, MessageType, ReactionType};
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId, UserId};
use serenity::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::sync::{Arc, OnceLock};
use tracing::{debug, error, info, warn};

/// Hard cap on consecutive bot messages in a channel or thread.
/// Prevents runaway loops between multiple bots in "all" mode.
const MAX_CONSECUTIVE_BOT_TURNS: u32 = 1000;

/// Maximum entries in the participation cache before eviction.
const PARTICIPATION_CACHE_MAX: usize = 1000;

/// Discord StringSelectMenu hard limit on options.
const SELECT_MENU_PAGE_SIZE: usize = 25;

/// Avoid unbounded Discord history exports from very large threads.
const THREAD_EXPORT_MESSAGE_LIMIT: usize = 5000;

fn cache_participation_entry(
    cache: &mut HashMap<String, tokio::time::Instant>,
    key: String,
    session_ttl: std::time::Duration,
) {
    cache.insert(key, tokio::time::Instant::now());

    if cache.len() > PARTICIPATION_CACHE_MAX {
        cache.retain(|_, ts| ts.elapsed() < session_ttl);
        if cache.len() > PARTICIPATION_CACHE_MAX {
            let mut entries: Vec<_> = cache.iter().map(|(k, v)| (k.clone(), *v)).collect();
            entries.sort_by_key(|(_, ts)| *ts);
            let evict_count = entries.len() / 2;
            for (k, _) in entries.into_iter().take(evict_count) {
                cache.remove(&k);
            }
        }
    }
}

// --- DiscordAdapter: implements ChatAdapter for Discord via serenity ---

pub struct DiscordAdapter {
    http: Arc<Http>,
}

impl DiscordAdapter {
    pub fn new(http: Arc<Http>) -> Self {
        Self { http }
    }

    /// Resolve the effective Discord channel ID from a ChannelRef.
    /// Discord threads are channels, so prefer thread_id when set.
    fn resolve_channel(channel: &ChannelRef) -> &str {
        channel.thread_id.as_deref().unwrap_or(&channel.channel_id)
    }
}

#[async_trait]
impl ChatAdapter for DiscordAdapter {
    fn platform(&self) -> &'static str {
        "discord"
    }

    fn message_limit(&self) -> usize {
        2000
    }

    async fn send_message(
        &self,
        channel: &ChannelRef,
        content: &str,
    ) -> anyhow::Result<MessageRef> {
        let ch_id: u64 = Self::resolve_channel(channel).parse()?;
        let msg = ChannelId::new(ch_id).say(&self.http, content).await?;
        Ok(MessageRef {
            channel: channel.clone(),
            message_id: msg.id.to_string(),
        })
    }

    async fn send_message_with_reply(
        &self,
        channel: &ChannelRef,
        content: &str,
        reply_to_message_id: &str,
    ) -> anyhow::Result<MessageRef> {
        let ch_id: u64 = Self::resolve_channel(channel).parse()?;
        let msg_id: u64 = reply_to_message_id.parse().unwrap_or(0);
        if msg_id == 0 {
            // Invalid message ID, fall back to plain send
            return self.send_message(channel, content).await;
        }
        let builder = serenity::builder::CreateMessage::new()
            .content(content)
            .reference_message((ChannelId::new(ch_id), MessageId::new(msg_id)));
        match ChannelId::new(ch_id)
            .send_message(&self.http, builder)
            .await
        {
            Ok(msg) => Ok(MessageRef {
                channel: channel.clone(),
                message_id: msg.id.to_string(),
            }),
            Err(e) => {
                // Fallback to plain send if reply fails (e.g. unknown message, cross-channel)
                tracing::warn!(error = ?e, reply_to = reply_to_message_id, "reply_to failed, falling back to plain send");
                self.send_message(channel, content).await
            }
        }
    }

    async fn send_attachments(
        &self,
        channel: &ChannelRef,
        content: &str,
        attachments: Vec<OutboundAttachment>,
        reply_to_message_id: Option<&str>,
    ) -> anyhow::Result<MessageRef> {
        let ch_id: u64 = Self::resolve_channel(channel).parse()?;
        let files = attachments
            .into_iter()
            .map(|file| CreateAttachment::bytes(file.data, file.filename));
        let mut builder = serenity::builder::CreateMessage::new().add_files(files);
        if !content.is_empty() {
            builder = builder.content(content);
        }
        if let Some(reply_to_message_id) = reply_to_message_id {
            if let Ok(msg_id) = reply_to_message_id.parse::<u64>() {
                builder =
                    builder.reference_message((ChannelId::new(ch_id), MessageId::new(msg_id)));
            }
        }

        let msg = ChannelId::new(ch_id)
            .send_message(&self.http, builder)
            .await?;
        Ok(MessageRef {
            channel: channel.clone(),
            message_id: msg.id.to_string(),
        })
    }

    async fn delete_message(&self, msg: &MessageRef) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(&msg.channel).parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .delete_message(ChannelId::new(ch_id), MessageId::new(msg_id), None)
            .await?;
        Ok(())
    }

    async fn edit_message(&self, msg: &MessageRef, content: &str) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(&msg.channel).parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        ChannelId::new(ch_id)
            .edit_message(
                &self.http,
                MessageId::new(msg_id),
                EditMessage::new().content(content),
            )
            .await?;
        Ok(())
    }

    fn use_streaming(&self, other_bot_present: bool) -> bool {
        !other_bot_present
    }

    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        title: &str,
    ) -> anyhow::Result<ChannelRef> {
        let ch_id: u64 = channel.channel_id.parse()?;
        let msg_id: u64 = trigger_msg.message_id.parse()?;
        let thread = ChannelId::new(ch_id)
            .create_thread_from_message(
                &self.http,
                MessageId::new(msg_id),
                CreateThread::new(title).auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await?;
        Ok(ChannelRef {
            platform: "discord".into(),
            channel_id: thread.id.to_string(),
            thread_id: None,
            parent_id: Some(channel.channel_id.clone()),
            origin_event_id: None,
        })
    }

    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(&msg.channel).parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .create_reaction(
                ChannelId::new(ch_id),
                MessageId::new(msg_id),
                &ReactionType::Unicode(emoji.to_string()),
            )
            .await?;
        Ok(())
    }

    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(&msg.channel).parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .delete_reaction_me(
                ChannelId::new(ch_id),
                MessageId::new(msg_id),
                &ReactionType::Unicode(emoji.to_string()),
            )
            .await?;
        Ok(())
    }
}

// --- Handler: serenity EventHandler that delegates to AdapterRouter ---

pub struct Handler {
    pub router: Arc<AdapterRouter>,
    pub allow_all_guilds: bool,
    pub allow_all_channels: bool,
    pub allow_all_users: bool,
    pub allowed_guilds: HashSet<u64>,
    pub allowed_channels: HashSet<u64>,
    pub allowed_users: HashSet<u64>,
    pub stt_config: SttConfig,
    pub context_recovery: ContextRecoveryConfig,
    pub inbound_attachment_content_blocks: bool,
    pub adapter: OnceLock<Arc<dyn ChatAdapter>>,
    pub allow_bot_messages: AllowBots,
    pub trusted_bot_ids: HashSet<u64>,
    pub allow_user_messages: AllowUsers,
    /// Role IDs that trigger the bot (same as direct @mention).
    pub allowed_role_ids: HashSet<u64>,
    /// Positive-only cache: thread channel_id → cached_at for threads where bot has participated.
    pub participated_threads: tokio::sync::Mutex<HashMap<String, tokio::time::Instant>>,
    /// Positive-only cache: thread channel_id → cached_at for threads where other bots have posted.
    /// Like participation, a thread becoming multi-bot is irreversible (bot messages don't disappear).
    pub multibot_threads: tokio::sync::Mutex<HashMap<String, tokio::time::Instant>>,
    /// TTL for participation cache entries (from pool.session_ttl_hours).
    pub session_ttl: std::time::Duration,
    /// Configurable soft limit on bot turns per thread (reset by human message).
    pub max_bot_turns: u32,
    /// Per-thread bot turn tracker. Both counters reset on human msg.
    pub bot_turns: tokio::sync::Mutex<BotTurnTracker>,
    /// Allow the bot to respond to Discord DMs.
    pub allow_dm: bool,
    /// Per-thread dispatcher (Message mode uses cap=1 for FIFO; Thread/Lane use configured cap).
    pub dispatcher: Arc<crate::dispatch::Dispatcher>,
    /// Reminder store for /remind slash command.
    pub reminder_store: ReminderStore,
    /// Track scheduled reminder IDs to prevent duplicate scheduling on reconnect.
    pub scheduled_ids: tokio::sync::Mutex<std::collections::HashSet<String>>,
    /// Durable queue backlog. Replayed once after connect so a restart resumes
    /// whatever was still queued (see `spool.rs`). Shared with `dispatcher`.
    pub spool: Arc<crate::spool::QueueSpool>,
    /// Guards the one-time queue replay so a serenity reconnect (which re-fires
    /// `ready`) does not re-submit the backlog.
    pub replayed: std::sync::atomic::AtomicBool,
}

impl Handler {
    fn guild_allowed(&self, guild_id: Option<u64>) -> bool {
        is_allowed_guild(self.allow_all_guilds, &self.allowed_guilds, guild_id)
    }

    async fn reject_disallowed_guild_interaction(
        &self,
        ctx: &Context,
        interaction: &Interaction,
        guild_id: u64,
    ) {
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("⚠️ This bot is not enabled in this server.")
                .ephemeral(true),
        );

        match interaction {
            Interaction::Command(cmd) => {
                if let Err(e) = cmd.create_response(&ctx.http, response).await {
                    tracing::debug!(
                        guild_id,
                        error = %e,
                        "failed to reject command in disallowed guild"
                    );
                }
            }
            Interaction::Component(comp) => {
                if let Err(e) = comp.create_response(&ctx.http, response).await {
                    tracing::debug!(
                        guild_id,
                        error = %e,
                        "failed to reject component interaction in disallowed guild"
                    );
                }
            }
            _ => {}
        }
    }

    async fn mark_thread_participated(&self, channel_id: &str) {
        let mut cache = self.participated_threads.lock().await;
        cache_participation_entry(&mut cache, channel_id.to_string(), self.session_ttl);
    }

    /// Check if the bot has participated in a Discord thread, and whether
    /// other bots have also posted in it.
    /// Returns `(involved, other_bot_present)`.
    /// Fail-closed: returns `(false, false)` on API error.
    /// Caches positive results only (both participation and multi-bot status are irreversible).
    async fn bot_participated_in_thread(
        &self,
        http: &Http,
        channel_id: ChannelId,
        bot_id: UserId,
    ) -> (bool, bool) {
        let key = channel_id.to_string();

        // Check positive caches
        let cached_involved = {
            let cache = self.participated_threads.lock().await;
            cache
                .get(&key)
                .is_some_and(|ts| ts.elapsed() < self.session_ttl)
        };
        let cached_multibot = {
            let cache = self.multibot_threads.lock().await;
            cache
                .get(&key)
                .is_some_and(|ts| ts.elapsed() < self.session_ttl)
        };

        // Both cached → skip fetch entirely
        // With early detection from msg.author, multibot_threads is populated
        // eagerly — no need to fetch just to check for other bots.
        if cached_involved {
            return (true, cached_multibot);
        }

        // Fetch recent messages
        let messages = match channel_id
            .messages(http, serenity::builder::GetMessages::new().limit(200))
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "failed to fetch thread messages for participation check, rejecting (fail-closed)"
                );
                return (false, false);
            }
        };

        let involved = cached_involved || messages.iter().any(|m| m.author.id == bot_id);
        let other_bot_present = cached_multibot
            || messages
                .iter()
                .any(|m| m.author.bot && m.author.id != bot_id);

        if involved && !cached_involved {
            let mut cache = self.participated_threads.lock().await;
            cache_participation_entry(&mut cache, key.clone(), self.session_ttl);
        }

        if other_bot_present && !cached_multibot {
            let mut cache = self.multibot_threads.lock().await;
            cache_participation_entry(&mut cache, key, self.session_ttl);
        }

        (involved, other_bot_present)
    }
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        let mut audit = IngressAuditGuard::new(IngressAuditRecord::new(
            "discord",
            msg.id.to_string(),
            msg.channel_id.to_string(),
            None,
            msg.guild_id.map(|id| id.to_string()),
            msg.author.id.to_string(),
            msg.author.bot,
            msg.timestamp.to_rfc3339(),
            "message",
            msg.content.chars().count(),
            msg.attachments.len(),
        ));
        let bot_id = ctx.cache.current_user().id;

        if !self.guild_allowed(msg.guild_id.map(|id| id.get())) {
            tracing::debug!(
                guild_id = ?msg.guild_id.map(|id| id.get()),
                channel_id = %msg.channel_id,
                "message ignored from disallowed guild"
            );
            audit.finish(IngressDecision::GuildDenied);
            return;
        }

        // Early multibot detection: cache that another bot is present.
        // Runs before self-check and bot gating so we always detect other bots. (#481)
        if msg.author.bot && msg.author.id != bot_id {
            let key = msg.channel_id.to_string();
            let mut cache = self.multibot_threads.lock().await;
            if cache
                .get(&key)
                .is_none_or(|ts| ts.elapsed() >= self.session_ttl)
            {
                cache_participation_entry(&mut cache, key, self.session_ttl);
            }
        }

        // Bot turn counting: runs before self-check so ALL bot messages
        // (including own) count toward the per-thread limit. This means
        // soft_limit=20 = 20 total bot messages in the thread (~10 per bot
        // in a two-bot ping-pong). (#483)
        {
            let thread_key = msg.channel_id.to_string();
            let mut tracker = self.bot_turns.lock().await;
            if msg.author.bot {
                match tracker.classify_bot_message(&thread_key) {
                    TurnAction::Continue => {}
                    TurnAction::SilentStop => {
                        audit.finish(IngressDecision::BotTurnLimit);
                        return;
                    }
                    TurnAction::WarnAndStop {
                        severity,
                        turns,
                        user_message,
                    } => {
                        match severity {
                            TurnSeverity::Hard => tracing::warn!(
                                channel_id = %msg.channel_id,
                                turns,
                                "hard bot turn limit reached",
                            ),
                            TurnSeverity::Soft => tracing::info!(
                                channel_id = %msg.channel_id,
                                turns,
                                max = self.max_bot_turns,
                                "soft bot turn limit reached",
                            ),
                        }
                        audit.finish(IngressDecision::BotTurnLimit);
                        // Only post the warning if this bot is allowed in the channel/thread.
                        // Bot turn counting intentionally runs before channel gating so ALL
                        // bot messages are counted, but the *warning message* must respect
                        // channel permissions — otherwise bots that never participated in a
                        // thread will spam it with warnings.
                        //
                        // Must match the full thread allowlist semantics: a thread is allowed
                        // if its own channel_id OR its parent_id is in allowed_channels.
                        let ch = msg.channel_id.get();
                        let in_allowed_channel = self.allowed_channels.contains(&ch);
                        let mut allowed_here = self.allow_all_channels || in_allowed_channel;
                        if !allowed_here {
                            // Reuse detect_thread() for thread allowlist semantics.
                            // Only called on the WarnAndStop path (once per soft/hard
                            // limit hit), not on every bot message.
                            if let Ok(serenity::model::channel::Channel::Guild(gc)) =
                                msg.channel_id.to_channel(&ctx.http).await
                            {
                                let (in_thread, _) = detect_thread(
                                    gc.thread_metadata.is_some(),
                                    gc.parent_id.map(|id| id.get()),
                                    gc.owner_id.map(|id| id.get()),
                                    bot_id.get(),
                                    &self.allowed_channels,
                                    self.allow_all_channels,
                                    in_allowed_channel,
                                );
                                if in_thread {
                                    allowed_here = true;
                                }
                            }
                        }
                        if msg.author.id != bot_id && allowed_here {
                            // Only warn if this bot actually participated in the
                            // thread — prevents uninvolved bots from spamming
                            // warnings in shared channels. (#727)
                            // Second value is `is_multibot`; not needed here.
                            let (participated, _) = self
                                .bot_participated_in_thread(&ctx.http, msg.channel_id, bot_id)
                                .await;
                            if participated {
                                // Dedup: skip if another bot already posted the same
                                // warning in this thread. Prevents N duplicate warnings
                                // when N bot processes each hit the soft limit. (#530)
                                let recent = msg
                                    .channel_id
                                    .messages(
                                        &ctx.http,
                                        serenity::builder::GetMessages::new().limit(10),
                                    )
                                    .await
                                    .unwrap_or_default();
                                let pairs: Vec<(bool, &str)> = recent
                                    .iter()
                                    .map(|m| (m.author.bot, m.content.as_str()))
                                    .collect();
                                let already_warned = turn_limit_warning_present(&pairs);
                                if !already_warned {
                                    let _ = msg.channel_id.say(&ctx.http, &user_message).await;
                                }
                            }
                        }
                        return;
                    }
                }
            } else if matches!(msg.kind, MessageType::Regular | MessageType::InlineReply)
                && !msg.content.is_empty()
            {
                tracker.on_human_message(&thread_key);
            }
        }

        // Ignore own messages (after counting toward bot turns above)
        if msg.author.id == bot_id {
            audit.finish(IngressDecision::SelfMessage);
            return;
        }

        let adapter = self
            .adapter
            .get_or_init(|| Arc::new(DiscordAdapter::new(ctx.http.clone())))
            .clone();

        let channel_id = msg.channel_id.get();
        let in_allowed_channel =
            self.allow_all_channels || self.allowed_channels.contains(&channel_id);

        let is_mentioned = msg.mentions_user_id(bot_id)
            || msg.content.contains(&format!("<@{}>", bot_id))
            || (!self.allowed_role_ids.is_empty()
                && msg
                    .mention_roles
                    .iter()
                    .any(|r| self.allowed_role_ids.contains(&r.get())));
        let is_reply_to_bot = !is_mentioned && references_bot_message(&ctx, &msg, bot_id).await;
        let is_user_trigger = is_mentioned || is_reply_to_bot;

        // Bot message gating (from upstream #321)
        if msg.author.bot {
            match self.allow_bot_messages {
                AllowBots::Off => {
                    audit.finish(IngressDecision::BotPolicyDenied);
                    return;
                }
                AllowBots::Mentions => {
                    if !is_mentioned {
                        audit.finish(IngressDecision::MentionRequired);
                        return;
                    }
                }
                AllowBots::All => {
                    let cap = MAX_CONSECUTIVE_BOT_TURNS as usize;
                    let limit = std::cmp::min(MAX_CONSECUTIVE_BOT_TURNS, 100) as u8;
                    let history = ctx
                        .cache
                        .channel_messages(msg.channel_id)
                        .map(|msgs| {
                            let mut recent: Vec<_> = msgs
                                .iter()
                                .filter(|(mid, _)| **mid < msg.id)
                                .map(|(_, m)| m.clone())
                                .collect();
                            recent.sort_unstable_by_key(|m| std::cmp::Reverse(m.id));
                            recent.truncate(cap);
                            recent
                        })
                        .filter(|msgs| !msgs.is_empty());

                    let recent = if let Some(cached) = history {
                        cached
                    } else {
                        match msg
                            .channel_id
                            .messages(
                                &ctx.http,
                                serenity::builder::GetMessages::new()
                                    .before(msg.id)
                                    .limit(limit),
                            )
                            .await
                        {
                            Ok(msgs) => msgs,
                            Err(e) => {
                                tracing::warn!(channel_id = %msg.channel_id, error = %e, "failed to fetch history for bot turn cap, rejecting (fail-closed)");
                                audit.finish(IngressDecision::LoopCheckFailed);
                                return;
                            }
                        }
                    };

                    let consecutive_bot = recent
                        .iter()
                        .take_while(|m| m.author.bot && m.author.id != bot_id)
                        .count();
                    if consecutive_bot >= cap {
                        tracing::warn!(channel_id = %msg.channel_id, cap, "bot turn cap reached, ignoring");
                        audit.finish(IngressDecision::BotTurnLimit);
                        return;
                    }
                }
            }

            if !self.trusted_bot_ids.is_empty()
                && !self.trusted_bot_ids.contains(&msg.author.id.get())
            {
                tracing::debug!(bot_id = %msg.author.id, "bot not in trusted_bot_ids, ignoring");
                audit.finish(IngressDecision::BotUntrusted);
                return;
            }
        }

        // Thread detection: single to_channel() call for both allowed and
        // non-allowed channels. Uses thread_metadata (not parent_id) to
        // identify threads — see detect_thread() doc comments for rationale.
        let (in_thread, bot_owns_thread, thread_parent_id, is_dm) = match msg
            .channel_id
            .to_channel(&ctx.http)
            .await
        {
            Ok(serenity::model::channel::Channel::Guild(gc)) => {
                let parent = gc.parent_id.map(|id| id.get().to_string());
                let result = detect_thread(
                    gc.thread_metadata.is_some(),
                    gc.parent_id.map(|id| id.get()),
                    gc.owner_id.map(|id| id.get()),
                    bot_id.get(),
                    &self.allowed_channels,
                    self.allow_all_channels,
                    in_allowed_channel,
                );
                tracing::debug!(
                    channel_id = %msg.channel_id,
                    parent_id = ?gc.parent_id,
                    owner_id = ?gc.owner_id,
                    has_thread_metadata = gc.thread_metadata.is_some(),
                    in_thread = result.0,
                    bot_owns = ?result.1,
                    "thread check"
                );
                (
                    result.0,
                    result.1.unwrap_or(false),
                    if result.0 { parent } else { None },
                    false,
                )
            }
            Ok(serenity::model::channel::Channel::Private(_)) => {
                tracing::debug!(channel_id = %msg.channel_id, "DM channel");
                (false, false, None, true)
            }
            Ok(other) => {
                tracing::debug!(channel_id = %msg.channel_id, kind = ?other, "not a guild thread");
                (false, false, None, false)
            }
            Err(e) => {
                tracing::debug!(channel_id = %msg.channel_id, error = %e, "to_channel failed");
                (false, false, None, false)
            }
        };

        // DM gating: allow_dm must be true, otherwise reject
        if is_dm && !self.allow_dm {
            tracing::debug!(channel_id = %msg.channel_id, "DM rejected (allow_dm=false)");
            audit.finish(IngressDecision::ChannelDenied);
            return;
        }

        if !is_dm && !in_allowed_channel && !in_thread {
            audit.finish(IngressDecision::ChannelDenied);
            return;
        }

        // User message gating (mirrors Slack's AllowUsers logic).
        // Mentions: always require @mention, even in bot's own threads.
        // Involved (default): skip @mention if the bot owns the thread
        //   (Option A) OR has previously posted in it (Option B).
        // MultibotMentions: same as Involved, but if other bots are also
        //   in the thread, require @mention to avoid all bots responding.
        // DMs are treated as implicit @mention (mirrors Slack behavior).
        if !is_user_trigger && !is_dm {
            match self.allow_user_messages {
                AllowUsers::Mentions => {
                    audit.finish(IngressDecision::MentionRequired);
                    return;
                }
                AllowUsers::Involved => {
                    if !in_thread {
                        audit.finish(IngressDecision::ThreadRequired);
                        return;
                    }
                    let (involved, _) = if bot_owns_thread {
                        (true, false) // other_bot_present not needed for Involved mode
                    } else {
                        self.bot_participated_in_thread(&ctx.http, msg.channel_id, bot_id)
                            .await
                    };
                    if !involved {
                        tracing::debug!(channel_id = %msg.channel_id, "bot not involved in thread, ignoring");
                        audit.finish(IngressDecision::BotNotInvolved);
                        return;
                    }
                }
                AllowUsers::MultibotMentions => {
                    if !in_thread {
                        audit.finish(IngressDecision::ThreadRequired);
                        return;
                    }
                    let (involved, other_bot) = if bot_owns_thread {
                        // Still need to check for other bots
                        let (_, other) = self
                            .bot_participated_in_thread(&ctx.http, msg.channel_id, bot_id)
                            .await;
                        (true, other)
                    } else {
                        self.bot_participated_in_thread(&ctx.http, msg.channel_id, bot_id)
                            .await
                    };
                    if !involved {
                        tracing::debug!(channel_id = %msg.channel_id, "bot not involved in thread, ignoring");
                        audit.finish(IngressDecision::BotNotInvolved);
                        return;
                    }
                    if other_bot {
                        tracing::debug!(channel_id = %msg.channel_id, "multi-bot thread, requiring @mention");
                        audit.finish(IngressDecision::MultiBotMentionRequired);
                        return;
                    }
                }
            }
        }

        if is_denied_user(
            msg.author.bot,
            self.allow_all_users,
            &self.allowed_users,
            msg.author.id.get(),
        ) {
            tracing::info!(user_id = %msg.author.id, "denied user, ignoring");
            audit.finish(IngressDecision::UserDenied);
            let msg_ref = discord_msg_ref(&msg);
            let _ = adapter.add_reaction(&msg_ref, "🚫").await;
            return;
        }

        let prompt = resolve_mentions(&msg.content, bot_id, &self.allowed_role_ids);

        // No dispatchable text/content → skip.
        if prompt.is_empty()
            && (msg.attachments.is_empty() || !self.inbound_attachment_content_blocks)
        {
            audit.finish(IngressDecision::EmptyContent);
            return;
        }

        let display_name = msg
            .member
            .as_ref()
            .and_then(|m| m.nick.as_ref())
            .or(msg.author.global_name.as_ref())
            .unwrap_or(&msg.author.name);
        let sender = build_sender_context(
            &msg.author.id.to_string(),
            &msg.author.name,
            display_name,
            &msg.channel_id.to_string(),
            thread_parent_id.as_deref(),
            msg.author.bot,
            &msg.timestamp.to_rfc3339().unwrap_or_default(),
            &msg.id.to_string(),
            &bot_id.to_string(),
        );

        // Build extra content blocks from attachments when enabled (audio -> STT,
        // text -> inline, image -> encode, video -> URL for agent-side inspection).
        let mut extra_blocks = Vec::new();
        let mut echo_entries: Vec<crate::stt::EchoEntry> = Vec::new();
        let mut failed_image_files: Vec<String> = Vec::new();
        let mut text_file_count: u32 = 0;
        const TEXT_FILE_COUNT_CAP: u32 = 5;

        if self.inbound_attachment_content_blocks {
            for attachment in &msg.attachments {
                let mime = attachment.content_type.as_deref().unwrap_or("");
                if media::is_audio_mime(mime) {
                    if self.stt_config.enabled {
                        let mime_clean = mime.split(';').next().unwrap_or(mime).trim();
                        match media::download_and_transcribe(
                            &attachment.url,
                            &attachment.filename,
                            mime_clean,
                            u64::from(attachment.size),
                            &self.stt_config,
                            None,
                        )
                        .await
                        {
                            Some(transcript) => {
                                debug!(filename = %attachment.filename, chars = transcript.len(), "voice transcript injected");
                                extra_blocks.insert(
                                    0,
                                    ContentBlock::Text {
                                        text: format!("[Voice message transcript]: {transcript}"),
                                    },
                                );
                                echo_entries.push(crate::stt::EchoEntry::Success(transcript));
                            }
                            None => {
                                warn!(filename = %attachment.filename, "STT failed for voice attachment");
                                echo_entries.push(crate::stt::EchoEntry::Failed);
                            }
                        }
                    } else {
                        tracing::warn!(filename = %attachment.filename, "skipping audio attachment (STT disabled)");
                        let msg_ref = discord_msg_ref(&msg);
                        let _ = adapter.add_reaction(&msg_ref, "🎤").await;
                    }
                } else if media::is_text_file(
                    &attachment.filename,
                    attachment.content_type.as_deref(),
                ) {
                    if text_file_count >= TEXT_FILE_COUNT_CAP {
                        tracing::warn!(filename = %attachment.filename, count = text_file_count, "text file count cap reached, skipping");
                        continue;
                    }
                    if let Some((block, _)) = media::download_and_read_text_file(
                        &attachment.url,
                        &attachment.filename,
                        u64::from(attachment.size),
                        None,
                    )
                    .await
                    {
                        text_file_count += 1;
                        debug!(filename = %attachment.filename, "adding text file attachment");
                        extra_blocks.push(block);
                    }
                } else {
                    match media::download_and_encode_image(
                        &attachment.url,
                        attachment.content_type.as_deref(),
                        &attachment.filename,
                        u64::from(attachment.size),
                        None,
                    )
                    .await
                    {
                        Ok(block) => {
                            debug!(url = %attachment.url, filename = %attachment.filename, "adding image attachment");
                            extra_blocks.push(block);
                        }
                        Err(media::MediaFetchError::NotAnImage) => {
                            if media::is_video_file(
                                &attachment.filename,
                                attachment.content_type.as_deref(),
                            ) {
                                debug!(url = %attachment.url, filename = %attachment.filename, "adding video attachment link");
                                extra_blocks.push(video_attachment_block(
                                    &attachment.filename,
                                    attachment.content_type.as_deref(),
                                    u64::from(attachment.size),
                                    &attachment.url,
                                ));
                            } else {
                                debug!(url = %attachment.url, filename = %attachment.filename, "adding file attachment link");
                                extra_blocks.push(file_attachment_block(
                                    &attachment.filename,
                                    attachment.content_type.as_deref(),
                                    u64::from(attachment.size),
                                    &attachment.url,
                                ));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                url = %attachment.url,
                                filename = %attachment.filename,
                                error = %e,
                                "image attachment failed"
                            );
                            failed_image_files.push(attachment.filename.clone());
                        }
                    }
                }
            }
        } else if !msg.attachments.is_empty() {
            tracing::debug!(
                num_attachments = msg.attachments.len(),
                "inbound attachment ContentBlocks disabled; not forwarding attachments to ACP"
            );
        }

        tracing::debug!(
            num_extra_blocks = extra_blocks.len(),
            num_attachments = msg.attachments.len(),
            in_thread,
            "processing"
        );

        let thread_channel = if in_thread || is_dm {
            // DMs use the DM channel directly (no threads in DMs).
            ChannelRef {
                platform: "discord".into(),
                channel_id: msg.channel_id.get().to_string(),
                thread_id: None,
                parent_id: thread_parent_id.clone(),
                origin_event_id: None,
            }
        } else {
            match get_or_create_thread(&ctx, &adapter, &msg, &prompt).await {
                Ok(ch) => ch,
                Err(e) => {
                    error!("failed to create thread: {e}");
                    audit.finish(IngressDecision::ThreadCreateFailed);
                    return;
                }
            }
        };

        audit.set_thread_id((!is_dm).then(|| thread_channel.channel_id.clone()));

        if !in_thread && !is_dm && thread_channel.parent_id.is_some() {
            // A top-level trigger that created/joined a thread makes this bot
            // logically involved before its first reply is visible in history.
            self.mark_thread_participated(&thread_channel.channel_id)
                .await;
        }

        // Notify user if any images couldn't be processed.
        if !failed_image_files.is_empty() {
            let file_list = failed_image_files
                .iter()
                .map(|n| format!("`{}`", n.replace('`', "'")))
                .collect::<Vec<_>>()
                .join(", ");
            let warn_msg = format!(
                ":warning: I couldn't process the image(s) you shared ({}). \
                 The files may be inaccessible or in an unsupported format (PNG/JPEG/GIF/WebP only).",
                file_list
            );
            if let Err(e) = adapter.send_message(&thread_channel, &warn_msg).await {
                tracing::warn!(error = %e, "failed to send image warning to user");
            }
        }

        let trigger_msg = discord_msg_ref(&msg);

        // Per-thread streaming: check if another bot is present in this thread
        let other_bot_present_flag = {
            let cache = self.multibot_threads.lock().await;
            cached_multibot_thread(&cache, &thread_channel.channel_id, self.session_ttl)
        };

        // Backfill thread_id: when OAB just created a new thread, the sender
        // was built before the thread existed. Patch it so the agent sees
        // thread_id on the very first turn.
        let mut sender = sender;
        if sender.thread_id.is_none() && thread_channel.parent_id.is_some() {
            sender.thread_id = Some(thread_channel.channel_id.clone());
        }
        sender.recovered_context = recover_discord_context(
            &ctx.http,
            &msg,
            &self.context_recovery,
            self.allow_all_channels,
            &self.allowed_channels,
        )
        .await;

        let dispatcher = self.dispatcher.clone();
        let stt_cfg = self.stt_config.clone();

        tokio::spawn(async move {
            // Best-effort echo before the agent reply so the user can verify STT.
            crate::stt::post_echo(
                &adapter,
                &thread_channel,
                &trigger_msg,
                &echo_entries,
                &stt_cfg,
            )
            .await;

            let sender_id = sender.sender_id.clone();
            let sender_name = sender.sender_name.clone();
            let sender_json = serde_json::to_string(&sender).unwrap();
            let thread_key = dispatcher.key("discord", &thread_channel.channel_id, &sender_id);
            let estimated_tokens = crate::dispatch::estimate_tokens(&prompt, &extra_blocks);
            let buf_msg = crate::dispatch::BufferedMessage {
                sender_json,
                sender_name,
                prompt,
                extra_blocks,
                trigger_msg,
                arrived_at: std::time::Instant::now(),
                estimated_tokens,
                other_bot_present: other_bot_present_flag,
                spool_id: None,
            };
            match dispatcher
                .submit(thread_key, thread_channel, adapter, buf_msg)
                .await
            {
                Ok(()) => audit.finish(IngressDecision::Dispatched),
                Err(e) => {
                    audit.finish(IngressDecision::DispatchFailed);
                    error!("dispatcher submit error: {e}");
                }
            }
        });
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "discord bot connected");

        // Build the shared command list once.
        let commands = vec![
            CreateCommand::new("models").description("Select the AI model for this session"),
            CreateCommand::new("agents").description("Select the agent mode for this session"),
            CreateCommand::new("cancel").description("Cancel the current operation"),
            CreateCommand::new("cancel-all")
                .description("Cancel current operation and drop all buffered messages"),
            CreateCommand::new("reset").description("Reset the conversation session"),
            CreateCommand::new("remind")
                .description("Set a one-shot reminder to mention users/roles after a delay")
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "targets",
                        "Users/roles to mention (e.g. @user1 @role1)",
                    )
                    .required(true),
                )
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "message",
                        "Reminder message",
                    )
                    .required(true),
                )
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "delay",
                        "Delay before firing (e.g. 30m, 2h, 1d)",
                    )
                    .required(true),
                ),
            CreateCommand::new("export-thread")
                .description("Download this thread as a text file")
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "limit",
                    "Export only the most recent N messages (1–5000)",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::String,
                    "since",
                    "Export messages after this message ID",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "days",
                    "Export messages from the last N days (1–365)",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Boolean,
                    "all",
                    "Export all messages (up to 5000). Default is last 100.",
                )),
        ];

        // Register global commands only. Registering the same commands per-guild
        // makes Discord show duplicate slash commands in guild command pickers.
        if let Err(e) = Command::set_global_commands(&ctx.http, commands.clone()).await {
            tracing::warn!(error = %e, "failed to register global slash commands");
        } else {
            info!("registered global slash commands");
        }

        // One-time migration cleanup: older versions registered the same
        // slash commands per-guild, and Discord persists those server-side.
        // Keep guild command sets empty so only global commands are shown.
        for guild in &ready.guilds {
            let guild_id = guild.id;
            if let Err(e) = guild_id.set_commands(&ctx.http, Vec::new()).await {
                tracing::warn!(
                    %guild_id,
                    error = %e,
                    "failed to clear stale guild slash commands"
                );
            }
        }

        // Re-schedule any pending reminders that survived a restart.
        let pending = self.reminder_store.pending().await;
        if !pending.is_empty() {
            let mut scheduled = self.scheduled_ids.lock().await;
            let mut count = 0;
            for r in pending {
                if scheduled.insert(r.id.clone()) {
                    remind::schedule_reminder(ctx.http.clone(), self.reminder_store.clone(), r);
                    count += 1;
                }
            }
            if count > 0 {
                info!(count, "re-scheduled pending reminders");
            }
        }

        // Replay the durable queue that survived a restart: re-submit each still-
        // queued message once. `swap` guards against a serenity reconnect (which
        // re-fires `ready`) re-submitting the backlog.
        if !self.replayed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let queued = self.spool.entries();
            if !queued.is_empty() {
                let adapter = self
                    .adapter
                    .get_or_init(|| Arc::new(DiscordAdapter::new(ctx.http.clone())))
                    .clone();
                info!(count = queued.len(), "replaying persisted queue after restart");
                for pm in queued {
                    let thread_key = pm.thread_key.clone();
                    let thread_channel = pm.thread_channel.clone();
                    let buf = crate::dispatch::BufferedMessage {
                        sender_json: pm.sender_json,
                        sender_name: pm.sender_name,
                        prompt: pm.prompt,
                        extra_blocks: pm.extra_blocks,
                        trigger_msg: pm.trigger_msg,
                        arrived_at: std::time::Instant::now(),
                        estimated_tokens: pm.estimated_tokens,
                        other_bot_present: pm.other_bot_present,
                        spool_id: Some(pm.id),
                    };
                    if let Err(e) = self
                        .dispatcher
                        .submit(thread_key, thread_channel, adapter.clone(), buf)
                        .await
                    {
                        error!(error = %e, "queue replay submit failed");
                    }
                }
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let guild_id = match &interaction {
            Interaction::Command(cmd) => cmd.guild_id.map(|id| id.get()),
            Interaction::Component(comp) => comp.guild_id.map(|id| id.get()),
            _ => None,
        };
        if !self.guild_allowed(guild_id) {
            if let Some(guild_id) = guild_id {
                self.reject_disallowed_guild_interaction(&ctx, &interaction, guild_id)
                    .await;
            }
            return;
        }

        match interaction {
            Interaction::Command(cmd) if cmd.data.name == "models" => {
                self.handle_config_command(&ctx, &cmd, "model", "model")
                    .await;
            }
            Interaction::Command(cmd) if cmd.data.name == "agents" => {
                self.handle_config_command(&ctx, &cmd, "agent", "agent")
                    .await;
            }
            Interaction::Command(cmd) if cmd.data.name == "cancel" => {
                self.handle_cancel_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "cancel-all" => {
                self.handle_cancel_all_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "reset" => {
                self.handle_reset_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "remind" => {
                self.handle_remind_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "export-thread" => {
                self.handle_export_thread_command(&ctx, &cmd).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("acp_config_") => {
                self.handle_config_select(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("acp_pg:") => {
                self.handle_pagination(&ctx, &comp).await;
            }
            _ => {}
        }
    }
}

// --- Slash command & interaction handlers ---

impl Handler {
    /// Build a Discord select menu from ACP configOptions with the given category.
    /// Paginates options in pages of 25 (Discord limit). The current selection is
    /// always placed first so it appears on page 0.
    fn build_config_select(
        options: &[ConfigOption],
        category: &str,
        page: usize,
    ) -> Option<CreateSelectMenu> {
        let opt = options
            .iter()
            .find(|o| o.category.as_deref() == Some(category))?;

        // Put current selection first so it always lands on page 0,
        // then fill remaining slots in original order.
        let sorted: Vec<_> = opt
            .options
            .iter()
            .filter(|o| o.value == opt.current_value)
            .chain(opt.options.iter().filter(|o| o.value != opt.current_value))
            .collect();

        let menu_options: Vec<CreateSelectMenuOption> = sorted
            .iter()
            .skip(page * SELECT_MENU_PAGE_SIZE)
            .take(SELECT_MENU_PAGE_SIZE)
            .map(|o| {
                let mut item = CreateSelectMenuOption::new(&o.name, &o.value);
                if let Some(desc) = &o.description {
                    item = item.description(desc);
                }
                if o.value == opt.current_value {
                    item = item.default_selection(true);
                }
                item
            })
            .collect();

        if menu_options.is_empty() {
            return None;
        }

        let current_name = opt
            .options
            .iter()
            .find(|o| o.value == opt.current_value)
            .map(|o| o.name.as_str())
            .unwrap_or(&opt.current_value);
        let total_pages = sorted.len().div_ceil(SELECT_MENU_PAGE_SIZE);
        let placeholder = if total_pages > 1 {
            format!(
                "Current: {} (page {}/{})",
                current_name,
                page + 1,
                total_pages
            )
        } else {
            format!("Current: {}", current_name)
        };

        Some(
            CreateSelectMenu::new(
                format!("acp_config_{}", opt.id),
                CreateSelectMenuKind::String {
                    options: menu_options,
                },
            )
            .placeholder(placeholder),
        )
    }

    /// Build ◀/▶ pagination buttons. Returns None when only one page exists.
    fn build_pagination_buttons(
        category: &str,
        page: usize,
        total_pages: usize,
    ) -> Option<CreateActionRow> {
        if total_pages <= 1 {
            return None;
        }
        let prev = CreateButton::new(format!("acp_pg:{}:{}", category, page.saturating_sub(1)))
            .label("◀")
            .style(ButtonStyle::Secondary)
            .disabled(page == 0);
        let next = CreateButton::new(format!("acp_pg:{}:{}", category, page + 1))
            .label("▶")
            .style(ButtonStyle::Secondary)
            .disabled(page + 1 >= total_pages);
        let indicator = CreateButton::new("acp_pg_noop")
            .label(format!("{}/{}", page + 1, total_pages))
            .style(ButtonStyle::Secondary)
            .disabled(true);
        Some(CreateActionRow::Buttons(vec![prev, indicator, next]))
    }

    /// Build the full component rows (select menu + optional pagination) for a config category.
    /// When `page` is `None`, auto-selects the page containing the current value.
    fn build_config_components(
        options: &[ConfigOption],
        category: &str,
        page: Option<usize>,
    ) -> Option<Vec<CreateActionRow>> {
        let opt = options
            .iter()
            .find(|o| o.category.as_deref() == Some(category))?;
        let total_pages = opt.options.len().div_ceil(SELECT_MENU_PAGE_SIZE);
        let page = match page {
            Some(p) => p.min(total_pages.saturating_sub(1)),
            None => opt
                .options
                .iter()
                .position(|o| o.value == opt.current_value)
                .map(|i| i / SELECT_MENU_PAGE_SIZE)
                .unwrap_or(0),
        };

        let select = Self::build_config_select(options, category, page)?;
        let mut rows = vec![CreateActionRow::SelectMenu(select)];
        if let Some(buttons) = Self::build_pagination_buttons(category, page, total_pages) {
            rows.push(buttons);
        }
        Some(rows)
    }

    async fn handle_config_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
        category: &str,
        label: &str,
    ) {
        let thread_key = format!("discord:{}", cmd.channel_id.get());
        let config_options = self.router.pool().get_config_options(&thread_key).await;

        let response = match Self::build_config_components(&config_options, category, None) {
            Some(rows) => CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!("🔧 Select a {label}:"))
                    .components(rows)
                    .ephemeral(true),
            ),
            None => CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!("⚠️ No {label} options available. Start a conversation first by @mentioning the bot."))
                    .ephemeral(true),
            ),
        };

        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, category, "failed to respond to slash command");
        }
    }

    async fn handle_cancel_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        let thread_key = format!("discord:{}", cmd.channel_id.get());
        let result = self.router.pool().cancel_session(&thread_key).await;

        let msg = match result {
            Ok(()) => "🛑 Cancel signal sent.".to_string(),
            Err(e) => format!("⚠️ {e}"),
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(msg)
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to /cancel command");
        }
    }

    async fn handle_cancel_all_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        // /cancel-all is the nuclear escape hatch: stop the in-flight turn AND clear
        // every lane's buffer in this thread, so a human can intervene from a clean slate.
        let session_key = format!("discord:{}", cmd.channel_id.get());
        let dropped = self
            .dispatcher
            .cancel_buffered_thread("discord", &cmd.channel_id.get().to_string());

        let cancel_result = self.router.pool().cancel_session(&session_key).await;

        // Buffer count is approximate (sweep races with new arrivals) so we surface
        // a binary "cleared / nothing" signal rather than a misleading exact number.
        let msg = match (cancel_result, dropped) {
            (Ok(()), 0) => "🛑 Cancel signal sent.".to_string(),
            (Ok(()), _) => "🛑 Cancel signal sent. Buffered messages cleared.".to_string(),
            (Err(_), 0) => {
                "⚠️ Nothing to cancel — no active session and no buffered messages.".to_string()
            }
            (Err(_), _) => "🛑 Buffered messages cleared. No active session to cancel.".to_string(),
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(msg)
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to /cancel-all command");
        }
    }

    async fn handle_reset_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        // /reset clears every lane's buffer in this thread and tears down the shared
        // ACP session — the next message in the thread starts a fresh conversation.
        let session_key = format!("discord:{}", cmd.channel_id.get());
        let dropped = self
            .dispatcher
            .cancel_buffered_thread("discord", &cmd.channel_id.get().to_string());

        let result = self.router.pool().reset_session(&session_key).await;

        let msg = match result {
            Ok(()) if dropped > 0 => {
                format!("🔄 Session reset. Dropped {dropped} buffered message(s). Start a new conversation!")
            }
            Ok(()) => "🔄 Session reset. Start a new conversation!".to_string(),
            Err(_) if dropped > 0 => {
                format!("🔄 Dropped {dropped} buffered message(s). No active session to reset.")
            }
            Err(_) => {
                "⚠️ No active session to reset. Start a conversation first by @mentioning the bot."
                    .to_string()
            }
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(msg)
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to /reset command");
        }
    }

    async fn handle_remind_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        // Only humans can use /remind
        if cmd.user.bot {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Only humans can set reminders.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // Extract options
        let opts = &cmd.data.options;
        let targets_raw = opts
            .iter()
            .find(|o| o.name == "targets")
            .and_then(|o| o.value.as_str())
            .unwrap_or("");
        let message = opts
            .iter()
            .find(|o| o.name == "message")
            .and_then(|o| o.value.as_str())
            .unwrap_or("");
        let delay_raw = opts
            .iter()
            .find(|o| o.name == "delay")
            .and_then(|o| o.value.as_str())
            .unwrap_or("");

        if targets_raw.is_empty() || message.is_empty() || delay_raw.is_empty() {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ All fields (targets, message, delay) are required.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // Parse delay
        let delay_secs = match remind::parse_delay(delay_raw) {
            Ok(s) => s,
            Err(e) => {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(format!("⚠️ Invalid delay: {e}"))
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
        };

        if let Err(e) = remind::validate_message(message) {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!("⚠️ {e}"))
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // Strip @everyone / @here to prevent unintended mass pings.
        let message = remind::sanitize_message(message);

        // Extract mention strings from targets (keep raw — Discord renders them)
        let targets: Vec<String> = targets_raw
            .split_whitespace()
            .filter(|t| t.starts_with("<@") && t.ends_with('>'))
            .map(|t| t.to_string())
            .collect();

        if targets.is_empty() {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ No valid mentions found in targets. Use @user or @role.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        if targets.len() > remind::MAX_TARGETS {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!(
                        "⚠️ Too many targets (max {}). Use a @role instead.",
                        remind::MAX_TARGETS
                    ))
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // F4: Per-user rate limit (max 5 active reminders)
        let user_id = cmd.user.id.get();
        let pending = self.reminder_store.pending().await;
        let user_count = pending.iter().filter(|r| r.sender_id == user_id).count();
        if user_count >= 5 {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ You already have 5 active reminders. Wait for some to fire before adding more.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        let fire_at = chrono::Utc::now() + chrono::Duration::seconds(delay_secs as i64);
        let reminder = remind::Reminder {
            id: uuid::Uuid::new_v4().to_string(),
            channel_id: cmd.channel_id.get(),
            sender_id: cmd.user.id.get(),
            targets: targets.clone(),
            message: message.clone(),
            fire_at,
            created_at: chrono::Utc::now(),
        };

        // Persist and schedule
        self.reminder_store.add(reminder.clone()).await;
        self.scheduled_ids.lock().await.insert(reminder.id.clone());
        remind::schedule_reminder(ctx.http.clone(), self.reminder_store.clone(), reminder);

        let delay_str = remind::format_delay(delay_secs);
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(format!(
                    "⏰ Reminder set! Will fire in **{delay_str}** and mention {}",
                    targets.join(" ")
                ))
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to /remind command");
        }
    }

    async fn handle_export_thread_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        if is_denied_user(
            false,
            self.allow_all_users,
            &self.allowed_users,
            cmd.user.id.get(),
        ) {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 You are not allowed to use this bot.")
                    .ephemeral(true),
            );
            if let Err(e) = cmd.create_response(&ctx.http, response).await {
                tracing::error!(error = %e, "failed to deny /export-thread command");
            }
            return;
        }

        let channel_id = cmd.channel_id;
        let (export_allowed, export_name) = match channel_id.to_channel(&ctx.http).await {
            Ok(serenity::model::channel::Channel::Guild(gc)) => {
                let in_allowed_channel =
                    self.allow_all_channels || self.allowed_channels.contains(&channel_id.get());
                let (in_thread, _) = detect_thread(
                    gc.thread_metadata.is_some(),
                    gc.parent_id.map(|id| id.get()),
                    gc.owner_id.map(|id| id.get()),
                    ctx.cache.current_user().id.get(),
                    &self.allowed_channels,
                    self.allow_all_channels,
                    in_allowed_channel,
                );
                (in_thread, gc.name.clone())
            }
            Ok(serenity::model::channel::Channel::Private(_)) => (self.allow_dm, "dm".to_string()),
            Ok(_) => (false, "channel".to_string()),
            Err(e) => {
                tracing::warn!(channel_id = %channel_id, error = %e, "failed to inspect channel for export");
                (false, "channel".to_string())
            }
        };

        if !export_allowed {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Run this command inside an allowed Discord thread or DM.")
                    .ephemeral(true),
            );
            if let Err(e) = cmd.create_response(&ctx.http, response).await {
                tracing::error!(error = %e, "failed to respond to /export-thread rejection");
            }
            return;
        }

        // --- Parse and validate filter params (mutual exclusion) ---
        let opts = &cmd.data.options;
        let limit_opt = opts
            .iter()
            .find(|o| o.name == "limit")
            .and_then(|o| o.value.as_i64());
        let since_opt = opts
            .iter()
            .find(|o| o.name == "since")
            .and_then(|o| o.value.as_str());
        let days_opt = opts
            .iter()
            .find(|o| o.name == "days")
            .and_then(|o| o.value.as_i64());
        let all_opt = opts
            .iter()
            .find(|o| o.name == "all")
            .and_then(|o| o.value.as_bool())
            .unwrap_or(false);

        let filter_count = limit_opt.is_some() as u8
            + since_opt.is_some() as u8
            + days_opt.is_some() as u8
            + all_opt as u8;
        if filter_count > 1 {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(
                        "⚠️ Please specify only one filter: `limit`, `since`, `days`, or `all`.",
                    )
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        let filter = if all_opt {
            ExportFilter::All
        } else if let Some(n) = limit_opt {
            if !(1..=5000).contains(&n) {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ `limit` must be between 1 and 5000.")
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
            ExportFilter::Limit(n as usize)
        } else if let Some(id_str) = since_opt {
            match id_str.parse::<u64>() {
                Ok(id) if id > 0 => ExportFilter::After(MessageId::new(id)),
                _ => {
                    let response = CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("⚠️ `since` must be a valid message ID (right-click a message → Copy Message ID).")
                            .ephemeral(true),
                    );
                    let _ = cmd.create_response(&ctx.http, response).await;
                    return;
                }
            }
        } else if let Some(d) = days_opt {
            if !(1..=365).contains(&d) {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ `days` must be between 1 and 365.")
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
            let since_ts = chrono::Utc::now() - chrono::Duration::days(d);
            let ts_ms = since_ts.timestamp_millis() as u64;
            ExportFilter::After(timestamp_ms_to_snowflake(ts_ms))
        } else {
            // Default: export last 100 messages (use limit:N or all:true for more)
            ExportFilter::Limit(100)
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("Preparing thread export...")
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to acknowledge /export-thread command");
            return;
        }

        match export_channel_messages(
            &ctx.http,
            channel_id,
            &export_name,
            cmd.attachment_size_limit,
            filter,
        )
        .await
        {
            Ok(result) => {
                let mut content = format!("Exported {} messages.", result.written);
                if result.hit_cap {
                    content.push_str(&format!(
                        " Only the most recent {} messages were fetched — older messages were not included.",
                        result.fetched
                    ));
                }
                if result.byte_truncated {
                    content.push_str(&format!(
                        " Transcript truncated to fit Discord's attachment size limit ({} of {} fetched messages included).",
                        result.written, result.fetched
                    ));
                }
                let attachment =
                    CreateAttachment::bytes(result.transcript.into_bytes(), result.filename);
                let followup = CreateInteractionResponseFollowup::new()
                    .content(content)
                    .add_file(attachment)
                    .ephemeral(true);
                if let Err(e) = cmd.create_followup(&ctx.http, followup).await {
                    tracing::error!(error = %e, "failed to send /export-thread attachment");
                }
            }
            Err(e) => {
                tracing::warn!(channel_id = %channel_id, error = %e, "failed to export thread");
                let followup = CreateInteractionResponseFollowup::new()
                    .content(format!("⚠️ Failed to export thread: {e}"))
                    .ephemeral(true);
                if let Err(e) = cmd.create_followup(&ctx.http, followup).await {
                    tracing::error!(error = %e, "failed to send /export-thread error");
                }
            }
        }
    }

    async fn handle_config_select(
        &self,
        ctx: &Context,
        comp: &serenity::model::application::ComponentInteraction,
    ) {
        let config_id = comp
            .data
            .custom_id
            .strip_prefix("acp_config_")
            .unwrap_or("")
            .to_string();

        if config_id.is_empty() {
            return;
        }

        let selected_value = match &comp.data.kind {
            ComponentInteractionDataKind::StringSelect { values } => match values.first() {
                Some(v) => v.clone(),
                None => return,
            },
            _ => return,
        };

        let thread_key = format!("discord:{}", comp.channel_id.get());

        let result = self
            .router
            .pool()
            .set_config_option(&thread_key, &config_id, &selected_value)
            .await;

        let response_msg = match result {
            Ok(updated_options) => {
                let display_name = updated_options
                    .iter()
                    .find(|o| o.id == config_id)
                    .and_then(|o| o.options.iter().find(|v| v.value == selected_value))
                    .map(|v| v.name.as_str())
                    .unwrap_or(&selected_value);
                format!("✅ Switched to **{}**", display_name)
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to set config option");
                format!("❌ Failed to switch: {}", e)
            }
        };

        let response = CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(response_msg)
                .components(vec![]),
        );

        if let Err(e) = comp.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to config select");
        }
    }

    async fn handle_pagination(
        &self,
        ctx: &Context,
        comp: &serenity::model::application::ComponentInteraction,
    ) {
        // Parse custom_id format: acp_pg:{category}:{page}
        let parts: Vec<&str> = comp.data.custom_id.splitn(3, ':').collect();
        let (category, page) = match parts.as_slice() {
            [_, cat, pg] => match pg.parse::<usize>() {
                Ok(p) => (*cat, p),
                Err(_) => return,
            },
            _ => return,
        };

        // Only allow known config categories.
        if !matches!(category, "model" | "agent") {
            return;
        }

        let thread_key = format!("discord:{}", comp.channel_id.get());
        let config_options = self.router.pool().get_config_options(&thread_key).await;

        let response = match Self::build_config_components(&config_options, category, Some(page)) {
            Some(rows) => CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(format!("🔧 Select a {category}:"))
                    .components(rows),
            ),
            None => CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(format!("⚠️ No {category} options available."))
                    .components(vec![]),
            ),
        };

        if let Err(e) = comp.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, category, "failed to respond to pagination");
        }
    }
}

// --- Discord-specific helpers ---

fn discord_msg_ref(msg: &Message) -> MessageRef {
    MessageRef {
        channel: ChannelRef {
            platform: "discord".into(),
            channel_id: msg.channel_id.get().to_string(),
            thread_id: None,
            parent_id: None,
            origin_event_id: None,
        },
        message_id: msg.id.to_string(),
    }
}

struct ExportResult {
    filename: String,
    transcript: String,
    /// Messages successfully pulled from Discord.
    fetched: usize,
    /// Messages that fit in the transcript (≤ `fetched`; differs when the
    /// attachment-size limit truncates).
    written: usize,
    /// We stopped fetching because we hit the message cap and the thread still
    /// has more messages we did not include.
    hit_cap: bool,
    /// Transcript was cut to keep the attachment under Discord's size limit.
    byte_truncated: bool,
}

/// Filter mode for export_channel_messages.
enum ExportFilter {
    /// Fetch all messages (newest-first via `before`), capped at THREAD_EXPORT_MESSAGE_LIMIT.
    All,
    /// Fetch the most recent N messages (newest-first via `before`).
    Limit(usize),
    /// Fetch messages after a synthetic snowflake (newest-first via `before`, with boundary filtering).
    After(MessageId),
}

/// Discord epoch: 2015-01-01T00:00:00Z in milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Convert a UTC timestamp (in milliseconds since Unix epoch) to a synthetic
/// Discord snowflake suitable for use as an `after` cursor.
fn timestamp_ms_to_snowflake(timestamp_ms: u64) -> MessageId {
    let discord_ms = timestamp_ms.saturating_sub(DISCORD_EPOCH_MS);
    // Snowflake IDs use NonZeroU64 in serenity; ensure at least 1.
    MessageId::new((discord_ms << 22).max(1))
}

async fn export_channel_messages(
    http: &Http,
    channel_id: ChannelId,
    channel_name: &str,
    attachment_size_limit: u32,
    filter: ExportFilter,
) -> anyhow::Result<ExportResult> {
    let cap = match &filter {
        ExportFilter::Limit(n) => *n,
        _ => THREAD_EXPORT_MESSAGE_LIMIT,
    };

    let mut messages = Vec::new();
    let mut hit_cap = false;

    match &filter {
        ExportFilter::All | ExportFilter::Limit(_) => {
            // Fetch newest-first using `before` pagination, then reverse.
            let mut before = None;
            loop {
                if messages.len() >= cap {
                    hit_cap = true;
                    break;
                }
                let remaining = cap - messages.len();
                let limit = remaining.min(100) as u8;
                let mut request = GetMessages::new().limit(limit);
                if let Some(before_id) = before {
                    request = request.before(before_id);
                }
                let batch = channel_id.messages(http, request).await?;
                if batch.is_empty() {
                    break;
                }
                before = batch.last().map(|m| m.id);
                let batch_len = batch.len();
                messages.extend(batch);
                if batch_len < limit as usize {
                    break;
                }
            }
            // Probe to confirm we actually left messages behind.
            if hit_cap {
                let probe = GetMessages::new().limit(1);
                let probe = if let Some(before_id) = before {
                    probe.before(before_id)
                } else {
                    probe
                };
                if matches!(channel_id.messages(http, probe).await, Ok(b) if b.is_empty()) {
                    hit_cap = false;
                }
            }
            messages.reverse();
        }
        ExportFilter::After(after_id) => {
            // Fetch newest-first using `before` pagination, stop when we hit
            // messages at or before the filter boundary. This ensures that when
            // the cap is reached, we keep the *newest* messages in the window.
            let mut before = None;
            loop {
                if messages.len() >= cap {
                    hit_cap = true;
                    break;
                }
                let remaining = cap - messages.len();
                let limit = remaining.min(100) as u8;
                let mut request = GetMessages::new().limit(limit);
                if let Some(before_id) = before {
                    request = request.before(before_id);
                }
                let batch = channel_id.messages(http, request).await?;
                if batch.is_empty() {
                    break;
                }
                before = batch.last().map(|m| m.id);
                let batch_len = batch.len();
                // Filter out messages at or before the boundary.
                let filtered: Vec<_> = batch.into_iter().filter(|m| m.id > *after_id).collect();
                let hit_boundary = filtered.len() < batch_len;
                messages.extend(filtered);
                if hit_boundary {
                    // We've reached the time boundary; no need to fetch older.
                    break;
                }
                if batch_len < limit as usize {
                    break;
                }
            }
            // Probe only if we stopped due to cap (not boundary).
            if hit_cap {
                let probe = GetMessages::new().limit(1);
                let probe = if let Some(before_id) = before {
                    probe.before(before_id)
                } else {
                    probe
                };
                if let Ok(batch) = channel_id.messages(http, probe).await {
                    // If the next message is beyond our filter boundary,
                    // we didn't actually leave relevant messages behind.
                    let has_more_in_window = batch.iter().any(|m| m.id > *after_id);
                    if !has_more_in_window {
                        hit_cap = false;
                    }
                }
            }
            messages.reverse();
        }
    }

    let filename = export_filename(channel_id, channel_name);
    if attachment_size_limit < 2048 {
        tracing::warn!(
            attachment_size_limit,
            "attachment_size_limit is very small; export will likely be truncated"
        );
    }
    let max_bytes = usize::try_from(attachment_size_limit)
        .unwrap_or(8 * 1024 * 1024)
        .saturating_sub(1024)
        .max(1024);
    let (transcript, written, byte_truncated) =
        format_thread_export(channel_id, channel_name, &messages, max_bytes);
    let fetched = messages.len();

    Ok(ExportResult {
        filename,
        transcript,
        fetched,
        written,
        hit_cap,
        byte_truncated,
    })
}

fn format_thread_export(
    channel_id: ChannelId,
    channel_name: &str,
    messages: &[Message],
    max_bytes: usize,
) -> (String, usize, bool) {
    let header = format!(
        "Discord thread export\nChannel: {channel_name} ({channel_id})\nMessages: {}\n\n",
        messages.len()
    );
    let entries: Vec<String> = messages.iter().map(format_export_message).collect();
    assemble_export(&header, &entries, max_bytes)
}

/// Build the transcript body from a pre-rendered header and a list of
/// already-formatted message entries, honouring `max_bytes`.
///
/// Returns `(transcript, written, truncated)` where `written` is the number of
/// entries actually included. Split out from `format_thread_export` so the
/// truncation boundary logic can be unit-tested without constructing real
/// `serenity::model::channel::Message` values.
fn assemble_export(header: &str, entries: &[String], max_bytes: usize) -> (String, usize, bool) {
    let mut out = String::from(header);
    let mut written = 0;
    let mut truncated = false;

    for entry in entries {
        if out.len() + entry.len() > max_bytes {
            truncated = true;
            break;
        }
        out.push_str(entry);
        written += 1;
    }

    if truncated {
        let note = "\n[Export truncated to fit Discord attachment size limit]\n";
        let room = max_bytes.saturating_sub(out.len());
        if room >= note.len() {
            out.push_str(note);
        }
    }

    (out, written, truncated)
}

fn format_export_message(msg: &Message) -> String {
    let bot_marker = if msg.author.bot { " [bot]" } else { "" };
    let mut out = format!(
        "[{}] {}{} ({})\n",
        msg.timestamp, msg.author.name, bot_marker, msg.author.id
    );

    if msg.content.is_empty() {
        out.push_str("(no text)\n");
    } else {
        out.push_str(&msg.content);
        out.push('\n');
    }

    for attachment in &msg.attachments {
        let mime = attachment.content_type.as_deref().unwrap_or("unknown");
        out.push_str(&format!(
            "[attachment] {} ({} bytes, {}): {}\n",
            attachment.filename, attachment.size, mime, attachment.url
        ));
    }

    out.push('\n');
    out
}

fn export_filename(channel_id: ChannelId, channel_name: &str) -> String {
    let safe_name = sanitize_filename_component(channel_name);
    format!("discord-thread-{safe_name}-{channel_id}.txt")
}

/// Reduce a free-form Discord channel/thread name to a safe ASCII filename
/// fragment.
///
/// Non-ASCII characters are dropped silently — a purely-Chinese thread name
/// like "扈三娘的房間" yields a date-based fallback (e.g. `"20260512"`).
/// The caller appends the channel ID, which already guarantees uniqueness,
/// and an ASCII fragment plays nicer with downstream tools (mail attachments,
/// S3 keys, browser save-as dialogs). The 64-byte cap leaves room for the
/// `discord-thread-` prefix and the channel-ID suffix within typical
/// filesystem limits.
fn sanitize_filename_component(input: &str) -> String {
    let mut safe = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            safe.push(ch);
        } else if ch.is_whitespace() || matches!(ch, '.' | '/') {
            safe.push('-');
        }
    }
    let safe = safe.trim_matches('-');
    if safe.is_empty() {
        // Use current date as a human-friendly fallback when the thread name
        // is entirely non-ASCII.
        chrono::Utc::now().format("%Y%m%d").to_string()
    } else {
        safe.chars().take(64).collect()
    }
}

async fn get_or_create_thread(
    ctx: &Context,
    adapter: &Arc<dyn ChatAdapter>,
    msg: &Message,
    prompt: &str,
) -> anyhow::Result<ChannelRef> {
    let channel = msg.channel_id.to_channel(&ctx.http).await?;
    if let serenity::model::channel::Channel::Guild(ref gc) = channel {
        // Already in a thread — reuse it. Uses thread_metadata (see detect_thread()).
        if gc.thread_metadata.is_some() {
            return Ok(ChannelRef {
                platform: "discord".into(),
                channel_id: msg.channel_id.get().to_string(),
                thread_id: None,
                parent_id: None,
                origin_event_id: None,
            });
        }
    }

    let thread_name = format::shorten_thread_name(prompt);
    let parent = ChannelRef {
        platform: "discord".into(),
        channel_id: msg.channel_id.get().to_string(),
        thread_id: None,
        parent_id: None,
        origin_event_id: None,
    };
    let trigger_ref = discord_msg_ref(msg);
    match adapter
        .create_thread(&parent, &trigger_ref, &thread_name)
        .await
    {
        Ok(ch) => Ok(ch),
        Err(e) if is_thread_already_exists_error(&e) => {
            // Another bot won the race from the same trigger message. Discord
            // only allows one thread per message, so refetch the message and
            // join the thread our sibling just created.
            let refreshed = msg
                .channel_id
                .message(&ctx.http, msg.id)
                .await
                .map_err(|fe| {
                    anyhow::anyhow!("thread_already_exists (race), but refetch failed: {fe}")
                })?;
            let existing = refreshed.thread.ok_or_else(|| {
                anyhow::anyhow!(
                    "thread_already_exists (race), but message has no thread after refetch"
                )
            })?;
            tracing::info!(
                channel_id = %msg.channel_id,
                thread_id = %existing.id,
                "joining thread created by sibling bot from same trigger message"
            );
            Ok(ChannelRef {
                platform: "discord".into(),
                channel_id: existing.id.to_string(),
                thread_id: None,
                parent_id: Some(msg.channel_id.get().to_string()),
                origin_event_id: None,
            })
        }
        Err(e) => Err(e),
    }
}

/// Detect Discord's "A thread has already been created for this message" error
/// (JSON error code 160004). Triggered when two bots responding to the same
/// @-mention race to create a thread from the same trigger message.
///
/// Uses string matching because serenity surfaces Discord API errors as
/// formatted strings — there is no structured error code we can match on.
/// Unit tests pin the expected patterns so serenity formatting changes are caught.
fn is_thread_already_exists_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("160004") || msg.contains("already been created")
}

static ROLE_MENTION_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<@&\d+>").unwrap());

fn resolve_mentions(content: &str, bot_id: UserId, allowed_role_ids: &HashSet<u64>) -> String {
    // 1. Strip the bot's own trigger mention
    let out = content
        .replace(&format!("<@{}>", bot_id), "")
        .replace(&format!("<@!{}>", bot_id), "");
    // 2. Strip allowed role mentions (they triggered the bot, not useful in prompt)
    let out = if allowed_role_ids.is_empty() {
        out
    } else {
        allowed_role_ids
            .iter()
            .fold(out, |s, id| s.replace(&format!("<@&{}>", id), ""))
    };
    // 3. Other user mentions: keep <@UID> as-is so the LLM can mention back
    // 4. Fallback: replace remaining role mentions only (user mentions are preserved)
    let out = ROLE_MENTION_RE.replace_all(&out, "@(role)").to_string();
    out.trim().to_string()
}

fn video_attachment_block(
    filename: &str,
    content_type: Option<&str>,
    size: u64,
    url: &str,
) -> ContentBlock {
    ContentBlock::Text {
        text: format!(
            "[Video attachment]\nfilename: {}\ncontent_type: {}\nsize_bytes: {}\nurl: {}",
            filename,
            content_type.unwrap_or("unknown"),
            size,
            url
        ),
    }
}

fn file_attachment_block(
    filename: &str,
    content_type: Option<&str>,
    size: u64,
    url: &str,
) -> ContentBlock {
    ContentBlock::Text {
        text: format!(
            "[File attachment: {} ({}, {} bytes) — download URL: {}]",
            filename,
            content_type.unwrap_or("unknown"),
            size,
            url
        ),
    }
}

/// Build a `SenderContext` for Discord messages.
///
/// Pure function extracted from `EventHandler::message` for testability.
/// When `thread_parent_id` is `Some`, the message is inside a thread:
/// - `channel_id` → parent channel (where the thread lives)
/// - `thread_id`  → thread's own channel ID
///
/// This mirrors Slack's model where `channel_id` is always the parent
/// channel and `thread_id` (thread_ts) identifies the thread.
///
/// Note: `ChannelRef.channel_id` uses the *opposite* convention — it holds
/// the thread's channel ID for routing (Discord API sends to thread by its
/// channel ID). See `ChannelRef` doc comments for details.
#[allow(clippy::too_many_arguments)]
fn build_sender_context(
    sender_id: &str,
    sender_name: &str,
    display_name: &str,
    msg_channel_id: &str,
    thread_parent_id: Option<&str>,
    is_bot: bool,
    timestamp: &str,
    message_id: &str,
    receiver_id: &str,
) -> SenderContext {
    SenderContext {
        schema: "openab.sender.v1".into(),
        sender_id: sender_id.to_string(),
        sender_name: sender_name.to_string(),
        display_name: display_name.to_string(),
        channel: "discord".into(),
        channel_id: thread_parent_id.unwrap_or(msg_channel_id).to_string(),
        thread_id: thread_parent_id.map(|_| msg_channel_id.to_string()),
        is_bot,
        timestamp: Some(timestamp.to_string()),
        message_id: Some(message_id.to_string()),
        receiver_id: Some(receiver_id.to_string()),
        recovered_context: None,
    }
}

fn recovered_discord_message(message: &Message) -> RecoveredMessage {
    RecoveredMessage {
        channel_id: message.channel_id.to_string(),
        thread_id: None,
        message_id: message.id.to_string(),
        sender_id: Some(message.author.id.to_string()),
        sender_name: Some(
            message
                .author
                .global_name
                .as_ref()
                .unwrap_or(&message.author.name)
                .clone(),
        ),
        timestamp: message.timestamp.to_rfc3339(),
        content: message.content.clone(),
        attachment_count: message.attachments.len(),
        relations: Vec::new(),
    }
}

fn discord_context_guild_channel_allowed(
    actual_guild_id: u64,
    expected_guild_id: u64,
    target_channel_id: u64,
    is_thread: bool,
    parent_channel_id: Option<u64>,
    allow_all_channels: bool,
    allowed_channels: &HashSet<u64>,
) -> bool {
    actual_guild_id == expected_guild_id
        && (allow_all_channels
            || allowed_channels.contains(&target_channel_id)
            || (is_thread
                && parent_channel_id.is_some_and(|parent| allowed_channels.contains(&parent))))
}

async fn discord_context_target_allowed(
    http: &Http,
    current_guild_id: Option<u64>,
    current_channel_id: ChannelId,
    link: &DiscordMessageLink,
    allow_all_channels: bool,
    allowed_channels: &HashSet<u64>,
) -> serenity::Result<bool> {
    let target_channel_id = match link.channel_id.parse::<u64>() {
        Ok(value) => ChannelId::new(value),
        Err(_) => return Ok(false),
    };

    if link.guild_id == "@me" {
        return Ok(current_guild_id.is_none() && target_channel_id == current_channel_id);
    }

    let target_guild_id = match link.guild_id.parse::<u64>() {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    if current_guild_id != Some(target_guild_id) {
        return Ok(false);
    }
    if target_channel_id == current_channel_id {
        return Ok(true);
    }

    match target_channel_id.to_channel(http).await? {
        serenity::model::channel::Channel::Guild(channel) => {
            Ok(discord_context_guild_channel_allowed(
                channel.guild_id.get(),
                target_guild_id,
                target_channel_id.get(),
                channel.thread_metadata.is_some(),
                channel.parent_id.map(|parent| parent.get()),
                allow_all_channels,
                allowed_channels,
            ))
        }
        _ => Ok(false),
    }
}

async fn recover_discord_context(
    http: &Http,
    message: &Message,
    config: &ContextRecoveryConfig,
    allow_all_channels: bool,
    allowed_channels: &HashSet<u64>,
) -> Option<RecoveredContext> {
    if !config.enabled {
        return None;
    }

    if config.settle_delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(config.settle_delay_ms)).await;
    }

    let mut collector = ContextCollector::new(config);
    let current_guild_id = message.guild_id.map(|guild_id| guild_id.get());

    if let Some(referenced) = message.referenced_message.as_deref() {
        let reply_link = DiscordMessageLink {
            guild_id: referenced
                .guild_id
                .or(message.guild_id)
                .map_or_else(|| "@me".to_string(), |guild_id| guild_id.to_string()),
            channel_id: referenced.channel_id.to_string(),
            message_id: referenced.id.to_string(),
        };
        match discord_context_target_allowed(
            http,
            current_guild_id,
            message.channel_id,
            &reply_link,
            allow_all_channels,
            allowed_channels,
        )
        .await
        {
            Ok(true) => collector.add(recovered_discord_message(referenced), "native_reply"),
            Ok(false) => collector.failure(
                "native_reply",
                "disallowed_target",
                Some(format!(
                    "discord:{}:{}",
                    referenced.channel_id, referenced.id
                )),
            ),
            Err(error) => {
                tracing::debug!(
                    channel_id = %referenced.channel_id,
                    message_id = %referenced.id,
                    error = %error,
                    "Discord context recovery could not inspect embedded reply target"
                );
                collector.failure(
                    "native_reply",
                    "api_error",
                    Some(format!(
                        "discord:{}:{}",
                        referenced.channel_id, referenced.id
                    )),
                );
            }
        }
    } else if let Some(reference) = &message.message_reference {
        if let Some(message_id) = reference.message_id {
            let reply_link = DiscordMessageLink {
                guild_id: reference
                    .guild_id
                    .or(message.guild_id)
                    .map_or_else(|| "@me".to_string(), |guild_id| guild_id.to_string()),
                channel_id: reference.channel_id.to_string(),
                message_id: message_id.to_string(),
            };
            match discord_context_target_allowed(
                http,
                current_guild_id,
                message.channel_id,
                &reply_link,
                allow_all_channels,
                allowed_channels,
            )
            .await
            {
                Ok(true) => match reference.channel_id.message(http, message_id).await {
                    Ok(referenced) => {
                        collector.add(recovered_discord_message(&referenced), "native_reply")
                    }
                    Err(error) => {
                        tracing::debug!(
                            channel_id = %reference.channel_id,
                            message_id = %message_id,
                            error = %error,
                            "Discord context recovery could not fetch reply target"
                        );
                        collector.failure(
                            "native_reply",
                            "api_error",
                            Some(format!("discord:{}:{}", reference.channel_id, message_id)),
                        );
                    }
                },
                Ok(false) => collector.failure(
                    "native_reply",
                    "disallowed_target",
                    Some(format!("discord:{}:{}", reference.channel_id, message_id)),
                ),
                Err(error) => {
                    tracing::debug!(
                        channel_id = %reference.channel_id,
                        message_id = %message_id,
                        error = %error,
                        "Discord context recovery could not inspect reply target"
                    );
                    collector.failure(
                        "native_reply",
                        "api_error",
                        Some(format!("discord:{}:{}", reference.channel_id, message_id)),
                    );
                }
            }
        }
    }

    match message
        .channel_id
        .messages(
            http,
            GetMessages::new()
                .around(message.id)
                .limit(config.history_limit as u8),
        )
        .await
    {
        Ok(mut messages) => {
            messages.sort_unstable_by_key(|candidate| candidate.id);
            for candidate in messages {
                if candidate.id != message.id {
                    collector.add(recovered_discord_message(&candidate), "current_window");
                }
            }
        }
        Err(error) => {
            tracing::debug!(
                channel_id = %message.channel_id,
                message_id = %message.id,
                error = %error,
                "Discord context recovery could not fetch current window"
            );
            collector.failure(
                "current_window",
                "api_error",
                Some(format!("discord:{}:{}", message.channel_id, message.id)),
            );
        }
    }

    for link in discord_message_links(&message.content, config.link_limit) {
        let source_ref = format!("discord:{}:{}", link.channel_id, link.message_id);
        let target_channel_id = match link.channel_id.parse::<u64>() {
            Ok(value) => ChannelId::new(value),
            Err(_) => {
                collector.failure("linked_target", "invalid_target", Some(source_ref));
                continue;
            }
        };
        let target_message_id = match link.message_id.parse::<u64>() {
            Ok(value) => MessageId::new(value),
            Err(_) => {
                collector.failure("linked_target", "invalid_target", Some(source_ref));
                continue;
            }
        };
        if target_channel_id == message.channel_id && target_message_id == message.id {
            continue;
        }

        match discord_context_target_allowed(
            http,
            current_guild_id,
            message.channel_id,
            &link,
            allow_all_channels,
            allowed_channels,
        )
        .await
        {
            Ok(false) => {
                collector.failure("linked_target", "disallowed_target", Some(source_ref));
                continue;
            }
            Err(error) => {
                tracing::debug!(
                    channel_id = %target_channel_id,
                    message_id = %target_message_id,
                    error = %error,
                    "Discord context recovery could not inspect linked target"
                );
                collector.failure("linked_target", "api_error", Some(source_ref));
                continue;
            }
            Ok(true) => {}
        }

        let limit = (1 + config.link_neighbors.saturating_mul(2)).min(100) as u8;
        match target_channel_id
            .messages(
                http,
                GetMessages::new().around(target_message_id).limit(limit),
            )
            .await
        {
            Ok(mut messages) => {
                messages.sort_unstable_by_key(|candidate| candidate.id);
                let mut found_target = false;
                for candidate in messages {
                    let relation = if candidate.id == target_message_id {
                        found_target = true;
                        "linked_target"
                    } else {
                        "linked_neighbor"
                    };
                    collector.add(recovered_discord_message(&candidate), relation);
                }
                if !found_target {
                    match target_channel_id.message(http, target_message_id).await {
                        Ok(target) => {
                            collector.add(recovered_discord_message(&target), "linked_target")
                        }
                        Err(error) => {
                            tracing::debug!(
                                channel_id = %target_channel_id,
                                message_id = %target_message_id,
                                error = %error,
                                "Discord context recovery could not fetch linked target"
                            );
                            collector.failure("linked_target", "api_error", Some(source_ref));
                        }
                    }
                }
            }
            Err(error) => {
                tracing::debug!(
                    channel_id = %target_channel_id,
                    message_id = %target_message_id,
                    error = %error,
                    "Discord context recovery could not fetch linked window"
                );
                collector.failure("linked_target", "api_error", Some(source_ref));
            }
        }
    }

    collector.finish()
}

/// Pure thread detection: determines whether a channel is a Discord thread
/// in an allowed parent, and whether the bot owns it.
///
/// Returns `(in_allowed_thread, bot_owns)`:
/// - `in_allowed_thread`: true only if the channel IS a thread AND its parent
///   is permitted (via allowlist, `allow_all_channels`, or `in_allowed_channel`).
/// - `bot_owns`: `None` if the channel is not a thread (ownership is meaningless);
///   `Some(true/false)` if it IS a thread, indicating whether the bot owns it.
///
/// Uses `thread_metadata.is_some()` — the canonical way to identify threads.
/// `parent_id` is NOT reliable for thread detection: category children also
/// have `parent_id` set. `parent_id` is only used here for the allowlist check.
///
/// Discord API refs:
/// - Channel Object (parent_id / thread_metadata fields):
///   https://docs.discord.com/developers/resources/channel#channel-object
/// - Thread Metadata ("thread-specific fields not needed by other channels"):
///   https://docs.discord.com/developers/resources/channel#thread-metadata-object
fn detect_thread(
    has_thread_metadata: bool,
    parent_id: Option<u64>,
    owner_id: Option<u64>,
    bot_id: u64,
    allowed_channels: &HashSet<u64>,
    allow_all_channels: bool,
    in_allowed_channel: bool,
) -> (bool, Option<bool>) {
    if !has_thread_metadata {
        return (false, None);
    }
    let in_allowed_thread = in_allowed_channel
        || allow_all_channels
        || parent_id.is_some_and(|pid| allowed_channels.contains(&pid));
    let bot_owns = owner_id.is_some_and(|oid| oid == bot_id);
    (in_allowed_thread, Some(bot_owns))
}

/// Returns true when the incoming message is a Discord reply to this bot.
async fn references_bot_message(ctx: &Context, msg: &Message, bot_id: UserId) -> bool {
    if msg
        .referenced_message
        .as_ref()
        .is_some_and(|referenced| referenced.author.id == bot_id)
    {
        return true;
    }

    let Some(reference) = &msg.message_reference else {
        return false;
    };
    let Some(message_id) = reference.message_id else {
        return false;
    };

    match reference.channel_id.message(&ctx.http, message_id).await {
        Ok(referenced) => referenced.author.id == bot_id,
        Err(e) => {
            tracing::debug!(
                channel_id = %reference.channel_id,
                message_id = %message_id,
                error = %e,
                "failed to fetch referenced Discord message"
            );
            false
        }
    }
}

/// Returns `true` if the author should be denied by the user allowlist.
/// Bot authors skip this check — they are gated by `allow_bot_messages` + `trusted_bot_ids`.
fn is_denied_user(
    is_bot: bool,
    allow_all_users: bool,
    allowed_users: &HashSet<u64>,
    user_id: u64,
) -> bool {
    !is_bot && !allow_all_users && !allowed_users.contains(&user_id)
}

/// Returns true when the message/interaction is allowed by the Discord guild gate.
/// DMs have no guild ID and are handled by the separate `allow_dm` gate.
fn is_allowed_guild(
    allow_all_guilds: bool,
    allowed_guilds: &HashSet<u64>,
    guild_id: Option<u64>,
) -> bool {
    match guild_id {
        None => true,
        Some(id) => allow_all_guilds || allowed_guilds.contains(&id),
    }
}

/// Pure decision function: should a DM be processed?
/// Returns `true` if the DM should be processed (bot responds).
/// Mirrors the DM gating logic in EventHandler::message:
/// - `allow_dm` must be true
/// - `allowed_users` still applies (checked separately via `is_denied_user`)
/// - DMs bypass `allowed_channels` and `@mention` requirements
#[cfg(test)]
fn should_process_dm(allow_dm: bool) -> bool {
    allow_dm
}

/// Pure decision function: should thread creation be skipped?
/// Returns `true` when the message should reuse the current channel
/// directly (existing thread or DM), `false` when a new thread should
/// be created. Pins the invariant that DMs never call
/// `get_or_create_thread()` — Discord DM channels cannot create threads.
#[cfg(test)]
fn should_skip_thread_creation(in_thread: bool, is_dm: bool) -> bool {
    in_thread || is_dm
}

/// Pure decision function: should this message be processed or ignored?
/// Returns `true` if the message should be processed (bot responds).
/// Extracted from the EventHandler::message gating logic for testability.
#[cfg(test)]
fn should_process_user_message(
    mode: AllowUsers,
    is_mentioned: bool,
    in_thread: bool,
    involved: bool,
    other_bot_present: bool,
) -> bool {
    if is_mentioned {
        return true;
    }
    match mode {
        AllowUsers::Mentions => false,
        AllowUsers::Involved => in_thread && involved,
        AllowUsers::MultibotMentions => {
            if !in_thread || !involved {
                return false;
            }
            !other_bot_present
        }
    }
}

fn cached_multibot_thread(
    cache: &HashMap<String, tokio::time::Instant>,
    thread_channel_id: &str,
    ttl: std::time::Duration,
) -> bool {
    cache
        .get(thread_channel_id)
        .is_some_and(|ts| ts.elapsed() < ttl)
}

/// Returns true if any bot message in `messages` contains a turn limit warning.
/// Used to dedup `WarnAndStop` across multiple bot processes sharing a thread. (#530)
/// Note: this is best-effort — a narrow race window exists where two bots fetch
/// simultaneously and both see no warning, resulting in a duplicate. For most
/// deployments this is acceptable; strict once-only semantics would require
/// shared state (e.g. gateway-owned emission or distributed lock).
///
/// Accepts `(is_bot, content)` pairs so the logic can be unit-tested without
/// constructing `serenity::model::channel::Message` values (see existing test
/// boundary comment at `format_thread_export`).
fn turn_limit_warning_present(messages: &[(bool, &str)]) -> bool {
    messages
        .iter()
        .any(|(is_bot, content)| *is_bot && content.contains(BOT_TURN_LIMIT_WARNING_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_turns::{TurnResult, BOT_TURN_LIMIT_WARNING_PREFIX, HARD_BOT_TURN_LIMIT};

    // --- resolve_mentions tests ---

    /// Bot's own <@UID> mention is stripped from the prompt.
    #[test]
    fn resolve_mentions_strips_bot_mention() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("hello <@111> world", bot_id, &HashSet::new());
        assert_eq!(result, "hello  world");
    }

    /// Bot's own legacy <@!UID> mention is also stripped.
    #[test]
    fn resolve_mentions_strips_bot_mention_legacy() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("hello <@!111> world", bot_id, &HashSet::new());
        assert_eq!(result, "hello  world");
    }

    /// Other users' <@UID> mentions are preserved so the LLM can mention them back.
    #[test]
    fn resolve_mentions_preserves_other_user_mentions() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("<@111> say hi to <@222>", bot_id, &HashSet::new());
        assert_eq!(result, "say hi to <@222>");
    }

    /// Role mentions <@&UID> are replaced with @(role) placeholder.
    #[test]
    fn resolve_mentions_replaces_role_mentions() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("hello <@&999>", bot_id, &HashSet::new());
        assert_eq!(result, "hello @(role)");
    }

    /// Message containing only the bot mention results in empty string.
    #[test]
    fn resolve_mentions_empty_after_strip() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("<@111>", bot_id, &HashSet::new());
        assert_eq!(result, "");
    }

    /// Allowed role mentions are stripped from prompt (not replaced with @(role)).
    #[test]
    fn resolve_mentions_strips_allowed_role() {
        let bot_id = UserId::new(111);
        let roles: HashSet<u64> = [999].into_iter().collect();
        let result = resolve_mentions("hello <@&999> world", bot_id, &roles);
        assert_eq!(result, "hello  world");
    }

    /// Non-allowed role mentions are still replaced with @(role).
    #[test]
    fn resolve_mentions_keeps_other_roles_as_placeholder() {
        let bot_id = UserId::new(111);
        let roles: HashSet<u64> = [999].into_iter().collect();
        let result = resolve_mentions("<@&999> check <@&888>", bot_id, &roles);
        assert_eq!(result, "check @(role)");
    }

    #[test]
    fn video_attachment_block_includes_actionable_metadata() {
        let block = video_attachment_block(
            "demo.mp4",
            Some("video/mp4"),
            12345,
            "https://cdn.discordapp.com/attachments/demo.mp4",
        );

        let ContentBlock::Text { text } = block else {
            panic!("video attachments must be forwarded as text metadata");
        };

        assert!(text.contains("[Video attachment]"));
        assert!(text.contains("filename: demo.mp4"));
        assert!(text.contains("content_type: video/mp4"));
        assert!(text.contains("size_bytes: 12345"));
        assert!(text.contains("url: https://cdn.discordapp.com/attachments/demo.mp4"));
    }

    #[test]
    fn file_attachment_block_includes_actionable_metadata() {
        let block = file_attachment_block(
            "document.pdf",
            Some("application/pdf"),
            12345,
            "https://cdn.discordapp.com/attachments/document.pdf",
        );

        let ContentBlock::Text { text } = block else {
            panic!("file attachments must be forwarded as text metadata");
        };

        assert_eq!(
            text,
            "[File attachment: document.pdf (application/pdf, 12345 bytes) — download URL: https://cdn.discordapp.com/attachments/document.pdf]"
        );
    }

    // --- thread-race error detection ---

    /// Detects the Discord error code for "thread already exists" (160004).
    #[test]
    fn is_thread_already_exists_matches_code() {
        let err = anyhow::Error::msg(
            r#"HTTP error: {"code": 160004, "message": "A thread has already been created for this message."}"#,
        );
        assert!(is_thread_already_exists_error(&err));
    }

    /// Detects the human-readable form of the error in case serenity renders
    /// it without the numeric code.
    #[test]
    fn is_thread_already_exists_matches_message() {
        let err = anyhow::anyhow!("A thread has already been created for this message.");
        assert!(is_thread_already_exists_error(&err));
    }

    /// Unrelated errors do not match — we don't want the fallback path
    /// swallowing real failures like permission denied.
    #[test]
    fn is_thread_already_exists_ignores_other_errors() {
        let err = anyhow::anyhow!("Missing Permissions");
        assert!(!is_thread_already_exists_error(&err));
        let err = anyhow::anyhow!("rate limit exceeded");
        assert!(!is_thread_already_exists_error(&err));
    }

    // --- thread export helpers ---

    #[test]
    fn sanitize_filename_component_keeps_safe_ascii() {
        assert_eq!(
            sanitize_filename_component("release notes_v2"),
            "release-notes_v2"
        );
    }

    #[test]
    fn sanitize_filename_component_falls_back_for_empty_result() {
        let result = sanitize_filename_component("///...");
        // Fallback is a YYYYMMDD date string
        assert_eq!(result.len(), 8);
        assert!(result.chars().all(|c| c.is_ascii_digit()));
    }

    // --- assemble_export ---
    // Split out from format_thread_export so we can test the truncation
    // boundary without constructing serenity::model::channel::Message values.

    #[test]
    fn assemble_export_empty_entries_returns_header_only() {
        let (out, written, truncated) = assemble_export("HDR\n", &[], 1024);
        assert_eq!(out, "HDR\n");
        assert_eq!(written, 0);
        assert!(!truncated);
    }

    #[test]
    fn assemble_export_single_oversized_entry_writes_zero_and_marks_truncated() {
        let entries = vec!["x".repeat(200)];
        let (out, written, truncated) = assemble_export("h\n", &entries, 50);
        assert_eq!(written, 0);
        assert!(truncated);
        // Footer needs ~56 bytes; max_bytes 50 leaves ≤48 of room, so it is
        // intentionally omitted (it can't be appended without exceeding the
        // limit). The header is still present.
        assert!(out.starts_with("h\n"));
        assert!(!out.contains("xx"));
    }

    #[test]
    fn assemble_export_entry_at_exact_boundary_is_included() {
        // header(2) + entry(3) == max_bytes(5); the strict-greater check
        // keeps the entry in.
        let (out, written, truncated) = assemble_export("h\n", &["abc".to_string()], 5);
        assert_eq!(written, 1);
        assert!(!truncated);
        assert_eq!(out, "h\nabc");
    }

    #[test]
    fn assemble_export_entry_one_byte_over_boundary_is_excluded() {
        // header(2) + entry(4) == 6 > max_bytes(5); entry is dropped.
        let (out, written, truncated) = assemble_export("h\n", &["abcd".to_string()], 5);
        assert_eq!(written, 0);
        assert!(truncated);
        assert!(out.starts_with("h\n"));
        assert!(!out.contains("abcd"));
    }

    #[test]
    fn assemble_export_appends_footer_when_room_remains() {
        // First two short entries fit; the long third entry would overflow,
        // and the remaining headroom is enough for the truncation footer.
        let entries = vec!["a\n".to_string(), "b\n".to_string(), "c".repeat(500)];
        let (out, written, truncated) = assemble_export("h\n", &entries, 200);
        assert_eq!(written, 2);
        assert!(truncated);
        assert!(out.contains("[Export truncated"));
    }

    // --- snowflake conversion ---

    #[test]
    fn timestamp_ms_to_snowflake_known_value() {
        // 2026-05-10 00:00:00 UTC = 1778572800000 ms since Unix epoch
        // Discord ms = 1778572800000 - 1420070400000 = 358502400000
        // Snowflake = 358502400000 << 22 = 1503238553600000000 (approx)
        let ts_ms: u64 = 1_778_572_800_000;
        let snowflake = timestamp_ms_to_snowflake(ts_ms);
        // Verify round-trip: extract timestamp back from snowflake
        let extracted_ms = (snowflake.get() >> 22) + DISCORD_EPOCH_MS;
        assert_eq!(extracted_ms, ts_ms);
    }

    #[test]
    fn timestamp_ms_to_snowflake_at_discord_epoch_is_one() {
        // At exactly the Discord epoch, discord_ms=0, shifted=0, clamped to 1
        let snowflake = timestamp_ms_to_snowflake(DISCORD_EPOCH_MS);
        assert_eq!(snowflake.get(), 1);
    }

    #[test]
    fn timestamp_ms_to_snowflake_before_epoch_saturates() {
        // Timestamp before Discord epoch should saturate to 1
        let snowflake = timestamp_ms_to_snowflake(1_000_000_000_000);
        assert_eq!(snowflake.get(), 1);
    }

    // --- ExportFilter cap logic ---

    #[test]
    fn export_filter_default_cap_is_100() {
        // Default (no params) uses Limit(100)
        let filter = ExportFilter::Limit(100);
        let cap = match &filter {
            ExportFilter::Limit(n) => *n,
            _ => THREAD_EXPORT_MESSAGE_LIMIT,
        };
        assert_eq!(cap, 100);
    }

    #[test]
    fn export_filter_all_cap_is_5000() {
        let filter = ExportFilter::All;
        let cap = match &filter {
            ExportFilter::Limit(n) => *n,
            _ => THREAD_EXPORT_MESSAGE_LIMIT,
        };
        assert_eq!(cap, THREAD_EXPORT_MESSAGE_LIMIT);
        assert_eq!(cap, 5000);
    }

    #[test]
    fn export_filter_limit_uses_custom_cap() {
        let filter = ExportFilter::Limit(250);
        let cap = match &filter {
            ExportFilter::Limit(n) => *n,
            _ => THREAD_EXPORT_MESSAGE_LIMIT,
        };
        assert_eq!(cap, 250);
    }

    #[test]
    fn export_filter_after_uses_global_cap() {
        let filter = ExportFilter::After(MessageId::new(123456789));
        let cap = match &filter {
            ExportFilter::Limit(n) => *n,
            _ => THREAD_EXPORT_MESSAGE_LIMIT,
        };
        assert_eq!(cap, THREAD_EXPORT_MESSAGE_LIMIT);
    }

    // --- should_process_user_message tests (GIVEN/WHEN/THEN) ---
    // Tests the multibot-mentions gating logic extracted from EventHandler::message.
    // The bug in #481 was that other bots' messages were filtered by bot gating
    // before multibot detection could run, so the bot never learned the thread
    // was multi-bot and responded without @mention.

    /// GIVEN: multibot-mentions mode, single-bot thread, bot is involved
    /// WHEN:  human sends message without @mention
    /// THEN:  bot responds (natural conversation)
    #[test]
    fn multibot_mentions_single_bot_thread_no_mention() {
        assert!(should_process_user_message(
            AllowUsers::MultibotMentions,
            false, // is_mentioned
            true,  // in_thread
            true,  // involved
            false, // other_bot_present
        ));
    }

    /// GIVEN: multibot-mentions mode, multi-bot thread (other bot has posted)
    /// WHEN:  human sends message without @mention
    /// THEN:  bot does NOT respond (requires @mention in multi-bot thread)
    /// This is the exact scenario from bug #481.
    #[test]
    fn multibot_mentions_multi_bot_thread_no_mention() {
        assert!(!should_process_user_message(
            AllowUsers::MultibotMentions,
            false, // is_mentioned
            true,  // in_thread
            true,  // involved
            true,  // other_bot_present ← another bot posted
        ));
    }

    /// GIVEN: multibot-mentions mode, multi-bot thread
    /// WHEN:  human sends message WITH @mention
    /// THEN:  bot responds (explicit @mention always works)
    #[test]
    fn multibot_mentions_multi_bot_thread_with_mention() {
        assert!(should_process_user_message(
            AllowUsers::MultibotMentions,
            true, // is_mentioned
            true, // in_thread
            true, // involved
            true, // other_bot_present
        ));
    }

    /// GIVEN: multibot-mentions mode, not in a thread (main channel)
    /// WHEN:  human sends message without @mention
    /// THEN:  bot does NOT respond (main channel always requires @mention)
    #[test]
    fn multibot_mentions_main_channel_no_mention() {
        assert!(!should_process_user_message(
            AllowUsers::MultibotMentions,
            false, // is_mentioned
            false, // in_thread (main channel)
            false, // involved
            false, // other_bot_present
        ));
    }

    /// GIVEN: multibot-mentions mode, in thread but bot is NOT involved
    /// WHEN:  human sends message without @mention
    /// THEN:  bot does NOT respond (not participating in this thread)
    #[test]
    fn multibot_mentions_not_involved() {
        assert!(!should_process_user_message(
            AllowUsers::MultibotMentions,
            false, // is_mentioned
            true,  // in_thread
            false, // involved ← bot hasn't posted here
            false, // other_bot_present
        ));
    }

    /// GIVEN: involved mode, multi-bot thread
    /// WHEN:  human sends message without @mention
    /// THEN:  bot responds (involved mode ignores multi-bot status)
    #[test]
    fn involved_mode_ignores_multibot() {
        assert!(should_process_user_message(
            AllowUsers::Involved,
            false, // is_mentioned
            true,  // in_thread
            true,  // involved
            true,  // other_bot_present ← ignored in involved mode
        ));
    }

    /// GIVEN: mentions mode
    /// WHEN:  human sends message without @mention (even in own thread)
    /// THEN:  bot does NOT respond (always requires @mention)
    #[test]
    fn mentions_mode_always_requires_mention() {
        assert!(!should_process_user_message(
            AllowUsers::Mentions,
            false, // is_mentioned
            true,  // in_thread
            true,  // involved
            false, // other_bot_present
        ));
    }

    #[test]
    fn streaming_multibot_cache_uses_destination_thread_key() {
        let ttl = std::time::Duration::from_secs(60);
        let now = tokio::time::Instant::now();
        let mut cache = HashMap::new();
        cache.insert("parent-channel".to_string(), now);

        assert!(
            !cached_multibot_thread(&cache, "new-thread", ttl),
            "a parent channel multibot marker must not force a new thread's first reply to send-once"
        );

        cache.insert("new-thread".to_string(), now);
        assert!(cached_multibot_thread(&cache, "new-thread", ttl));
    }

    #[test]
    fn streaming_multibot_cache_ignores_expired_entries() {
        let ttl = std::time::Duration::from_secs(60);
        let expired = tokio::time::Instant::now() - ttl - std::time::Duration::from_secs(1);
        let mut cache = HashMap::new();
        cache.insert("thread".to_string(), expired);

        assert!(!cached_multibot_thread(&cache, "thread", ttl));
    }

    /// After soft limit fires once (n==20), subsequent bot messages still return
    /// SoftLimit but with n>20. The caller warns only when n==max (exact hit),
    /// preventing warning messages from ping-ponging between bots.
    #[test]
    fn soft_limit_warn_once_semantics() {
        let mut t = BotTurnTracker::new(20);
        for _ in 0..19 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        // n==20: exact hit — caller should send warning
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(20));
        // n==21: past limit — caller should silently return (no warning)
        assert_eq!(t.on_bot_message("t1"), TurnResult::Throttled);
        // n==22: still past — still silent
        assert_eq!(t.on_bot_message("t1"), TurnResult::Throttled);
    }

    /// Hard limit also carries count for warn-once semantics.
    #[test]
    fn hard_limit_warn_once_semantics() {
        let mut t = BotTurnTracker::new(HARD_BOT_TURN_LIMIT);
        for _ in 0..HARD_BOT_TURN_LIMIT - 1 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        // Exact hit — warn
        assert_eq!(t.on_bot_message("t1"), TurnResult::HardLimit);
        // Past — silent
        assert_eq!(t.on_bot_message("t1"), TurnResult::Stopped);
    }

    /// Regression test for #497: system messages (thread created, pin, etc.)
    /// should NOT reset the bot turn counter. The filtering happens at the
    /// call site (MessageType check); this verifies the counter stays put
    /// when on_human_message is never called.
    #[test]
    fn system_message_does_not_reset_counter() {
        let mut t = BotTurnTracker::new(3);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        // No on_human_message (system message filtered out at call site)
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(3));
    }

    // --- build_sender_context tests (regression for #581 → #584) ---
    // PR #583 fixed SenderContext to use parent channel_id when in a thread.
    // These tests verify the pure function extracted from EventHandler::message.

    /// In-thread message: channel_id = parent, thread_id = thread channel ID.
    #[test]
    fn build_sender_context_in_thread() {
        let ctx = build_sender_context(
            "user1",
            "alice",
            "Alice",
            "thread_ch",
            Some("parent_ch"),
            false,
            "2026-05-01T00:00:00Z",
            "msg123",
            "bot99",
        );
        assert_eq!(ctx.channel_id, "parent_ch");
        assert_eq!(ctx.thread_id, Some("thread_ch".to_string()));
        assert_eq!(ctx.channel, "discord");
        assert_eq!(ctx.sender_id, "user1");
        assert!(!ctx.is_bot);
        assert_eq!(ctx.receiver_id, Some("bot99".to_string()));
    }

    /// Non-thread message: channel_id = message channel, thread_id = None.
    #[test]
    fn build_sender_context_not_in_thread() {
        let ctx = build_sender_context(
            "user1",
            "alice",
            "Alice",
            "main_ch",
            None,
            false,
            "2026-05-01T00:00:00Z",
            "msg456",
            "bot99",
        );
        assert_eq!(ctx.channel_id, "main_ch");
        assert_eq!(ctx.thread_id, None);
    }

    /// Bot sender: is_bot flag propagated correctly.
    #[test]
    fn build_sender_context_bot_sender() {
        let ctx = build_sender_context(
            "bot1",
            "mybot",
            "MyBot",
            "ch",
            Some("parent"),
            true,
            "2026-05-01T00:00:00Z",
            "msg789",
            "bot99",
        );
        assert!(ctx.is_bot);
        assert_eq!(ctx.channel_id, "parent");
        assert_eq!(ctx.thread_id, Some("ch".to_string()));
    }

    // --- detect_thread tests (regression for #506 → #518 → #519) ---
    // PR #506 used parent_id.is_some() to detect threads, but category text
    // channels also have parent_id (pointing to the category). This caused
    // the bot to skip thread creation for normal channels inside categories.
    //
    // detect_thread() uses thread_metadata.is_some() — the canonical check
    // per Discord API docs. Table-driven to cover all channel scenarios.

    const BOT: u64 = 1000;
    const OTHER: u64 = 2000;
    const PARENT_CH: u64 = 100;
    const CATEGORY: u64 = 200;

    /// Helper: build an allowed_channels set from a slice.
    fn allowed(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    /// Table-driven: each row is a realistic Discord channel scenario.
    #[test]
    fn detect_thread_table() {
        struct Case {
            name: &'static str,
            has_thread_metadata: bool,
            parent_id: Option<u64>,
            owner_id: Option<u64>,
            bot_id: u64,
            allowed_channels: HashSet<u64>,
            allow_all: bool,
            in_allowed: bool,
            expect: (bool, Option<bool>), // (in_thread, bot_owns)
        }

        let cases = vec![
            // --- Non-thread channels: thread_metadata = None ---
            Case {
                name: "text channel under category (regression #506)",
                has_thread_metadata: false,
                parent_id: Some(CATEGORY), // points to category, NOT a thread
                owner_id: None,
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: true,
                expect: (false, None),
            },
            Case {
                name: "top-level text channel (no category)",
                has_thread_metadata: false,
                parent_id: None,
                owner_id: None,
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: true,
                expect: (false, None),
            },
            Case {
                name: "voice channel under category",
                has_thread_metadata: false,
                parent_id: Some(CATEGORY),
                owner_id: None,
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: false,
                expect: (false, None),
            },
            // --- Thread channels: thread_metadata = Some ---
            Case {
                name: "public thread, parent in allowlist, bot owns",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: Some(BOT),
                bot_id: BOT,
                allowed_channels: allowed(&[PARENT_CH]),
                allow_all: false,
                in_allowed: false,
                expect: (true, Some(true)),
            },
            Case {
                name: "public thread, parent in allowlist, other user owns",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: Some(OTHER),
                bot_id: BOT,
                allowed_channels: allowed(&[PARENT_CH]),
                allow_all: false,
                in_allowed: false,
                expect: (true, Some(false)),
            },
            Case {
                name: "thread, parent NOT in allowlist, not allow_all",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: Some(BOT),
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: false,
                expect: (false, Some(true)),
            },
            Case {
                name: "thread, allow_all_channels = true",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: Some(OTHER),
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: true,
                in_allowed: false,
                expect: (true, Some(false)),
            },
            Case {
                name: "thread, in_allowed_channel = true (parent is the allowed channel)",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: None,
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: true,
                expect: (true, Some(false)),
            },
            // --- Defensive: partial data ---
            Case {
                name: "thread with parent_id = None (defensive, partial API data)",
                has_thread_metadata: true,
                parent_id: None,
                owner_id: Some(BOT),
                bot_id: BOT,
                allowed_channels: allowed(&[PARENT_CH]),
                allow_all: false,
                in_allowed: false,
                expect: (false, Some(true)), // can't verify parent → not allowed, but bot still owns
            },
        ];

        for c in &cases {
            let result = detect_thread(
                c.has_thread_metadata,
                c.parent_id,
                c.owner_id,
                c.bot_id,
                &c.allowed_channels,
                c.allow_all,
                c.in_allowed,
            );
            assert_eq!(result, c.expect, "FAILED: {}", c.name);
        }
    }

    // --- WarnAndStop regression test (#633) ---
    // The WarnAndStop path now delegates to detect_thread(). This test pins
    // the exact scenario from #633: a category child channel whose category
    // ID is in another bot's allowed_channels must NOT be treated as allowed.
    #[test]
    fn detect_thread_rejects_category_child_in_warn_and_stop() {
        let category_id: u64 = 200;
        let allowed = HashSet::from([category_id]);
        // Category child: has parent_id (the category) but NO thread_metadata.
        let (in_thread, _) =
            detect_thread(false, Some(category_id), None, 1000, &allowed, false, false);
        assert!(
            !in_thread,
            "category child must not match allowed_channels via parent_id"
        );
    }

    // --- Per-thread streaming tests (#534) ---
    // Streaming ON by default, OFF when another bot is detected in the thread.

    /// Single bot thread: streaming enabled.
    #[test]
    fn discord_streams_when_no_other_bot() {
        let adapter = super::DiscordAdapter::new(Arc::new(super::Http::new("")));
        assert!(adapter.use_streaming(false));
    }

    /// Multi-bot thread: send-once to avoid edit interference.
    #[test]
    fn discord_no_stream_when_other_bot_present() {
        let adapter = super::DiscordAdapter::new(Arc::new(super::Http::new("")));
        assert!(!adapter.use_streaming(true));
    }

    // --- resolve_channel tests ---

    #[test]
    fn resolve_channel_uses_channel_id_when_no_thread() {
        let ch = ChannelRef {
            platform: "discord".into(),
            channel_id: "111".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: None,
        };
        assert_eq!(DiscordAdapter::resolve_channel(&ch), "111");
    }

    #[test]
    fn resolve_channel_prefers_thread_id_when_set() {
        let ch = ChannelRef {
            platform: "discord".into(),
            channel_id: "111".into(),
            thread_id: Some("222".into()),
            parent_id: None,
            origin_event_id: None,
        };
        assert_eq!(DiscordAdapter::resolve_channel(&ch), "222");
    }

    // --- is_denied_user tests (regression for #604) ---

    /// Human not in allowlist → denied.
    #[test]
    fn denied_user_human_not_in_allowlist() {
        let allowed = HashSet::from([100]);
        assert!(is_denied_user(false, false, &allowed, 999));
    }

    /// Human in allowlist → allowed.
    #[test]
    fn denied_user_human_in_allowlist() {
        let allowed = HashSet::from([100]);
        assert!(!is_denied_user(false, false, &allowed, 100));
    }

    /// Bot not in allowlist → allowed (bots skip user gate). This is the #604 fix.
    #[test]
    fn denied_user_bot_skips_allowlist() {
        let allowed = HashSet::from([100]);
        assert!(!is_denied_user(true, false, &allowed, 999));
    }

    // --- DM gating tests (#656) ---
    // DMs are gated by `allow_dm` config. When allowed, DMs bypass
    // `allowed_channels` and treat the message as implicit @mention.

    /// GIVEN: allow_dm = false
    /// WHEN:  user sends a DM
    /// THEN:  DM is rejected
    #[test]
    fn dm_rejected_when_allow_dm_false() {
        assert!(!should_process_dm(false));
    }

    /// GIVEN: allow_dm = true
    /// WHEN:  user sends a DM
    /// THEN:  DM is accepted
    #[test]
    fn dm_accepted_when_allow_dm_true() {
        assert!(should_process_dm(true));
    }

    /// GIVEN: allow_dm = true, user NOT in allowed_users
    /// WHEN:  user sends a DM
    /// THEN:  user is denied (allowed_users still enforced in DMs)
    #[test]
    fn dm_denied_user_still_enforced() {
        let allowed = HashSet::from([100]);
        // DM passes allow_dm gate, but user gate still applies
        assert!(should_process_dm(true));
        assert!(is_denied_user(false, false, &allowed, 999));
    }

    /// GIVEN: allow_dm = true, user in allowed_users
    /// WHEN:  user sends a DM
    /// THEN:  user is allowed
    #[test]
    fn dm_allowed_user_passes() {
        let allowed = HashSet::from([100]);
        assert!(should_process_dm(true));
        assert!(!is_denied_user(false, false, &allowed, 100));
    }

    /// DMs are treated as implicit @mention — should_process_user_message
    /// is never called for DMs (the `!is_dm` guard skips it).
    /// This test verifies the Involved mode would reject a non-thread,
    /// non-mentioned message — confirming DMs MUST bypass this check.
    #[test]
    fn dm_must_bypass_user_message_gating() {
        // Without the `!is_dm` bypass, a DM would be rejected by Involved mode
        // because is_mentioned=false and in_thread=false.
        assert!(!should_process_user_message(
            AllowUsers::Involved,
            false, // is_mentioned (DMs don't have @mention)
            false, // in_thread (DMs are not threads)
            false, // involved
            false, // other_bot_present
        ));
    }

    // --- Guild gating tests ---

    #[test]
    fn guild_gate_allows_dm_without_guild_id() {
        let allowed = HashSet::from([1284331895190196234]);
        assert!(is_allowed_guild(false, &allowed, None));
    }

    #[test]
    fn guild_gate_allows_any_guild_when_allow_all_true() {
        let allowed = HashSet::new();
        assert!(is_allowed_guild(true, &allowed, Some(42)));
    }

    #[test]
    fn guild_gate_allows_configured_guild() {
        let allowed = HashSet::from([1284331895190196234, 1500052145108418592]);
        assert!(is_allowed_guild(false, &allowed, Some(1500052145108418592)));
    }

    #[test]
    fn guild_gate_rejects_unconfigured_guild() {
        let allowed = HashSet::from([1284331895190196234, 1500052145108418592]);
        assert!(!is_allowed_guild(false, &allowed, Some(42)));
    }

    #[test]
    fn context_link_rejects_forged_guild_even_when_all_channels_allowed() {
        assert!(!discord_context_guild_channel_allowed(
            2,
            1,
            20,
            false,
            None,
            true,
            &HashSet::new(),
        ));
    }

    #[test]
    fn context_link_allows_thread_when_parent_is_allowlisted() {
        assert!(discord_context_guild_channel_allowed(
            1,
            1,
            20,
            true,
            Some(10),
            false,
            &HashSet::from([10]),
        ));
    }

    #[test]
    fn context_link_rejects_category_child_when_only_parent_is_allowlisted() {
        assert!(!discord_context_guild_channel_allowed(
            1,
            1,
            20,
            false,
            Some(10),
            false,
            &HashSet::from([10]),
        ));
    }

    // --- Thread creation skip tests (regression for #656 DM bug) ---
    // Pins the invariant: DMs must never call get_or_create_thread().
    // Discord DM channels do not support thread creation.

    /// GIVEN: is_dm = true, not in a thread
    /// THEN:  skip thread creation (use DM channel directly)
    #[test]
    fn dm_skips_thread_creation() {
        assert!(should_skip_thread_creation(false, true));
    }

    /// GIVEN: already in a thread, not a DM
    /// THEN:  skip thread creation (reuse existing thread)
    #[test]
    fn existing_thread_skips_thread_creation() {
        assert!(should_skip_thread_creation(true, false));
    }

    /// GIVEN: not in a thread, not a DM (normal channel message)
    /// THEN:  do NOT skip — create a new thread
    #[test]
    fn normal_channel_creates_thread() {
        assert!(!should_skip_thread_creation(false, false));
    }

    // --- WarnAndStop dedup tests (#530) ---

    #[test]
    fn dedup_detects_existing_bot_warning() {
        let msg = format!(
            "{} (20/20). A human must reply.",
            BOT_TURN_LIMIT_WARNING_PREFIX
        );
        assert!(turn_limit_warning_present(&[(true, &msg)]));
    }

    #[test]
    fn dedup_ignores_human_warning_text() {
        let msg = format!(
            "{} (20/20). A human must reply.",
            BOT_TURN_LIMIT_WARNING_PREFIX
        );
        assert!(!turn_limit_warning_present(&[(false, &msg)]));
    }

    #[test]
    fn dedup_returns_false_when_no_warning() {
        assert!(!turn_limit_warning_present(&[
            (true, "hello"),
            (false, "world")
        ]));
    }

    #[test]
    fn dedup_returns_false_for_empty_messages() {
        assert!(!turn_limit_warning_present(&[]));
    }
}

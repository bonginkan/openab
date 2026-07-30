use crate::adapter::{ChatAdapter, MessageRef};
use crate::config::{ActivityHeartbeatConfig, ReactionEmojis, ReactionTiming};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex;
use tokio::time::Duration;

const CODING_TOKENS: &[&str] = &["exec", "process", "read", "write", "edit", "bash", "shell"];
const WEB_TOKENS: &[&str] = &[
    "web_search",
    "web_fetch",
    "web-search",
    "web-fetch",
    "browser",
];
const ACTIVITY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(600);

struct ActivityHeartbeatState {
    active_turns: usize,
    handle: Option<tokio::task::JoinHandle<()>>,
}

pub struct ActivityHeartbeatManager {
    adapter: Option<Arc<dyn ChatAdapter>>,
    channel: String,
    content: String,
    interval: Duration,
    state: StdMutex<ActivityHeartbeatState>,
}

pub struct ActivityHeartbeatLease {
    manager: Arc<ActivityHeartbeatManager>,
}

impl Drop for ActivityHeartbeatLease {
    fn drop(&mut self) {
        self.manager.end_turn();
    }
}

impl ActivityHeartbeatManager {
    pub fn new(
        config: &ActivityHeartbeatConfig,
        discord_adapter: Option<Arc<dyn ChatAdapter>>,
    ) -> Arc<Self> {
        Self::with_interval(config, discord_adapter, ACTIVITY_HEARTBEAT_INTERVAL)
    }

    fn with_interval(
        config: &ActivityHeartbeatConfig,
        discord_adapter: Option<Arc<dyn ChatAdapter>>,
        interval: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            adapter: config.enabled.then_some(discord_adapter).flatten(),
            channel: config.channel.clone(),
            content: format!("[{}] 作業中: ACPセッション処理中", config.label),
            interval,
            state: StdMutex::new(ActivityHeartbeatState {
                active_turns: 0,
                handle: None,
            }),
        })
    }

    pub fn begin_turn(self: &Arc<Self>) -> ActivityHeartbeatLease {
        let mut state = self.state.lock().expect("heartbeat state mutex poisoned");
        state.active_turns += 1;
        if state.active_turns == 1 {
            if let Some(adapter) = self.adapter.clone() {
                let channel = crate::adapter::ChannelRef {
                    platform: "discord".to_string(),
                    channel_id: self.channel.clone(),
                    thread_id: None,
                    parent_id: None,
                    origin_event_id: None,
                };
                let content = self.content.clone();
                let interval = self.interval;
                state.handle = Some(tokio::spawn(async move {
                    loop {
                        if let Err(error) = adapter.send_message(&channel, &content).await {
                            tracing::warn!(%error, "activity heartbeat send failed");
                        }
                        tokio::time::sleep(interval).await;
                    }
                }));
            }
        }
        ActivityHeartbeatLease {
            manager: self.clone(),
        }
    }

    fn end_turn(&self) {
        let mut state = self.state.lock().expect("heartbeat state mutex poisoned");
        state.active_turns = state.active_turns.saturating_sub(1);
        if state.active_turns == 0 {
            if let Some(handle) = state.handle.take() {
                handle.abort();
            }
        }
    }
}

fn classify_tool<'a>(name: &str, emojis: &'a ReactionEmojis) -> &'a str {
    let n = name.to_lowercase();
    if WEB_TOKENS.iter().any(|t| n.contains(t)) {
        &emojis.web
    } else if CODING_TOKENS.iter().any(|t| n.contains(t)) {
        &emojis.coding
    } else {
        &emojis.tool
    }
}

struct Inner {
    adapter: Arc<dyn ChatAdapter>,
    message: MessageRef,
    emojis: ReactionEmojis,
    timing: ReactionTiming,
    current: String,
    finished: bool,
    debounce_handle: Option<tokio::task::JoinHandle<()>>,
    stall_soft_handle: Option<tokio::task::JoinHandle<()>>,
    stall_hard_handle: Option<tokio::task::JoinHandle<()>>,
}

pub struct StatusReactionController {
    inner: Arc<Mutex<Inner>>,
    enabled: bool,
}

impl StatusReactionController {
    pub fn new(
        enabled: bool,
        adapter: Arc<dyn ChatAdapter>,
        message: MessageRef,
        emojis: ReactionEmojis,
        timing: ReactionTiming,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                adapter,
                message,
                emojis,
                timing,
                current: String::new(),
                finished: false,
                debounce_handle: None,
                stall_soft_handle: None,
                stall_hard_handle: None,
            })),
            enabled,
        }
    }

    pub async fn set_queued(&self) {
        if !self.enabled {
            return;
        }
        let emoji = { self.inner.lock().await.emojis.queued.clone() };
        self.apply_immediate(&emoji).await;
    }

    /// Record that the queued emoji was already applied by the caller.
    ///
    /// Batched dispatch adds 👀 to each message before creating the controller;
    /// the controller still needs to know the anchor message's current status so
    /// later transitions can remove or move that emoji correctly.
    pub async fn track_existing_queued(&self) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        if inner.current.is_empty() {
            inner.current = inner.emojis.queued.clone();
        }
    }

    pub async fn set_thinking(&self) {
        if !self.enabled {
            return;
        }
        let emoji = { self.inner.lock().await.emojis.thinking.clone() };
        self.schedule_debounced(&emoji).await;
    }

    pub async fn set_tool(&self, tool_name: &str) {
        if !self.enabled {
            return;
        }
        let emoji = {
            let inner = self.inner.lock().await;
            classify_tool(tool_name, &inner.emojis).to_string()
        };
        self.schedule_debounced(&emoji).await;
    }

    pub async fn set_done(&self) {
        if !self.enabled {
            return;
        }
        let emoji = { self.inner.lock().await.emojis.done.clone() };
        self.finish(&emoji).await;
        // Add a random mood face
        let faces = ["😊", "😎", "🫡", "🤓", "😏", "✌️", "💪", "🦾"];
        let face = faces[rand::random::<usize>() % faces.len()];
        let inner = self.inner.lock().await;
        let _ = inner.adapter.add_reaction(&inner.message, face).await;
    }

    pub async fn set_error(&self) {
        if !self.enabled {
            return;
        }
        let emoji = { self.inner.lock().await.emojis.error.clone() };
        self.finish(&emoji).await;
    }

    pub async fn clear(&self) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        cancel_timers(&mut inner);
        let current = inner.current.clone();
        if !current.is_empty() {
            let _ = inner
                .adapter
                .remove_reaction(&inner.message, &current)
                .await;
            inner.current.clear();
        }
    }

    pub async fn move_to_message(&self, message: MessageRef) {
        if !self.enabled {
            return;
        }

        let mut inner = self.inner.lock().await;
        if inner.finished
            || (inner.message.channel == message.channel
                && inner.message.message_id == message.message_id)
        {
            inner.message = message;
            return;
        }

        let old_msg = inner.message.clone();
        let current = inner.current.clone();
        let adapter = inner.adapter.clone();
        inner.message = message.clone();
        drop(inner);

        if current.is_empty() {
            return;
        }

        let _ = adapter.add_reaction(&message, &current).await;
        let _ = adapter.remove_reaction(&old_msg, &current).await;
    }

    async fn apply_immediate(&self, emoji: &str) {
        let mut inner = self.inner.lock().await;
        if inner.finished || emoji == inner.current {
            return;
        }
        cancel_debounce(&mut inner);
        let old = inner.current.clone();
        inner.current = emoji.to_string();
        let adapter = inner.adapter.clone();
        let msg = inner.message.clone();
        let new = emoji.to_string();
        drop(inner);

        let _ = adapter.add_reaction(&msg, &new).await;
        if !old.is_empty() && old != new {
            let _ = adapter.remove_reaction(&msg, &old).await;
        }
        self.reset_stall_timers().await;
    }

    async fn schedule_debounced(&self, emoji: &str) {
        let mut inner = self.inner.lock().await;
        if inner.finished || emoji == inner.current {
            self.reset_stall_timers_inner(&mut inner);
            return;
        }
        cancel_debounce(&mut inner);

        let emoji = emoji.to_string();
        let ctrl = self.inner.clone();
        let debounce_ms = inner.timing.debounce_ms;
        inner.debounce_handle = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
            let mut inner = ctrl.lock().await;
            if inner.finished {
                return;
            }
            let old = inner.current.clone();
            inner.current = emoji.clone();
            let adapter = inner.adapter.clone();
            let msg = inner.message.clone();
            drop(inner);

            let _ = adapter.add_reaction(&msg, &emoji).await;
            if !old.is_empty() && old != emoji {
                let _ = adapter.remove_reaction(&msg, &old).await;
            }
        }));
        self.reset_stall_timers_inner(&mut inner);
    }

    async fn finish(&self, emoji: &str) {
        let mut inner = self.inner.lock().await;
        if inner.finished {
            return;
        }
        inner.finished = true;
        cancel_timers(&mut inner);

        let old = inner.current.clone();
        inner.current = emoji.to_string();
        let adapter = inner.adapter.clone();
        let msg = inner.message.clone();
        let new = emoji.to_string();
        drop(inner);

        let _ = adapter.add_reaction(&msg, &new).await;
        if !old.is_empty() && old != new {
            let _ = adapter.remove_reaction(&msg, &old).await;
        }
    }

    async fn reset_stall_timers(&self) {
        let mut inner = self.inner.lock().await;
        self.reset_stall_timers_inner(&mut inner);
    }

    fn reset_stall_timers_inner(&self, inner: &mut Inner) {
        if let Some(h) = inner.stall_soft_handle.take() {
            h.abort();
        }
        if let Some(h) = inner.stall_hard_handle.take() {
            h.abort();
        }

        let soft_ms = inner.timing.stall_soft_ms;
        let hard_ms = inner.timing.stall_hard_ms;
        let ctrl = self.inner.clone();

        inner.stall_soft_handle = Some(tokio::spawn({
            let ctrl = ctrl.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(soft_ms)).await;
                let mut inner = ctrl.lock().await;
                if inner.finished {
                    return;
                }
                let old = inner.current.clone();
                inner.current = "🥱".to_string();
                let adapter = inner.adapter.clone();
                let msg = inner.message.clone();
                drop(inner);
                let _ = adapter.add_reaction(&msg, "🥱").await;
                if !old.is_empty() && old != "🥱" {
                    let _ = adapter.remove_reaction(&msg, &old).await;
                }
            }
        }));

        inner.stall_hard_handle = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(hard_ms)).await;
            let mut inner = ctrl.lock().await;
            if inner.finished {
                return;
            }
            let old = inner.current.clone();
            inner.current = "😨".to_string();
            let adapter = inner.adapter.clone();
            let msg = inner.message.clone();
            drop(inner);
            let _ = adapter.add_reaction(&msg, "😨").await;
            if !old.is_empty() && old != "😨" {
                let _ = adapter.remove_reaction(&msg, &old).await;
            }
        }));
    }
}

fn cancel_debounce(inner: &mut Inner) {
    if let Some(h) = inner.debounce_handle.take() {
        h.abort();
    }
}

fn cancel_timers(inner: &mut Inner) {
    if let Some(h) = inner.debounce_handle.take() {
        h.abort();
    }
    if let Some(h) = inner.stall_soft_handle.take() {
        h.abort();
    }
    if let Some(h) = inner.stall_hard_handle.take() {
        h.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ChannelRef;
    use anyhow::Result;
    use async_trait::async_trait;

    struct RecordingAdapter {
        events: Arc<Mutex<Vec<(String, String, String)>>>,
        messages: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[async_trait]
    impl ChatAdapter for RecordingAdapter {
        fn platform(&self) -> &'static str {
            "mock"
        }

        fn message_limit(&self) -> usize {
            2000
        }

        async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef> {
            self.messages
                .lock()
                .await
                .push((channel.channel_id.clone(), content.to_string()));
            Ok(MessageRef {
                channel: channel.clone(),
                message_id: "sent".into(),
            })
        }

        async fn create_thread(
            &self,
            channel: &ChannelRef,
            _trigger_msg: &MessageRef,
            _title: &str,
        ) -> Result<ChannelRef> {
            Ok(channel.clone())
        }

        async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()> {
            self.events.lock().await.push((
                "add".into(),
                msg.message_id.clone(),
                emoji.to_string(),
            ));
            Ok(())
        }

        async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()> {
            self.events.lock().await.push((
                "remove".into(),
                msg.message_id.clone(),
                emoji.to_string(),
            ));
            Ok(())
        }

        fn use_streaming(&self, _other_bot_present: bool) -> bool {
            false
        }
    }

    fn channel() -> ChannelRef {
        ChannelRef {
            platform: "mock".into(),
            channel_id: "C".into(),
            thread_id: Some("T".into()),
            parent_id: None,
            origin_event_id: None,
        }
    }

    fn msg(id: &str) -> MessageRef {
        MessageRef {
            channel: channel(),
            message_id: id.into(),
        }
    }

    #[tokio::test]
    async fn move_to_message_transfers_current_reaction() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let adapter: Arc<dyn ChatAdapter> = Arc::new(RecordingAdapter {
            events: events.clone(),
            messages: Arc::new(Mutex::new(Vec::new())),
        });
        let ctrl = StatusReactionController::new(
            true,
            adapter,
            msg("old"),
            ReactionEmojis::default(),
            ReactionTiming::default(),
        );

        ctrl.set_queued().await;
        ctrl.move_to_message(msg("new")).await;

        let events = events.lock().await.clone();
        assert_eq!(
            events,
            vec![
                ("add".to_string(), "old".to_string(), "👀".to_string()),
                ("add".to_string(), "new".to_string(), "👀".to_string()),
                ("remove".to_string(), "old".to_string(), "👀".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn activity_heartbeat_is_shared_and_stops_after_last_turn() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(Vec::new()));
        let adapter: Arc<dyn ChatAdapter> = Arc::new(RecordingAdapter {
            events,
            messages: messages.clone(),
        });
        let manager = ActivityHeartbeatManager::with_interval(
            &ActivityHeartbeatConfig {
                enabled: true,
                channel: "1530491625351151616".into(),
                label: "takodex".into(),
            },
            Some(adapter),
            Duration::from_millis(100),
        );

        let first = manager.begin_turn();
        let second = manager.begin_turn();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            messages.lock().await.as_slice(),
            &[(
                "1530491625351151616".to_string(),
                "[takodex] 作業中: ACPセッション処理中".to_string(),
            )]
        );

        drop(first);
        tokio::time::sleep(Duration::from_millis(110)).await;
        assert_eq!(messages.lock().await.len(), 2);

        drop(second);
        tokio::time::sleep(Duration::from_millis(110)).await;
        assert_eq!(messages.lock().await.len(), 2);
    }
}

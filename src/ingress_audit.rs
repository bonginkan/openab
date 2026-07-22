use serde::{Serialize, Serializer};

pub(crate) const INGRESS_AUDIT_SCHEMA: &str = "openab.ingress-audit.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressDecision {
    UnclassifiedDrop,
    Dispatched,
    Duplicate,
    UnsupportedEvent,
    UnsupportedSubtype,
    GuildDenied,
    ChannelDenied,
    UserDenied,
    BotPolicyDenied,
    BotUntrusted,
    BotTurnLimit,
    LoopCheckFailed,
    SelfMessage,
    MentionRequired,
    ThreadRequired,
    BotNotInvolved,
    MultiBotMentionRequired,
    EmptyContent,
    MalformedEvent,
    ThreadCreateFailed,
    DispatchFailed,
}

impl Serialize for IngressDecision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl IngressDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::UnclassifiedDrop => "unclassified_drop",
            Self::Dispatched => "dispatched",
            Self::Duplicate => "duplicate",
            Self::UnsupportedEvent => "unsupported_event",
            Self::UnsupportedSubtype => "unsupported_subtype",
            Self::GuildDenied => "guild_denied",
            Self::ChannelDenied => "channel_denied",
            Self::UserDenied => "user_denied",
            Self::BotPolicyDenied => "bot_policy_denied",
            Self::BotUntrusted => "bot_untrusted",
            Self::BotTurnLimit => "bot_turn_limit",
            Self::LoopCheckFailed => "loop_check_failed",
            Self::SelfMessage => "self_message",
            Self::MentionRequired => "mention_required",
            Self::ThreadRequired => "thread_required",
            Self::BotNotInvolved => "bot_not_involved",
            Self::MultiBotMentionRequired => "multi_bot_mention_required",
            Self::EmptyContent => "empty_content",
            Self::MalformedEvent => "malformed_event",
            Self::ThreadCreateFailed => "thread_create_failed",
            Self::DispatchFailed => "dispatch_failed",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct IngressAuditRecord {
    schema: &'static str,
    platform: &'static str,
    event_id: String,
    channel_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_id: Option<String>,
    sender_id: String,
    sender_is_bot: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_timestamp: Option<String>,
    event_kind: String,
    content_chars: usize,
    attachment_count: usize,
    route_decision: IngressDecision,
}

impl IngressAuditRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        platform: &'static str,
        event_id: impl Into<String>,
        channel_id: impl Into<String>,
        thread_id: Option<String>,
        scope_id: Option<String>,
        sender_id: impl Into<String>,
        sender_is_bot: bool,
        event_timestamp: Option<String>,
        event_kind: impl Into<String>,
        content_chars: usize,
        attachment_count: usize,
    ) -> Self {
        Self {
            schema: INGRESS_AUDIT_SCHEMA,
            platform,
            event_id: event_id.into(),
            channel_id: channel_id.into(),
            thread_id,
            scope_id,
            sender_id: sender_id.into(),
            sender_is_bot,
            event_timestamp,
            event_kind: event_kind.into(),
            content_chars,
            attachment_count,
            route_decision: IngressDecision::UnclassifiedDrop,
        }
    }
}

/// Emits exactly one metadata-only routing record for an inbound message.
///
/// A guard that leaves scope without an explicit terminal decision records
/// `unclassified_drop`. This keeps early returns and newly added policy paths
/// visible without storing message content or invoking an agent.
pub(crate) struct IngressAuditGuard {
    record: Option<IngressAuditRecord>,
}

impl IngressAuditGuard {
    pub(crate) fn new(record: IngressAuditRecord) -> Self {
        Self {
            record: Some(record),
        }
    }

    pub(crate) fn set_thread_id(&mut self, thread_id: Option<String>) {
        if let Some(record) = self.record.as_mut() {
            record.thread_id = thread_id;
        }
    }

    pub(crate) fn finish(&mut self, decision: IngressDecision) {
        let Some(mut record) = self.record.take() else {
            return;
        };
        record.route_decision = decision;
        emit_ingress_audit(&record);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Option<serde_json::Value> {
        self.record
            .as_ref()
            .and_then(|record| serde_json::to_value(record).ok())
    }
}

impl Drop for IngressAuditGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        emit_ingress_audit(&record);
    }
}

fn emit_ingress_audit(record: &IngressAuditRecord) {
    tracing::info!(
        target: "openab::ingress_audit",
        audit_schema = record.schema,
        platform = record.platform,
        event_id = %record.event_id,
        channel_id = %record.channel_id,
        thread_id = record.thread_id.as_deref().unwrap_or(""),
        scope_id = record.scope_id.as_deref().unwrap_or(""),
        sender_id = %record.sender_id,
        sender_is_bot = record.sender_is_bot,
        event_timestamp = record.event_timestamp.as_deref().unwrap_or(""),
        event_kind = %record.event_kind,
        content_chars = record.content_chars,
        attachment_count = record.attachment_count,
        route_decision = record.route_decision.as_str(),
        "ingress routing decision"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_record_is_metadata_only_and_uses_stable_schema() {
        let mut record = IngressAuditRecord::new(
            "discord",
            "message-1",
            "channel-1",
            Some("thread-1".into()),
            Some("guild-1".into()),
            "sender-1",
            true,
            Some("2026-07-22T14:00:00Z".into()),
            "message",
            19,
            2,
        );
        assert_eq!(record.route_decision, IngressDecision::UnclassifiedDrop);
        record.route_decision = IngressDecision::BotPolicyDenied;

        let value = serde_json::to_value(record).unwrap();
        assert_eq!(value["schema"], INGRESS_AUDIT_SCHEMA);
        assert_eq!(value["route_decision"], "bot_policy_denied");
        assert_eq!(
            IngressDecision::BotPolicyDenied.as_str(),
            "bot_policy_denied"
        );
        assert_eq!(value["content_chars"], 19);
        assert!(value.get("content").is_none());
        assert!(value.get("prompt").is_none());
        assert!(value.get("token").is_none());
    }

    #[test]
    fn guard_accepts_only_one_terminal_decision() {
        let record = IngressAuditRecord::new(
            "slack",
            "event-1",
            "channel-1",
            None,
            None,
            "sender-1",
            false,
            None,
            "message",
            0,
            0,
        );
        let mut guard = IngressAuditGuard::new(record);
        guard.finish(IngressDecision::Dispatched);
        guard.finish(IngressDecision::DispatchFailed);
        assert!(guard.record.is_none());
    }
}

//! Shared bounded context recovery contract for chat adapters.
//!
//! Platform credentials never cross this module boundary. Discord and Slack
//! adapters fetch messages, normalize them into [`RecoveredMessage`], and use
//! [`ContextCollector`] to enforce deduplication and prompt-size budgets.

use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use crate::config::ContextRecoveryConfig;

pub const CONTEXT_SCHEMA: &str = "openab.recovered-context.v1";

#[derive(Clone, Debug, Serialize)]
pub struct RecoveredContext {
    pub schema: String,
    pub messages: Vec<RecoveredMessage>,
    pub incomplete: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<ContextFailure>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecoveredMessage {
    pub channel_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub content: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub attachment_count: usize,
    pub relations: Vec<String>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextFailure {
    pub operation: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordMessageLink {
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackMessageLink {
    pub workspace: String,
    pub channel_id: String,
    pub message_ts: String,
    pub thread_ts: Option<String>,
}

static DISCORD_MESSAGE_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:(?:canary|ptb)\.)?discord(?:app)?\.com/channels/(\d+|@me)/(\d+)/(\d+)")
        .expect("valid Discord message link regex")
});

static SLACK_MESSAGE_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"https?://([A-Za-z0-9.-]+)\.slack\.com/archives/([A-Za-z0-9]+)/p(\d{16,20})(?:\?[^\s<>]*)?",
    )
    .expect("valid Slack message link regex")
});

static SLACK_THREAD_TS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\?|&)thread_ts=(\d{10,}\.?\d*)").expect("valid Slack thread_ts regex")
});

pub fn discord_message_links(content: &str, limit: usize) -> Vec<DiscordMessageLink> {
    let mut seen = HashSet::new();
    DISCORD_MESSAGE_LINK_RE
        .captures_iter(content)
        .filter_map(|captures| {
            let link = DiscordMessageLink {
                guild_id: captures.get(1)?.as_str().to_string(),
                channel_id: captures.get(2)?.as_str().to_string(),
                message_id: captures.get(3)?.as_str().to_string(),
            };
            seen.insert((link.channel_id.clone(), link.message_id.clone()))
                .then_some(link)
        })
        .take(limit)
        .collect()
}

pub fn slack_message_links(content: &str, limit: usize) -> Vec<SlackMessageLink> {
    let mut seen = HashSet::new();
    SLACK_MESSAGE_LINK_RE
        .captures_iter(content)
        .filter_map(|captures| {
            let whole = captures.get(0)?.as_str();
            let message_ts = slack_permalink_timestamp(captures.get(3)?.as_str())?;
            let thread_ts = match SLACK_THREAD_TS_RE
                .captures(whole)
                .and_then(|thread| thread.get(1))
            {
                Some(value) => Some(normalize_slack_timestamp(value.as_str())?),
                None => None,
            };
            let link = SlackMessageLink {
                workspace: captures.get(1)?.as_str().to_ascii_lowercase(),
                channel_id: captures.get(2)?.as_str().to_string(),
                message_ts,
                thread_ts,
            };
            seen.insert((
                link.workspace.clone(),
                link.channel_id.clone(),
                link.message_ts.clone(),
            ))
            .then_some(link)
        })
        .take(limit)
        .collect()
}

fn slack_permalink_timestamp(raw: &str) -> Option<String> {
    if raw.len() < 7 || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let split = raw.len() - 6;
    Some(format!("{}.{}", &raw[..split], &raw[split..]))
}

fn normalize_slack_timestamp(raw: &str) -> Option<String> {
    if let Some((seconds, micros)) = raw.split_once('.') {
        if seconds.len() < 10
            || micros.is_empty()
            || micros.len() > 6
            || !seconds.bytes().all(|byte| byte.is_ascii_digit())
            || !micros.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        return Some(format!("{seconds}.{micros:0<6}"));
    }
    if raw.len() >= 16 {
        return slack_permalink_timestamp(raw);
    }
    None
}

struct CollectedMessage {
    message: RecoveredMessage,
    order: usize,
}

pub struct ContextCollector<'a> {
    config: &'a ContextRecoveryConfig,
    messages: Vec<CollectedMessage>,
    indices: HashMap<(String, String), usize>,
    failures: Vec<ContextFailure>,
    incomplete: bool,
}

impl<'a> ContextCollector<'a> {
    pub fn new(config: &'a ContextRecoveryConfig) -> Self {
        Self {
            config,
            messages: Vec::new(),
            indices: HashMap::new(),
            failures: Vec::new(),
            incomplete: false,
        }
    }

    pub fn add(&mut self, mut message: RecoveredMessage, relation: &str) {
        let key = (message.channel_id.clone(), message.message_id.clone());
        if let Some(index) = self.indices.get(&key).copied() {
            let relations = &mut self.messages[index].message.relations;
            if !relations.iter().any(|value| value == relation) {
                relations.push(relation.to_string());
            }
            return;
        }

        let (content, truncated) = truncate_chars(&message.content, self.config.max_message_chars);
        message.content = content;
        message.relations = vec![relation.to_string()];
        self.incomplete |= truncated;
        let index = self.messages.len();
        self.indices.insert(key, index);
        self.messages.push(CollectedMessage {
            message,
            order: index,
        });
    }

    pub fn failure(&mut self, operation: &str, code: &str, source_ref: Option<String>) {
        self.incomplete = true;
        self.failures.push(ContextFailure {
            operation: operation.to_string(),
            code: code.to_string(),
            source_ref,
        });
    }

    pub fn finish(mut self) -> Option<RecoveredContext> {
        let mut ranked: Vec<usize> = (0..self.messages.len()).collect();
        ranked.sort_by_key(|index| {
            let entry = &self.messages[*index];
            (
                std::cmp::Reverse(message_priority(&entry.message)),
                entry.order,
            )
        });

        let mut remaining = self.config.max_total_chars;
        let mut selected = HashSet::new();
        for index in ranked {
            if remaining == 0 {
                self.incomplete = true;
                break;
            }
            let content_len = self.messages[index].message.content.chars().count();
            if content_len <= remaining {
                remaining -= content_len;
                selected.insert(index);
                continue;
            }
            let (content, _) = truncate_chars(&self.messages[index].message.content, remaining);
            self.messages[index].message.content = content;
            selected.insert(index);
            remaining = 0;
            self.incomplete = true;
        }

        let messages = self
            .messages
            .into_iter()
            .enumerate()
            .filter_map(|(index, entry)| selected.contains(&index).then_some(entry.message))
            .collect();

        Some(RecoveredContext {
            schema: CONTEXT_SCHEMA.to_string(),
            messages,
            incomplete: self.incomplete,
            failures: self.failures,
        })
    }
}

fn message_priority(message: &RecoveredMessage) -> u8 {
    if message.relations.iter().any(|value| {
        matches!(
            value.as_str(),
            "native_reply" | "thread_root" | "linked_target"
        )
    }) {
        3
    } else if message
        .relations
        .iter()
        .any(|value| value == "linked_neighbor")
    {
        2
    } else {
        1
    }
}

fn truncate_chars(value: &str, limit: usize) -> (String, bool) {
    if value.chars().count() <= limit {
        return (value.to_string(), false);
    }
    if limit == 0 {
        return (String::new(), true);
    }
    let marker = "…[truncated]";
    if limit <= marker.chars().count() {
        return (value.chars().take(limit).collect(), true);
    }
    let keep = limit - marker.chars().count();
    let mut out: String = value.chars().take(keep).collect();
    out.push_str(marker);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ContextRecoveryConfig {
        ContextRecoveryConfig {
            enabled: true,
            history_limit: 12,
            link_limit: 4,
            link_neighbors: 2,
            max_message_chars: 20,
            max_total_chars: 24,
            settle_delay_ms: 0,
        }
    }

    fn message(channel: &str, id: &str, content: &str) -> RecoveredMessage {
        RecoveredMessage {
            channel_id: channel.to_string(),
            thread_id: None,
            message_id: id.to_string(),
            sender_id: None,
            sender_name: None,
            timestamp: None,
            content: content.to_string(),
            attachment_count: 0,
            relations: Vec::new(),
        }
    }

    #[test]
    fn parses_discord_links_and_deduplicates() {
        let input = "https://discord.com/channels/1/2/3 and https://canary.discord.com/channels/1/2/3 plus https://discordapp.com/channels/@me/4/5";
        assert_eq!(
            discord_message_links(input, 4),
            vec![
                DiscordMessageLink {
                    guild_id: "1".into(),
                    channel_id: "2".into(),
                    message_id: "3".into(),
                },
                DiscordMessageLink {
                    guild_id: "@me".into(),
                    channel_id: "4".into(),
                    message_id: "5".into(),
                },
            ]
        );
    }

    #[test]
    fn parses_slack_permalink_and_thread_root() {
        let input = "https://acme.slack.com/archives/C123/p1721234567123456?thread_ts=1721234000.654321&cid=C123";
        assert_eq!(
            slack_message_links(input, 4),
            vec![SlackMessageLink {
                workspace: "acme".into(),
                channel_id: "C123".into(),
                message_ts: "1721234567.123456".into(),
                thread_ts: Some("1721234000.654321".into()),
            }]
        );
    }

    #[test]
    fn slack_link_identity_includes_workspace() {
        let input = "https://alpha.slack.com/archives/C123/p1721234567123456 https://beta.slack.com/archives/C123/p1721234567123456";
        let links = slack_message_links(input, 4);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].workspace, "alpha");
        assert_eq!(links[1].workspace, "beta");
    }

    #[test]
    fn collector_deduplicates_and_preserves_relations() {
        let cfg = config();
        let mut collector = ContextCollector::new(&cfg);
        collector.add(message("c", "1", "hello"), "current_window");
        collector.add(message("c", "1", "hello"), "native_reply");
        let context = collector.finish().unwrap();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(
            context.messages[0].relations,
            vec!["current_window", "native_reply"]
        );
    }

    #[test]
    fn collector_prioritizes_linked_target_under_total_budget() {
        let cfg = config();
        let mut collector = ContextCollector::new(&cfg);
        collector.add(message("c", "1", "abcdefghijklmnopqrst"), "current_window");
        collector.add(message("x", "2", "linked-target"), "linked_target");
        let context = collector.finish().unwrap();
        assert!(context.incomplete);
        assert!(context.messages.iter().any(|item| item.message_id == "2"));
    }

    #[test]
    fn failures_survive_without_messages() {
        let cfg = config();
        let mut collector = ContextCollector::new(&cfg);
        collector.failure(
            "linked_message",
            "disallowed_target",
            Some("discord:1:2".into()),
        );
        let context = collector.finish().unwrap();
        assert!(context.incomplete);
        assert!(context.messages.is_empty());
        assert_eq!(context.failures[0].code, "disallowed_target");
    }

    #[test]
    fn empty_success_still_records_that_recovery_ran() {
        let cfg = config();
        let context = ContextCollector::new(&cfg).finish().unwrap();
        assert!(!context.incomplete);
        assert!(context.messages.is_empty());
        assert!(context.failures.is_empty());
    }

    #[test]
    fn collector_marks_per_message_truncation() {
        let cfg = config();
        let mut collector = ContextCollector::new(&cfg);
        collector.add(
            message("c", "1", "abcdefghijklmnopqrstuvwxyz"),
            "linked_target",
        );
        let context = collector.finish().unwrap();
        assert!(context.incomplete);
        assert_eq!(context.messages[0].content.chars().count(), 20);
        assert!(context.messages[0].content.ends_with("[truncated]"));
    }
}

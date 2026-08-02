//! Per-thread bot turn tracking for runaway-loop prevention.
//!
//! Shared between Discord and Slack adapters so both platforms apply the same
//! soft/hard limit semantics. Runs before self-check so a bot's own messages
//! count too — the limit caps the *total* bot messages in a thread, not per-bot.
//!
//! ## The limit is a rate, not a total
//!
//! Counting every bot message since the last human one cannot tell a runaway
//! loop from a long piece of work. A deliberate multi-hour loop — issue,
//! implement, review, fix, re-review — reaches any fixed total eventually and
//! then stops dead until someone happens to type something.
//!
//! That is not hypothetical: on 2026-08-02 three bodies were handed
//! twenty-four hours of work and went quiet within the hour, at
//! `turns=100 max=100` on two channels. Nothing had failed. The processes were
//! healthy and still receiving events; the messages were being dropped at the
//! door, and there was no way back without a human.
//!
//! What actually distinguishes a runaway is **speed**. Bots answering each
//! other as fast as inference allows produce dozens of messages a minute and
//! never stop; a working loop produces a few and makes progress. So the
//! counter is over a window: sustained flooding still trips it and stays
//! tripped, while a slow loop passes and, once the flood stops, the thread
//! recovers on its own instead of waiting to be rescued.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Absolute per-thread cap on bot turns **within the window**.
///
/// A backstop for a mis-set soft limit, not a second opinion about it: at the
/// default window this is a hundred messages a minute, which no working loop
/// produces. A human message still clears it immediately.
pub const HARD_BOT_TURN_LIMIT: u32 = 1000;

/// How far back the counter looks.
///
/// Ten minutes: long enough that a burst cannot be hidden by pausing briefly,
/// short enough that a thread throttled by a genuine runaway is usable again
/// soon after it stops.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(600);

/// Stable prefix used in all bot turn limit warning messages.
/// Referenced by the dedup check in the Discord adapter — changing this
/// string requires updating the dedup check too.
pub const BOT_TURN_LIMIT_WARNING_PREFIX: &str = "⚠️ Bot turn limit reached";

#[derive(Debug, PartialEq, Eq)]
pub enum TurnResult {
    /// Counter below limits — continue normally.
    Ok,
    /// Counter == soft_limit — warn once, then stop.
    SoftLimit(u32),
    /// Counter > soft_limit — silently stop (already warned).
    Throttled,
    /// Counter == HARD_BOT_TURN_LIMIT — warn once, then stop.
    HardLimit,
    /// Counter > HARD_BOT_TURN_LIMIT — silently stop (already warned).
    Stopped,
}

pub struct BotTurnTracker {
    soft_limit: u32,
    window: Duration,
    /// When each recent bot message arrived, oldest first. Entries outside the
    /// window are dropped as they are passed, so this cannot outgrow the hard
    /// limit, and a thread that goes quiet stops occupying the map at all.
    recent: HashMap<String, Vec<Instant>>,
}

impl BotTurnTracker {
    pub fn new(soft_limit: u32) -> Self {
        Self::with_window(soft_limit, DEFAULT_WINDOW)
    }

    pub fn with_window(soft_limit: u32, window: Duration) -> Self {
        Self {
            soft_limit,
            window,
            recent: HashMap::new(),
        }
    }

    pub fn on_bot_message(&mut self, thread_id: &str) -> TurnResult {
        self.on_bot_message_at(thread_id, Instant::now())
    }

    /// The clock is a parameter so the behaviour can be tested without
    /// sleeping through a ten-minute window.
    pub fn on_bot_message_at(&mut self, thread_id: &str, now: Instant) -> TurnResult {
        let window = self.window;
        let arrivals = self.recent.entry(thread_id.to_string()).or_default();
        arrivals.retain(|at| now.duration_since(*at) < window);
        arrivals.push(now);
        let turns = arrivals.len() as u32;
        if turns > HARD_BOT_TURN_LIMIT {
            TurnResult::Stopped
        } else if turns == HARD_BOT_TURN_LIMIT {
            TurnResult::HardLimit
        } else if turns > self.soft_limit {
            TurnResult::Throttled
        } else if turns == self.soft_limit {
            TurnResult::SoftLimit(turns)
        } else {
            TurnResult::Ok
        }
    }

    pub fn on_human_message(&mut self, thread_id: &str) {
        // A human speaking clears the thread outright rather than waiting for
        // the window: they have seen the state and decided to carry on.
        self.recent.remove(thread_id);
    }

    /// High-level decision for a bot message: increments the counter and
    /// returns what the adapter should do. Collapses the warn-once semantics
    /// and user-facing message formatting so Discord/Slack (and future adapters)
    /// don't duplicate the match.
    pub fn classify_bot_message(&mut self, thread_id: &str) -> TurnAction {
        match self.on_bot_message(thread_id) {
            TurnResult::Ok => TurnAction::Continue,
            TurnResult::SoftLimit(n) => TurnAction::WarnAndStop {
                severity: TurnSeverity::Soft,
                turns: n,
                // Say what is actually true now: this clears itself. The old
                // wording promised that only a human could restore the thread,
                // which was correct then and would be a lie today.
                user_message: format!(
                    "{} ({n} messages in {minutes} minutes). \
                     Bot-to-bot messages resume once the rate falls, or \
                     immediately if a human replies here.",
                    BOT_TURN_LIMIT_WARNING_PREFIX,
                    minutes = self.window.as_secs() / 60,
                ),
            },
            TurnResult::HardLimit => TurnAction::WarnAndStop {
                severity: TurnSeverity::Hard,
                turns: HARD_BOT_TURN_LIMIT,
                user_message: format!(
                    "🛑 Hard bot turn limit reached ({HARD_BOT_TURN_LIMIT} messages in \
                     {minutes} minutes). Bot-to-bot messages resume once the rate \
                     falls, or immediately if a human replies here.",
                    minutes = self.window.as_secs() / 60,
                ),
            },
            TurnResult::Throttled | TurnResult::Stopped => TurnAction::SilentStop,
        }
    }
}

/// Log severity hint for `TurnAction::WarnAndStop`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TurnSeverity {
    /// Soft limit — typically logged at `info!`.
    Soft,
    /// Hard absolute cap — typically logged at `warn!`.
    Hard,
}

/// High-level action for a bot message after calling
/// [`BotTurnTracker::classify_bot_message`].
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum TurnAction {
    /// Safe to continue processing this bot message.
    Continue,
    /// Stop processing; if the message did not come from our own bot, the
    /// caller should post `user_message` to the thread so humans see why
    /// the bot went quiet. `turns` is the counter value at the warning
    /// point — useful as a structured log field.
    WarnAndStop {
        severity: TurnSeverity,
        turns: u32,
        user_message: String,
    },
    /// Stop processing silently — the warning was already sent on a previous
    /// turn; further warnings would spam the thread.
    SilentStop,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_turns_increment() {
        let mut t = BotTurnTracker::new(5);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
    }

    #[test]
    fn soft_limit_triggers() {
        let mut t = BotTurnTracker::new(3);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(3));
    }

    #[test]
    fn human_resets_both_counters() {
        let mut t = BotTurnTracker::new(3);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        t.on_human_message("t1");
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(3));
    }

    #[test]
    fn hard_limit_triggers() {
        let mut t = BotTurnTracker::new(HARD_BOT_TURN_LIMIT + 1);
        for _ in 0..HARD_BOT_TURN_LIMIT - 1 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        assert_eq!(t.on_bot_message("t1"), TurnResult::HardLimit);
    }

    #[test]
    fn hard_limit_does_not_fire_at_legacy_100() {
        let mut t = BotTurnTracker::new(HARD_BOT_TURN_LIMIT + 1);
        for i in 1..=100 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok, "turn {i}");
        }
    }

    #[test]
    fn hard_limit_resets_on_human() {
        let mut t = BotTurnTracker::new(HARD_BOT_TURN_LIMIT + 1);
        for _ in 0..HARD_BOT_TURN_LIMIT - 1 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        t.on_human_message("t1");
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
    }

    #[test]
    fn hard_before_soft_when_equal() {
        let mut t = BotTurnTracker::new(HARD_BOT_TURN_LIMIT);
        for _ in 0..HARD_BOT_TURN_LIMIT - 1 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        assert_eq!(t.on_bot_message("t1"), TurnResult::HardLimit);
    }

    #[test]
    fn threads_are_independent() {
        let mut t = BotTurnTracker::new(3);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(3));
        assert_eq!(t.on_bot_message("t2"), TurnResult::Ok);
    }

    #[test]
    fn human_on_unknown_thread_is_noop() {
        let mut t = BotTurnTracker::new(5);
        t.on_human_message("unknown");
    }

    #[test]
    fn two_bot_pingpong_hits_soft_limit() {
        let mut t = BotTurnTracker::new(20);
        for i in 1..20 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok, "turn {i}");
        }
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(20));
    }

    #[test]
    fn two_bot_pingpong_human_resets() {
        let mut t = BotTurnTracker::new(20);
        for _ in 0..15 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        t.on_human_message("t1");
        for _ in 0..15 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        for _ in 0..4 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(20));
    }

    #[test]
    fn soft_limit_warn_once_semantics() {
        let mut t = BotTurnTracker::new(20);
        for _ in 0..19 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(20));
        assert_eq!(t.on_bot_message("t1"), TurnResult::Throttled);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Throttled);
    }

    #[test]
    fn hard_limit_warn_once_semantics() {
        let mut t = BotTurnTracker::new(HARD_BOT_TURN_LIMIT + 1);
        for _ in 0..HARD_BOT_TURN_LIMIT - 1 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        assert_eq!(t.on_bot_message("t1"), TurnResult::HardLimit);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Stopped);
    }

    // System messages (thread created, pin, etc.) must not reset the counter.
    // Filtering happens at the call site; this verifies the counter stays put
    // when on_human_message is never called. Regression for openabdev/openab#497.
    #[test]
    fn system_message_does_not_reset_counter() {
        let mut t = BotTurnTracker::new(3);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(3));
    }

    #[test]
    fn classify_returns_continue_under_limits() {
        let mut t = BotTurnTracker::new(5);
        assert_eq!(t.classify_bot_message("t1"), TurnAction::Continue);
    }

    #[test]
    fn classify_returns_warn_and_stop_on_soft_limit() {
        let mut t = BotTurnTracker::new(3);
        let _ = t.classify_bot_message("t1");
        let _ = t.classify_bot_message("t1");
        assert_eq!(
            t.classify_bot_message("t1"),
            TurnAction::WarnAndStop {
                severity: TurnSeverity::Soft,
                turns: 3,
                // 文言は「人間が返信するしかない」から「速度が落ちれば戻る」へ
                // 変わった。実際に自力で戻るようになったので、以前の文面は
                // そのままだと嘘になる。
                user_message: format!(
                    "{} (3 messages in {minutes} minutes). \
                     Bot-to-bot messages resume once the rate falls, or \
                     immediately if a human replies here.",
                    BOT_TURN_LIMIT_WARNING_PREFIX,
                    minutes = DEFAULT_WINDOW.as_secs() / 60,
                ),
            },
        );
    }

    #[test]
    fn classify_returns_silent_stop_past_soft_limit() {
        let mut t = BotTurnTracker::new(2);
        let _ = t.classify_bot_message("t1");
        let _ = t.classify_bot_message("t1");
        assert_eq!(t.classify_bot_message("t1"), TurnAction::SilentStop);
        assert_eq!(t.classify_bot_message("t1"), TurnAction::SilentStop);
    }

    #[test]
    fn classify_returns_warn_and_stop_on_hard_limit() {
        let mut t = BotTurnTracker::new(HARD_BOT_TURN_LIMIT + 1);
        for _ in 0..HARD_BOT_TURN_LIMIT - 1 {
            let _ = t.classify_bot_message("t1");
        }
        assert_eq!(
            t.classify_bot_message("t1"),
            TurnAction::WarnAndStop {
                severity: TurnSeverity::Hard,
                turns: HARD_BOT_TURN_LIMIT,
                user_message: format!(
                    "🛑 Hard bot turn limit reached ({HARD_BOT_TURN_LIMIT} messages in \
                     {minutes} minutes). Bot-to-bot messages resume once the rate \
                     falls, or immediately if a human replies here.",
                    minutes = DEFAULT_WINDOW.as_secs() / 60,
                ),
            },
        );
        assert_eq!(t.classify_bot_message("t1"), TurnAction::SilentStop);
    }

    #[test]
    fn a_long_running_loop_is_not_stopped_by_its_own_length() {
        // 2026-08-02 に実際に起きた形。24時間の作業を渡した3体が、
        // `turns=100 max=100` で1時間以内に無言になった。プロセスは健全で
        // イベントも届いており、**入口で捨てられていた**。
        //
        // 数分に1往復のループを、丸一日続けても止めないこと。
        let mut tracker = BotTurnTracker::with_window(100, Duration::from_secs(600));
        let start = Instant::now();
        for minute in 0..(24 * 60) {
            let now = start + Duration::from_secs(minute * 60);
            // 1分あたり3通。5体が数分おきに喋る程度の速さ。
            for _ in 0..3 {
                assert_eq!(
                    tracker.on_bot_message_at("thread", now),
                    TurnResult::Ok,
                    "{minute} 分の時点で止まった",
                );
            }
        }
    }

    #[test]
    fn a_flood_still_stops() {
        // 窓にしたことで暴走を素通りさせていないこと。
        let mut tracker = BotTurnTracker::with_window(100, Duration::from_secs(600));
        let now = Instant::now();
        for _ in 0..99 {
            assert_eq!(tracker.on_bot_message_at("thread", now), TurnResult::Ok);
        }
        assert_eq!(
            tracker.on_bot_message_at("thread", now),
            TurnResult::SoftLimit(100),
        );
        assert_eq!(
            tracker.on_bot_message_at("thread", now),
            TurnResult::Throttled,
        );
    }

    #[test]
    fn a_flood_stays_stopped_while_it_continues() {
        // 少し間を置いただけで解除されると、暴走は止まらない。
        let mut tracker = BotTurnTracker::with_window(10, Duration::from_secs(600));
        let start = Instant::now();
        for i in 0..10 {
            let _ = tracker.on_bot_message_at("thread", start + Duration::from_secs(i));
        }
        // 窓の内側でどれだけ待っても、まだ throttled のまま。
        assert_eq!(
            tracker.on_bot_message_at("thread", start + Duration::from_secs(300)),
            TurnResult::Throttled,
        );
    }

    #[test]
    fn a_thread_recovers_once_the_flood_stops() {
        // **人を待たずに戻る。** これが元の実装に無かったもの。
        let mut tracker = BotTurnTracker::with_window(10, Duration::from_secs(600));
        let start = Instant::now();
        for i in 0..12 {
            let _ = tracker.on_bot_message_at("thread", start + Duration::from_secs(i));
        }
        // 最後の1通(start+11s)も窓の外へ出る時刻まで進める。601s だと
        // まだ大半が窓の内側に残る。
        assert_eq!(
            tracker.on_bot_message_at("thread", start + Duration::from_secs(700)),
            TurnResult::Ok,
            "窓を過ぎても解除されない",
        );
    }

    #[test]
    fn a_human_still_clears_it_immediately() {
        // 窓を待たずに再開できる道も残す。
        let mut tracker = BotTurnTracker::with_window(3, Duration::from_secs(600));
        let now = Instant::now();
        for _ in 0..5 {
            let _ = tracker.on_bot_message_at("thread", now);
        }
        tracker.on_human_message("thread");
        assert_eq!(tracker.on_bot_message_at("thread", now), TurnResult::Ok);
    }

    #[test]
    fn a_quiet_thread_stops_taking_up_room() {
        // 窓の外の記録は捨てる。以前は thread ごとの数値が残り続けていた。
        let mut tracker = BotTurnTracker::with_window(100, Duration::from_secs(600));
        let start = Instant::now();
        for i in 0..50 {
            let _ = tracker.on_bot_message_at("thread", start + Duration::from_secs(i));
        }
        let _ = tracker.on_bot_message_at("thread", start + Duration::from_secs(1_000));
        assert_eq!(tracker.recent["thread"].len(), 1, "窓の外が残っている");
    }

    #[test]
    fn classify_is_per_thread_independent() {
        let mut t = BotTurnTracker::new(2);
        assert_eq!(t.classify_bot_message("t1"), TurnAction::Continue);
        assert!(matches!(
            t.classify_bot_message("t1"),
            TurnAction::WarnAndStop {
                severity: TurnSeverity::Soft,
                ..
            },
        ));
        assert_eq!(t.classify_bot_message("t2"), TurnAction::Continue);
        assert!(matches!(
            t.classify_bot_message("t2"),
            TurnAction::WarnAndStop {
                severity: TurnSeverity::Soft,
                ..
            },
        ));
    }

    // End-to-end: human message must fully reset classify behavior on the
    // same thread, including unlocking new `Continue` responses.
    #[test]
    fn classify_resumes_after_human_message() {
        let mut t = BotTurnTracker::new(2);
        let _ = t.classify_bot_message("t1"); // Continue
        assert!(matches!(
            t.classify_bot_message("t1"),
            TurnAction::WarnAndStop { .. },
        ));
        // Without a human message, the next classify is silent.
        assert_eq!(t.classify_bot_message("t1"), TurnAction::SilentStop);
        // Human resets — classify starts at Continue again.
        t.on_human_message("t1");
        assert_eq!(t.classify_bot_message("t1"), TurnAction::Continue);
        assert!(matches!(
            t.classify_bot_message("t1"),
            TurnAction::WarnAndStop {
                severity: TurnSeverity::Soft,
                turns: 2,
                ..
            },
        ));
    }
}

//! A live status line whose every word is backed by an observed event.
//!
//! The reaction machine (`reactions.rs`) already shows a single emoji, which
//! answers "did it hear me" but not "what is it doing" or "how long has it
//! been". With local models queueing for ten-plus seconds behind each other,
//! that gap is where a working agent becomes indistinguishable from a dead one.
//!
//! The rule this module exists to enforce:
//!
//! > **Never render activity that was not observed.** A phase is entered only
//! > by the event that proves it. Time alone never promotes a phase — it can
//! > only report *silence*, and silence is labelled as silence.
//!
//! So there is no "thinking…" while nothing has been heard. Before the first
//! event the line says the request was accepted and nothing has come back yet,
//! which is the whole truth. `Phase::Silent` names the absence of signal rather
//! than dressing it up as work, because "waiting" and "wedged" look identical
//! from outside and only the elapsed time distinguishes them.
//!
//! The decoration is in the glyphs and the layout. The claims are not
//! decorated.

use std::time::Duration;

/// What the agent is doing, as far as we have been *told*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The prompt was sent. Nothing has come back. This is a fact about us,
    /// not about the agent — we do not know whether it has started.
    Accepted,
    /// An `AcpEvent::Thinking` arrived.
    Thinking,
    /// A tool is running: `ToolStart` seen, matching `ToolDone` not yet.
    Tool { title: String },
    /// Nothing has arrived for a while. Reports the wait, claims nothing about
    /// the agent's state, and remembers what we last actually heard.
    Silent { last_known: Box<Phase> },
}

impl Phase {
    fn glyph(&self) -> &'static str {
        match self {
            Phase::Accepted => "⏳",
            Phase::Thinking => "🤔",
            Phase::Tool { .. } => "🔧",
            Phase::Silent { .. } => "…",
        }
    }

    fn label(&self) -> String {
        match self {
            Phase::Accepted => "受け取りました".to_string(),
            Phase::Thinking => "考えています".to_string(),
            Phase::Tool { title } => format!("実行中: {title}"),
            Phase::Silent { last_known } => {
                // Say what we last *heard*, not what we guess is happening.
                match last_known.as_ref() {
                    Phase::Accepted => "応答がまだありません".to_string(),
                    other => format!("反応がありません（直前: {}）", other.label()),
                }
            }
        }
    }
}

/// Observed facts about one turn. Every field is written only by an event or
/// by the clock; nothing is inferred.
#[derive(Debug, Clone)]
pub struct LiveStatus {
    phase: Phase,
    /// Milliseconds since the prompt was sent, as last supplied by the caller.
    elapsed_ms: u64,
    /// Value of `elapsed_ms` when the last event arrived.
    last_event_ms: u64,
    /// Value of `elapsed_ms` when the current phase began.
    phase_since_ms: u64,
    /// Tools that started and have not reported done, in start order.
    running_tools: Vec<(String, String)>,
    /// Tools that reported done, and whether they succeeded.
    finished_tools: Vec<bool>,
    /// Silence longer than this becomes `Phase::Silent`.
    silence_after: Duration,
}

impl LiveStatus {
    pub fn new(silence_after: Duration) -> Self {
        Self {
            phase: Phase::Accepted,
            elapsed_ms: 0,
            last_event_ms: 0,
            phase_since_ms: 0,
            running_tools: Vec::new(),
            finished_tools: Vec::new(),
            silence_after,
        }
    }

    #[cfg(test)]
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    fn enter(&mut self, phase: Phase) {
        if self.phase != phase {
            self.phase_since_ms = self.elapsed_ms;
            self.phase = phase;
        }
    }

    /// Advance the clock. **Only ever demotes to `Silent`.** Time is not
    /// evidence of work, so it may never promote a phase.
    pub fn tick(&mut self, elapsed: Duration) {
        // A clock that goes backwards would make elapsed times lie.
        let elapsed_ms = elapsed.as_millis() as u64;
        self.elapsed_ms = self.elapsed_ms.max(elapsed_ms);
        if matches!(self.phase, Phase::Silent { .. }) {
            return;
        }
        // A running tool is not silence: we were told it started and have not
        // been told it ended, so "running" remains the last thing we know.
        if !self.running_tools.is_empty() {
            return;
        }
        if self.elapsed_ms.saturating_sub(self.last_event_ms) >= self.silence_after.as_millis() as u64
        {
            let last_known = Box::new(self.phase.clone());
            self.enter(Phase::Silent { last_known });
        }
    }

    fn observed(&mut self) {
        self.last_event_ms = self.elapsed_ms;
    }

    pub fn on_thinking(&mut self) {
        self.observed();
        if self.running_tools.is_empty() {
            self.enter(Phase::Thinking);
        }
    }

    pub fn on_tool_start(&mut self, id: &str, title: &str) {
        self.observed();
        // The title is shown verbatim. Renaming it here would mean the line
        // says one thing while the agent did another.
        if let Some(slot) = self.running_tools.iter_mut().find(|(x, _)| x == id) {
            slot.1 = title.to_string();
        } else {
            self.running_tools.push((id.to_string(), title.to_string()));
        }
        let title = self.running_tools.last().map(|(_, t)| t.clone()).unwrap_or_default();
        self.enter(Phase::Tool { title });
    }

    pub fn on_tool_done(&mut self, id: &str, succeeded: bool) {
        self.observed();
        self.running_tools.retain(|(x, _)| x != id);
        self.finished_tools.push(succeeded);
        match self.running_tools.last() {
            // Another tool is still running; keep naming the one still going.
            Some((_, title)) => {
                let title = title.clone();
                self.enter(Phase::Tool { title });
            }
            // We were told a tool finished. We were *not* told what happens
            // next, so we fall back to the weakest claim that stays true:
            // it was working a moment ago.
            None => self.enter(Phase::Thinking),
        }
    }

    /// The line as shown. Returns `None` before anything is worth showing.
    pub fn render(&self) -> String {
        let mut line = format!(
            "{} {} · {}",
            self.phase.glyph(),
            self.phase.label(),
            format_duration(self.elapsed_ms)
        );

        // Counts, not adjectives. Each is something we were told.
        let mut facts: Vec<String> = Vec::new();
        let done = self.finished_tools.len();
        if done > 0 {
            let failed = self.finished_tools.iter().filter(|ok| !**ok).count();
            facts.push(if failed > 0 {
                format!("ツール{done}件（失敗{failed}）")
            } else {
                format!("ツール{done}件")
            });
        }
        if self.running_tools.len() > 1 {
            facts.push(format!("並行{}件", self.running_tools.len()));
        }
        if !facts.is_empty() {
            line.push_str(&format!("　{}", facts.join(" · ")));
        }
        line
    }
}

fn format_duration(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds < 60 {
        format!("{seconds}秒")
    } else {
        format!("{}分{}秒", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> LiveStatus {
        LiveStatus::new(Duration::from_secs(10))
    }

    #[test]
    fn it_does_not_claim_thinking_before_anything_is_heard() {
        // The whole defect this module exists for: a display that asserts the
        // agent is working when nothing has come back yet.
        let mut s = status();
        s.tick(Duration::from_secs(5));
        assert_eq!(s.phase(), &Phase::Accepted);
        assert!(s.render().contains("受け取りました"));
        assert!(!s.render().contains("考えています"));
    }

    #[test]
    fn time_alone_never_promotes_a_phase() {
        // Only events move the state forward. If ticking could advance it,
        // the line would invent activity out of the clock.
        let mut s = status();
        for second in 1..120 {
            s.tick(Duration::from_secs(second));
            assert!(
                matches!(s.phase(), Phase::Accepted | Phase::Silent { .. }),
                "tick promoted the phase to {:?} without an event",
                s.phase()
            );
        }
    }

    #[test]
    fn silence_is_reported_as_silence() {
        let mut s = status();
        s.on_thinking();
        s.tick(Duration::from_secs(9));
        assert_eq!(s.phase(), &Phase::Thinking);
        s.tick(Duration::from_secs(10));
        // It says we stopped hearing, and what we last heard -- it does not
        // keep claiming the agent is thinking.
        match s.phase() {
            Phase::Silent { last_known } => assert_eq!(**last_known, Phase::Thinking),
            other => panic!("expected silence, got {other:?}"),
        }
        assert!(s.render().contains("反応がありません"));
    }

    #[test]
    fn a_running_tool_is_not_silence() {
        // Long tools are the common case (a build, a test run). We were told
        // it started and not told it ended, so "running" is still true.
        let mut s = status();
        s.on_tool_start("1", "bash");
        s.tick(Duration::from_secs(300));
        assert_eq!(s.phase(), &Phase::Tool { title: "bash".into() });
    }

    #[test]
    fn the_tool_name_is_shown_verbatim() {
        let mut s = status();
        s.on_tool_start("1", "npm run test:worker");
        assert!(s.render().contains("npm run test:worker"));
    }

    #[test]
    fn it_names_the_tool_that_is_still_running() {
        let mut s = status();
        s.on_tool_start("1", "bash");
        s.on_tool_start("2", "read");
        s.on_tool_done("1", true);
        assert_eq!(s.phase(), &Phase::Tool { title: "read".into() });
    }

    #[test]
    fn tool_failures_are_not_hidden() {
        let mut s = status();
        s.on_tool_start("1", "bash");
        s.on_tool_done("1", false);
        assert!(s.render().contains("失敗1"));
    }

    #[test]
    fn elapsed_time_never_goes_backwards() {
        let mut s = status();
        s.tick(Duration::from_secs(30));
        s.tick(Duration::from_secs(2));
        assert!(s.render().contains("30秒"));
    }

    #[test]
    fn recovering_from_silence_requires_an_event() {
        let mut s = status();
        s.on_thinking();
        s.tick(Duration::from_secs(20));
        assert!(matches!(s.phase(), Phase::Silent { .. }));
        s.tick(Duration::from_secs(21));
        assert!(matches!(s.phase(), Phase::Silent { .. }), "silence lifted itself");
        s.on_thinking();
        assert_eq!(s.phase(), &Phase::Thinking);
    }

    #[test]
    fn every_rendered_phase_was_caused_by_its_event() {
        // Walk the whole surface: no label may appear unless its event fired.
        let mut s = status();
        assert!(!s.render().contains("実行中"));
        assert!(!s.render().contains("書いています"));
        s.on_thinking();
        assert!(s.render().contains("考えています"));
        assert!(!s.render().contains("実行中"));
        s.on_tool_start("1", "grep");
        assert!(s.render().contains("実行中: grep"));
        s.on_tool_done("1", true);
        assert!(s.render().contains("ツール1件"));
    }

    #[test]
    fn minutes_are_shown_once_past_a_minute() {
        let mut s = status();
        s.on_tool_start("1", "cargo build");
        s.tick(Duration::from_secs(95));
        assert!(s.render().contains("1分35秒"));
    }
}

// ---------------------------------------------------------------------------
// Driving the line
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// Zero-width space, invisible in Discord, prefixed to every status line.
///
/// The steer logic needs to tell "the message still only holds a placeholder"
/// from "there is real output worth keeping". Matching on the rendered words
/// would break the moment the wording changes, so the line carries a mark.
pub const MARKER: &str = "\u{200b}";

/// True when `display` is a status line rather than agent output.
pub fn is_status_line(display: &str) -> bool {
    display.starts_with(MARKER)
}

/// What the receive loop reports. Deliberately narrow: only things the agent
/// actually told us, so the renderer cannot be handed a guess.
#[derive(Debug)]
pub enum StatusEvent {
    Thinking,
    /// The answer has started arriving; the message now belongs to it.
    TextStarted,
    ToolStart { id: String, title: String },
    ToolDone { id: String, succeeded: bool },
}

pub struct LiveStatusHandle {
    tx: Option<mpsc::UnboundedSender<StatusEvent>>,
    handed_over: Arc<AtomicBool>,
}

impl LiveStatusHandle {
    /// A handle that reports nothing, for turns that do not stream.
    pub fn disabled() -> Self {
        Self {
            tx: None,
            handed_over: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn send(&self, event: StatusEvent) {
        // Set synchronously, before the caller writes the composed answer.
        // Going through the channel would leave a window in which a tick could
        // overwrite real output with a status line.
        if matches!(event, StatusEvent::TextStarted) {
            self.handed_over.store(true, Ordering::SeqCst);
        }
        if let Some(tx) = &self.tx {
            // A closed channel means the turn ended; dropping is right.
            let _ = tx.send(event);
        }
    }
}

/// The first thing the streaming message shows.
pub fn initial_line(silence_after: Duration) -> String {
    format!("{MARKER}{}", LiveStatus::new(silence_after).render())
}

/// Own the streaming message until the first response text arrives.
///
/// Writes into the same channel the streaming edit loop already reads, so this
/// adds no message of its own — it replaces what that message says while there
/// is nothing to say yet. Once text starts, it stops writing and the composed
/// answer takes the message over.
pub fn spawn(
    display: watch::Sender<String>,
    tick: Duration,
    silence_after: Duration,
) -> LiveStatusHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<StatusEvent>();
    let handed_over = Arc::new(AtomicBool::new(false));
    let done = handed_over.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let mut status = LiveStatus::new(silence_after);
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(StatusEvent::Thinking) => status.on_thinking(),
                    Some(StatusEvent::ToolStart { id, title }) => {
                        status.on_tool_start(&id, &title)
                    }
                    Some(StatusEvent::ToolDone { id, succeeded }) => {
                        status.on_tool_done(&id, succeeded)
                    }
                    // Text means the answer has started. Stop touching it.
                    Some(StatusEvent::TextStarted) | None => break,
                },
                _ = tokio::time::sleep(tick) => {}
            }
            if done.load(Ordering::SeqCst) {
                break;
            }
            status.tick(started.elapsed());
            if display.send(format!("{MARKER}{}", status.render())).is_err() {
                break;
            }
        }
    });
    LiveStatusHandle {
        tx: Some(tx),
        handed_over,
    }
}

#[cfg(test)]
mod driver_tests {
    use super::*;

    #[test]
    fn a_status_line_is_recognisable() {
        // The steer path must not mistake the line for output worth keeping.
        assert!(is_status_line(&initial_line(Duration::from_secs(10))));
        assert!(!is_status_line("実際の回答です"));
    }

    #[test]
    fn the_marker_is_invisible_rather_than_decorative() {
        // A visible marker would show up in the chat as noise.
        assert_eq!(MARKER.chars().count(), 1);
        assert!(MARKER.starts_with('\u{200b}'));
    }

    #[test]
    fn the_first_line_claims_nothing_about_the_agent() {
        let line = initial_line(Duration::from_secs(10));
        assert!(line.contains("受け取りました"));
        assert!(!line.contains("考えています"));
    }

    #[tokio::test]
    async fn text_hands_the_message_over_immediately() {
        // Set before the caller writes the answer, so no tick can overwrite it.
        let (tx, _rx) = watch::channel(String::new());
        let handle = spawn(tx, Duration::from_millis(5), Duration::from_secs(10));
        handle.send(StatusEvent::TextStarted);
        assert!(handle.handed_over.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn the_line_updates_while_waiting() {
        let (tx, mut rx) = watch::channel(String::new());
        let handle = spawn(tx, Duration::from_millis(10), Duration::from_secs(10));
        handle.send(StatusEvent::Thinking);
        rx.changed().await.unwrap();
        let line = rx.borrow_and_update().clone();
        assert!(is_status_line(&line), "{line}");
        assert!(line.contains("考えています"), "{line}");
    }

    #[tokio::test]
    async fn a_disabled_handle_reports_nothing() {
        let handle = LiveStatusHandle::disabled();
        handle.send(StatusEvent::Thinking);
        assert!(handle.handed_over.load(Ordering::SeqCst));
    }
}

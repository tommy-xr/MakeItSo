//! Batches an agent's `TextDelta`s, so a reply streams in a few messages a second
//! rather than one per token.

use std::time::{Duration, Instant};

use mis_types::AgentEvent;

/// A batch goes this long after its first delta at the latest...
pub const FLUSH_EVERY: Duration = Duration::from_millis(100);
/// ...or once it holds this many bytes.
pub const FLUSH_BYTES: usize = 200;

#[derive(Default)]
pub struct Coalescer {
    held: String,
    /// When the first delta now held arrived.
    since: Option<Instant>,
}

impl Coalescer {
    /// What to forward for `ev`: a delta joins the held batch, which comes back
    /// once full or due; any other event comes back as is, dropping the batch:
    /// a text block's `Text` follows its deltas with all of it.
    pub fn event(&mut self, ev: AgentEvent, now: Instant) -> Option<AgentEvent> {
        let AgentEvent::TextDelta { text } = ev else {
            self.take();
            return Some(ev);
        };
        self.held.push_str(&text);
        let since = *self.since.get_or_insert(now);
        if self.held.len() >= FLUSH_BYTES || now >= since + FLUSH_EVERY {
            return self.flush();
        }
        None
    }

    /// When the held batch is due, if one is held.
    pub fn due(&self) -> Option<Instant> {
        self.since.map(|s| s + FLUSH_EVERY)
    }

    /// The held batch, if any, as one delta.
    pub fn flush(&mut self) -> Option<AgentEvent> {
        let text = self.take();
        (!text.is_empty()).then_some(AgentEvent::TextDelta { text })
    }

    fn take(&mut self) -> String {
        self.since = None;
        std::mem::take(&mut self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(t: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: t.into() }
    }

    #[test]
    fn deltas_batch_until_due_or_full() {
        let t0 = Instant::now();
        let mut c = Coalescer::default();
        assert_eq!(c.due(), None);
        assert_eq!(c.event(delta("Hel"), t0), None);
        assert_eq!(c.event(delta("lo"), t0 + Duration::from_millis(40)), None);
        assert_eq!(c.due(), Some(t0 + FLUSH_EVERY), "timed from the first");
        assert_eq!(c.flush(), Some(delta("Hello")));
        assert_eq!((c.flush(), c.due()), (None, None));
        // Due on arrival: goes at once with what was held.
        c.event(delta("a"), t0);
        assert_eq!(c.event(delta("b"), t0 + FLUSH_EVERY), Some(delta("ab")));
        // Full: goes at once.
        let big = "x".repeat(FLUSH_BYTES);
        assert_eq!(c.event(delta(&big), t0), Some(delta(&big)));
        assert_eq!(c.due(), None);
    }

    #[test]
    fn other_events_pass_and_drop_the_batch_they_supersede() {
        let t0 = Instant::now();
        let mut c = Coalescer::default();
        c.event(delta("Hi th"), t0);
        let text = AgentEvent::Text {
            text: "Hi there".into(),
        };
        assert_eq!(c.event(text.clone(), t0), Some(text));
        assert_eq!((c.flush(), c.due()), (None, None));
        let done = AgentEvent::Finished {
            ok: true,
            summary: None,
        };
        assert_eq!(c.event(done.clone(), t0), Some(done));
    }
}

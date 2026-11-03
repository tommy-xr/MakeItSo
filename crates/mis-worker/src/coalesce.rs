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
    /// What to forward for `ev`, in order: a delta joins the held batch, which
    /// comes back once full or due. A `Text` drops the batch (the block's `Text`
    /// has all of it); any other event comes after it.
    pub fn event(&mut self, ev: AgentEvent, now: Instant) -> Vec<AgentEvent> {
        match ev {
            AgentEvent::TextDelta { text } => {
                self.held.push_str(&text);
                let since = *self.since.get_or_insert(now);
                let due = self.held.len() >= FLUSH_BYTES || now >= since + FLUSH_EVERY;
                if due {
                    self.flush().into_iter().collect()
                } else {
                    vec![]
                }
            }
            AgentEvent::Text { .. } => {
                self.take();
                vec![ev]
            }
            ev => self.flush().into_iter().chain([ev]).collect(),
        }
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
        assert_eq!(c.event(delta("Hel"), t0), []);
        assert_eq!(c.event(delta("lo"), t0 + Duration::from_millis(40)), []);
        assert_eq!(c.due(), Some(t0 + FLUSH_EVERY), "timed from the first");
        assert_eq!(c.flush(), Some(delta("Hello")));
        assert_eq!((c.flush(), c.due()), (None, None));
        // Due on arrival: goes at once with what was held.
        c.event(delta("a"), t0);
        assert_eq!(c.event(delta("b"), t0 + FLUSH_EVERY), [delta("ab")]);
        // Full: goes at once.
        let big = "x".repeat(FLUSH_BYTES);
        assert_eq!(c.event(delta(&big), t0), [delta(&big)]);
        assert_eq!(c.due(), None);
    }

    #[test]
    fn text_drops_the_batch_it_supersedes_and_other_events_follow_it() {
        let t0 = Instant::now();
        let mut c = Coalescer::default();
        c.event(delta("Hi th"), t0);
        let text = AgentEvent::Text {
            text: "Hi there".into(),
        };
        assert_eq!(c.event(text.clone(), t0), [text]);
        assert_eq!((c.flush(), c.due()), (None, None));
        c.event(delta("Hi"), t0);
        let usage = AgentEvent::Usage {
            input: 1,
            output: 2,
        };
        assert_eq!(c.event(usage.clone(), t0), [delta("Hi"), usage.clone()]);
        assert_eq!(c.event(usage.clone(), t0), [usage]);
    }
}

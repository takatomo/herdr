//! When to rebuild a pane's render stream on cores that cannot tell an
//! observer about grid changes inside the stream (`resize_notifications`
//! missing, e.g. 0.1.16). A `size` event arrives on another channel, so its
//! order against the output is unknown; instead of guessing, the pane attaches
//! a fresh observer and starts over from its authoritative first paint.
//! Observer attaches never change the size (precheck4), so this cannot loop.

use std::time::{Duration, Instant};

pub(crate) const RESYNC_DEBOUNCE: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResyncAction {
    None,
    /// Attach a new render stream now; switch to it when it answers.
    Reattach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Live,
    Pending { deadline: Instant },
    Reattaching { again: bool },
}

#[derive(Debug, Clone)]
pub(crate) struct RenderResync {
    state: State,
    debounce: Duration,
}

impl RenderResync {
    pub(crate) fn new(debounce: Duration) -> Self {
        Self {
            state: State::Live,
            debounce,
        }
    }

    pub(crate) fn on_size_hint(&mut self, now: Instant) {
        match &mut self.state {
            State::Live | State::Pending { .. } => {
                self.state = State::Pending {
                    deadline: now + self.debounce,
                };
            }
            State::Reattaching { again } => *again = true,
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Pending { deadline } => Some(deadline),
            State::Live | State::Reattaching { .. } => None,
        }
    }

    pub(crate) fn on_tick(&mut self, now: Instant) -> ResyncAction {
        match self.state {
            State::Pending { deadline } if now >= deadline => {
                self.state = State::Reattaching { again: false };
                ResyncAction::Reattach
            }
            _ => ResyncAction::None,
        }
    }

    pub(crate) fn on_reattached(&mut self, now: Instant) {
        self.state = match self.state {
            State::Reattaching { again: true } => State::Pending {
                deadline: now + self.debounce,
            },
            _ => State::Live,
        };
    }

    pub(crate) fn on_reattach_failed(&mut self) {
        self.state = State::Live;
    }

    pub(crate) fn is_reattaching(&self) -> bool {
        matches!(self.state, State::Reattaching { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Duration = Duration::from_millis(80);

    #[test]
    fn a_size_hint_waits_for_the_debounce_then_reattaches_once() {
        let start = Instant::now();
        let mut resync = RenderResync::new(D);
        assert_eq!(resync.deadline(), None);
        assert_eq!(resync.on_tick(start + D * 5), ResyncAction::None);
        resync.on_size_hint(start);
        assert_eq!(resync.deadline(), Some(start + D));
        assert_eq!(resync.on_tick(start + D / 2), ResyncAction::None);
        assert_eq!(resync.on_tick(start + D), ResyncAction::Reattach);
        assert!(resync.is_reattaching());
        assert_eq!(resync.deadline(), None);
        assert_eq!(resync.on_tick(start + D * 3), ResyncAction::None);
        resync.on_reattached(start + D * 3);
        assert!(!resync.is_reattaching());
        assert_eq!(resync.on_tick(start + D * 10), ResyncAction::None);
    }

    #[test]
    fn hints_in_a_burst_coalesce_into_one_reattach() {
        let start = Instant::now();
        let mut resync = RenderResync::new(D);
        resync.on_size_hint(start);
        resync.on_size_hint(start + D / 2);
        resync.on_size_hint(start + D);
        assert_eq!(resync.on_tick(start + D + D / 2), ResyncAction::None);
        assert_eq!(resync.on_tick(start + D * 2), ResyncAction::Reattach);
    }

    #[test]
    fn a_hint_during_a_reattach_runs_one_more_after_it() {
        let start = Instant::now();
        let mut resync = RenderResync::new(D);
        resync.on_size_hint(start);
        assert_eq!(resync.on_tick(start + D), ResyncAction::Reattach);
        resync.on_size_hint(start + D + D / 4);
        assert_eq!(
            resync.deadline(),
            None,
            "no second attach while one is in flight"
        );
        resync.on_reattached(start + D * 2);
        assert_eq!(resync.deadline(), Some(start + D * 3));
        assert_eq!(resync.on_tick(start + D * 3), ResyncAction::Reattach);
        resync.on_reattached(start + D * 4);
        assert_eq!(resync.deadline(), None);
    }

    #[test]
    fn a_failed_reattach_returns_to_live() {
        let start = Instant::now();
        let mut resync = RenderResync::new(D);
        resync.on_size_hint(start);
        assert_eq!(resync.on_tick(start + D), ResyncAction::Reattach);
        resync.on_reattach_failed();
        assert!(!resync.is_reattaching());
        assert_eq!(resync.deadline(), None);
    }
}

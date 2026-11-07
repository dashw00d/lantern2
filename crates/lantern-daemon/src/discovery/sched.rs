//! Clock-injected demand, freshness and publication-debounce policy.
use std::time::{Duration, Instant};

/// Monotonic time source for the scheduling policy.
pub trait Clock {
    /// Time elapsed on this clock's arbitrary monotonic epoch.
    fn now(&self) -> Duration;
}
/// Production monotonic clock.
#[derive(Debug)]
pub struct MonotonicClock(Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}
/// Scanner demand inferred from consumers and adoption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demand {
    /// At least one live interest: scan every two seconds.
    Active,
    /// Adoption requires observing listeners: scan every five seconds.
    Watching,
    /// No periodic scan is needed.
    Idle,
}
impl Demand {
    /// Poll interval, or none when idle.
    pub fn interval(self) -> Option<Duration> {
        match self {
            Self::Active => Some(Duration::from_secs(2)),
            Self::Watching => Some(Duration::from_secs(5)),
            Self::Idle => None,
        }
    }
}
/// State machine for scheduling scans and coalescing change publication.
#[derive(Debug)]
pub struct ScanPolicy<C> {
    clock: C,
    interests: usize,
    adopted: bool,
    auto_adopt: bool,
    last_scan: Option<Duration>,
    next_scan: Option<Duration>,
    last_publish: Option<Duration>,
    publish_due: Option<Duration>,
}
impl<C: Clock> ScanPolicy<C> {
    /// Schedule the initial scan immediately, even in idle mode.
    pub fn new(clock: C) -> Self {
        let now = clock.now();
        Self {
            clock,
            interests: 0,
            adopted: false,
            auto_adopt: false,
            last_scan: None,
            next_scan: Some(now),
            last_publish: None,
            publish_due: None,
        }
    }
    /// Current demand level.
    pub fn demand(&self) -> Demand {
        if self.interests > 0 {
            Demand::Active
        } else if self.adopted || self.auto_adopt {
            Demand::Watching
        } else {
            Demand::Idle
        }
    }
    /// Set interest count; acquiring a new interest requests an immediate scan.
    pub fn set_interests(&mut self, count: usize) {
        let previous = self.demand();
        let old = self.interests;
        self.interests = count;
        if count > old {
            self.next_scan = Some(self.clock.now());
        } else {
            self.reconfigure(previous);
        }
    }
    /// Set adoption demand and recalculate the next periodic deadline.
    pub fn set_watching(&mut self, adopted: bool, auto_adopt: bool) {
        let previous = self.demand();
        self.adopted = adopted;
        self.auto_adopt = auto_adopt;
        self.reconfigure(previous);
    }
    fn reconfigure(&mut self, previous: Demand) {
        if self.demand() == previous {
            return;
        }
        if self.last_scan.is_none() {
            return;
        }
        self.next_scan = self
            .demand()
            .interval()
            .map(|interval| self.clock.now().saturating_add(interval));
    }
    /// Request an immediate on-demand scan.
    pub fn request_scan(&mut self) {
        self.next_scan = Some(self.clock.now());
    }
    /// The next scan deadline, or none in idle mode after the initial scan.
    pub fn next_scan(&self) -> Option<Duration> {
        self.next_scan
    }
    /// Whether the cached snapshot is too old for an on-demand request.
    pub fn needs_fresh_scan(&self, max_age: Duration) -> bool {
        self.last_scan
            .is_none_or(|last| self.clock.now().saturating_sub(last) >= max_age)
    }
    /// Record scan completion and delay the next tick by the current interval.
    pub fn scanned(&mut self) {
        let now = self.clock.now();
        self.last_scan = Some(now);
        self.next_scan = self
            .demand()
            .interval()
            .map(|interval| now.saturating_add(interval));
    }
    /// Record a changed visible snapshot and arm a 250 ms publication debounce.
    pub fn changed(&mut self) {
        let now = self.clock.now();
        let earliest = self.last_publish.map_or(now, |last| {
            last.saturating_add(Duration::from_millis(250)).max(now)
        });
        self.publish_due = Some(self.publish_due.map_or(earliest, |due| due.min(earliest)));
    }
    /// Deadline for publishing pending changes, if any.
    pub fn publish_deadline(&self) -> Option<Duration> {
        self.publish_due
    }
    /// Publish a pending change if the debounce deadline has elapsed.
    pub fn take_publication(&mut self) -> bool {
        let now = self.clock.now();
        if self.publish_due.is_some_and(|due| due <= now) {
            self.publish_due = None;
            self.last_publish = Some(now);
            true
        } else {
            false
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    #[derive(Debug, Clone, Default)]
    struct Manual(Arc<AtomicU64>);
    impl Manual {
        fn set(&self, ms: u64) {
            self.0.store(ms, Ordering::Relaxed);
        }
    }
    impl Clock for Manual {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::Relaxed))
        }
    }
    #[test]
    fn discovery_scheduler_demand_freshness_and_debounce() {
        let clock = Manual::default();
        let mut p = ScanPolicy::new(clock.clone());
        assert_eq!(p.next_scan(), Some(Duration::ZERO));
        assert!(p.needs_fresh_scan(Duration::from_secs(2)));
        p.scanned();
        assert_eq!(p.next_scan(), None);
        p.set_watching(true, false);
        assert_eq!(p.next_scan(), Some(Duration::from_secs(5)));
        clock.set(1000);
        p.set_interests(1);
        assert_eq!(p.next_scan(), Some(Duration::from_secs(1)));
        p.scanned();
        assert_eq!(p.next_scan(), Some(Duration::from_secs(3)));
        p.changed();
        assert!(p.take_publication());
        clock.set(1100);
        p.changed();
        p.changed();
        assert_eq!(p.publish_deadline(), Some(Duration::from_millis(1250)));
        assert!(!p.take_publication());
        clock.set(1250);
        assert!(p.take_publication());
        p.set_interests(0);
        p.set_watching(false, false);
        assert_eq!(p.next_scan(), None);
        clock.set(3000);
        assert!(p.needs_fresh_scan(Duration::from_secs(2)));
    }
}

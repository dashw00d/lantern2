//! Pure sticky port leases, parameterized by a listener/conflict probe.

use std::collections::{BTreeMap, BTreeSet};

use lantern_core::ids::InstanceId;

/// Inclusive range used when assigning new ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    /// Lowest assignable port.
    pub start: u16,
    /// Highest assignable port.
    pub end: u16,
}

impl Default for PortRange {
    fn default() -> Self {
        Self {
            start: 29000,
            end: 29999,
        }
    }
}

/// Result of an injected check against actual listeners and bindability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortAvailability<H> {
    /// The port is available for the instance to bind.
    Free,
    /// The port is busy; discovery may or may not know its holder.
    Busy(Option<H>),
}

/// Why an existing sticky lease could not be reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviousLeaseReason<H> {
    /// A listener or bind probe found the port in use.
    Busy(Option<H>),
    /// The port is now reserved by settings.
    Reserved,
    /// Another stored instance also claims this port.
    Duplicate,
    /// Port zero is never assignable.
    Invalid,
}

/// Information useful for explaining a re-lease to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousLease<H> {
    /// Former port.
    pub port: u16,
    /// Why the former lease could not be reused.
    pub reason: PreviousLeaseReason<H>,
}

/// Assigned port and any change from an existing sticky lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseOutcome<H> {
    /// Assigned port.
    pub port: u16,
    /// Present only when an earlier lease had to be replaced.
    pub previous: Option<PreviousLease<H>>,
}

/// Failure to allocate from a configured range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError<H, E = std::convert::Infallible> {
    /// A zero or inverted range was supplied.
    InvalidRange,
    /// No unleased, unreserved port passed the injected conflict check.
    Exhausted {
        /// Previous unusable sticky lease and holder, when applicable.
        previous: Option<PreviousLease<H>>,
    },
    /// The injected discovery or bind probe failed; the lease table is unchanged.
    Probe {
        /// Port whose probe failed.
        port: u16,
        /// Original probe error.
        source: E,
    },
}

/// Sticky per-instance lease table; persistence and locking belong to the registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortLeases {
    leases: BTreeMap<InstanceId, u16>,
}

impl PortLeases {
    /// Reconstruct leases from persisted state.
    pub fn from_table(leases: BTreeMap<InstanceId, u16>) -> Self {
        Self { leases }
    }

    /// Borrow the table to persist it.
    pub fn table(&self) -> &BTreeMap<InstanceId, u16> {
        &self.leases
    }

    /// Release a lease when its instance is removed, not on stop or crash.
    pub fn release(&mut self, id: &InstanceId) -> Option<u16> {
        self.leases.remove(id)
    }

    /// Reuse a free sticky lease or allocate the lowest free port in the range.
    ///
    /// The table is unchanged on failure. A prior lease is kept even outside the
    /// current range if it is free and not reserved or duplicated. The caller
    /// supplies a discovery/bind check and must serialize access to this table.
    pub fn allocate<H>(
        &mut self,
        id: &InstanceId,
        range: PortRange,
        reserved: &[u16],
        mut check: impl FnMut(u16) -> PortAvailability<H>,
    ) -> Result<LeaseOutcome<H>, PortError<H>> {
        self.try_allocate(id, range, reserved, |port| {
            Ok::<_, std::convert::Infallible>(check(port))
        })
    }

    /// Allocate while propagating errors from the injected conflict probe.
    ///
    /// A probe error never changes the lease table and is not treated as a free port.
    pub fn try_allocate<H, E>(
        &mut self,
        id: &InstanceId,
        range: PortRange,
        reserved: &[u16],
        mut check: impl FnMut(u16) -> Result<PortAvailability<H>, E>,
    ) -> Result<LeaseOutcome<H>, PortError<H, E>> {
        if range.start == 0 || range.start > range.end {
            return Err(PortError::InvalidRange);
        }
        let other_leases: BTreeSet<_> = self
            .leases
            .iter()
            .filter(|(other, _)| *other != id)
            .map(|(_, port)| *port)
            .collect();
        let previous = if let Some(&port) = self.leases.get(id) {
            let reason = if port == 0 {
                PreviousLeaseReason::Invalid
            } else if reserved.contains(&port) {
                PreviousLeaseReason::Reserved
            } else if other_leases.contains(&port) {
                PreviousLeaseReason::Duplicate
            } else {
                match check(port).map_err(|source| PortError::Probe { port, source })? {
                    PortAvailability::Free => return Ok(LeaseOutcome { port, previous: None }),
                    PortAvailability::Busy(holder) => PreviousLeaseReason::Busy(holder),
                }
            };
            Some(PreviousLease { port, reason })
        } else {
            None
        };
        for port in range.start..=range.end {
            if reserved.contains(&port)
                || other_leases.contains(&port)
                || previous.as_ref().is_some_and(|old| old.port == port)
            {
                continue;
            }
            if matches!(
                check(port).map_err(|source| PortError::Probe { port, source })?,
                PortAvailability::Free
            ) {
                self.leases.insert(id.clone(), port);
                return Ok(LeaseOutcome { port, previous });
            }
        }
        Err(PortError::Exhausted { previous })
    }
}

/// Allocate against a borrowed table without mutating it.
pub fn allocate<H>(
    leases: &BTreeMap<InstanceId, u16>,
    id: &InstanceId,
    range: PortRange,
    reserved: &[u16],
    check: impl FnMut(u16) -> PortAvailability<H>,
) -> Result<LeaseOutcome<H>, PortError<H>> {
    PortLeases::from_table(leases.clone()).allocate(id, range, reserved, check)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> InstanceId {
        InstanceId::parse(name).unwrap()
    }
    fn free(_: u16) -> PortAvailability<&'static str> {
        PortAvailability::Free
    }

    #[test]
    fn sticky_and_lowest_free_excluding_reserved_leased_and_busy() {
        let mut leases = PortLeases::from_table(BTreeMap::from([(id("other"), 39100)]));
        let range = PortRange {
            start: 39100,
            end: 39105,
        };
        let result = leases
            .allocate(&id("app"), range, &[39101], |port| {
                if port == 39102 {
                    PortAvailability::Busy(Some("foreign"))
                } else {
                    PortAvailability::Free
                }
            })
            .unwrap();
        assert_eq!(result.port, 39103);
        assert!(result.previous.is_none());
        let sticky = leases
            .allocate(
                &id("app"),
                PortRange {
                    start: 39104,
                    end: 39105,
                },
                &[],
                free,
            )
            .unwrap();
        assert_eq!(sticky.port, 39103);
        assert_eq!(leases.release(&id("app")), Some(39103));
        assert!(!leases.table().contains_key(&id("app")));
        assert_eq!(
            PortRange::default(),
            PortRange {
                start: 29000,
                end: 29999
            }
        );
    }

    #[test]
    fn occupied_sticky_lease_moves_and_reports_holder() {
        let mut leases = PortLeases::from_table(BTreeMap::from([(id("app"), 39100)]));
        let result = leases
            .allocate(
                &id("app"),
                PortRange {
                    start: 39100,
                    end: 39101,
                },
                &[],
                |port| {
                    if port == 39100 {
                        PortAvailability::Busy(Some(1234))
                    } else {
                        PortAvailability::Free
                    }
                },
            )
            .unwrap();
        assert_eq!(result.port, 39101);
        assert_eq!(
            result.previous,
            Some(PreviousLease {
                port: 39100,
                reason: PreviousLeaseReason::Busy(Some(1234))
            })
        );
        assert_eq!(leases.table()[&id("app")], 39101);
    }

    #[test]
    fn exhaustion_and_invalid_range_preserve_old_lease() {
        let mut leases = PortLeases::from_table(BTreeMap::from([(id("app"), 39100)]));
        let old = leases.clone();
        let result = leases.allocate(
            &id("app"),
            PortRange {
                start: 39100,
                end: 39101,
            },
            &[],
            |_| PortAvailability::Busy(Some("pid")),
        );
        assert_eq!(
            result,
            Err(PortError::Exhausted {
                previous: Some(PreviousLease {
                    port: 39100,
                    reason: PreviousLeaseReason::Busy(Some("pid"))
                })
            })
        );
        assert_eq!(leases, old);
        assert_eq!(
            leases.allocate(&id("app"), PortRange { start: 2, end: 1 }, &[], free),
            Err(PortError::InvalidRange)
        );
        assert_eq!(leases, old);
    }

    #[test]
    fn duplicate_and_newly_reserved_lease_are_not_reused() {
        let range = PortRange {
            start: 39100,
            end: 39102,
        };
        let mut duplicate =
            PortLeases::from_table(BTreeMap::from([(id("app"), 39100), (id("other"), 39100)]));
        let a = duplicate.allocate(&id("app"), range, &[], free).unwrap();
        assert_eq!(a.port, 39101);
        assert_eq!(a.previous.unwrap().reason, PreviousLeaseReason::Duplicate);
        let mut reserved = PortLeases::from_table(BTreeMap::from([(id("app"), 39100)]));
        let b = reserved.allocate(&id("app"), range, &[39100], free).unwrap();
        assert_eq!(b.port, 39101);
        assert_eq!(b.previous.unwrap().reason, PreviousLeaseReason::Reserved);
    }

    #[test]
    fn probe_failure_is_reported_and_does_not_mutate_leases() {
        let mut leases = PortLeases::from_table(BTreeMap::from([(id("app"), 39100)]));
        let old = leases.clone();
        let result: Result<LeaseOutcome<()>, PortError<(), &str>> = leases.try_allocate(
            &id("app"),
            PortRange {
                start: 39100,
                end: 39101,
            },
            &[],
            |port| {
                if port == 39100 {
                    Ok(PortAvailability::Busy(None))
                } else {
                    Err("scan unavailable")
                }
            },
        );
        assert_eq!(
            result,
            Err(PortError::Probe {
                port: 39101,
                source: "scan unavailable"
            })
        );
        assert_eq!(leases, old);
    }
}

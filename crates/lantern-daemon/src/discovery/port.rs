//! Fresh readiness and conflict answers using observed listeners.
use super::procinfo::normalize_unit;
use super::scanner::Snapshot;
use lantern_core::api::PortConflict;
use lantern_core::ids::InstanceId;
use lantern_core::model::PortHolder;
use std::net::SocketAddr;

/// A fresh answer about a TCP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortBinding {
    /// No TCP listener was observed on this port.
    Free,
    /// At least one socket belongs to the expected unit.
    BoundByUnit {
        /// A holder's process id.
        pid: u32,
        /// The expected unit's observed addresses.
        addrs: Vec<SocketAddr>,
    },
    /// The port is held, but not by the expected unit.
    HeldByOther(PortHolder),
}
impl Snapshot {
    /// Explain occupied reservations, including normally hidden listeners.
    pub fn conflicts(&self, reservations: &[(InstanceId, u16)]) -> Vec<PortConflict> {
        let mut result = Vec::new();
        for (instance, port) in reservations {
            let service = format!("lantern-inst-{}.service", instance.safe_name());
            let scope = format!("lantern-inst-{}.scope", instance.safe_name());
            if let Some(listener) = self.listeners.iter().chain(&self.hidden).find(|l| {
                l.port == *port
                    && !l.unit.as_ref().is_some_and(|u| {
                        let unit = normalize_unit(u);
                        unit == service || unit == scope
                    })
            }) {
                result.push(PortConflict {
                    instance: instance.clone(),
                    holder: listener.into(),
                });
            }
        }
        result
    }
}

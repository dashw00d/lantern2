//! Fresh readiness and conflict answers using observed listeners.
use super::procinfo::normalize_unit;
use super::scanner::Snapshot;
use lantern_core::ids::InstanceId;
use lantern_core::model::Listener;
use std::net::SocketAddr;
use std::path::PathBuf;

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
/// Public, redacted explanation of a port holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortHolder {
    /// TCP port.
    pub port: u16,
    /// Primary local bind address.
    pub addr: SocketAddr,
    /// Process id, if visible.
    pub pid: Option<u32>,
    /// Socket owner uid.
    pub uid: u32,
    /// Executable path, if visible.
    pub exe: Option<PathBuf>,
    /// Redacted arguments.
    pub cmdline: Vec<String>,
    /// Working directory, if visible.
    pub cwd: Option<PathBuf>,
    /// systemd unit, if known.
    pub unit: Option<String>,
    /// Matched instance, if known.
    pub instance: Option<InstanceId>,
    /// Human-readable process label.
    pub label: String,
}
impl From<&Listener> for PortHolder {
    fn from(l: &Listener) -> Self {
        Self {
            port: l.port,
            addr: l.addr,
            pid: l.pid,
            uid: l.uid,
            exe: l.exe.clone(),
            cmdline: l.cmdline.clone(),
            cwd: l.cwd.clone(),
            unit: l.unit.clone(),
            instance: l.matched.clone(),
            label: l.label.clone(),
        }
    }
}
impl PortHolder {
    /// Describe the holder for a conflict message.
    pub fn describe(&self) -> String {
        let Some(pid) = self.pid else {
            return format!(
                "port {} is held by a process owned by uid {} (not visible to you)",
                self.port, self.uid
            );
        };
        let name = self
            .exe
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or(&self.label);
        let mut text = format!("port {} is held by `{}` (pid {})", self.port, name, pid);
        if let Some(cwd) = &self.cwd {
            let pretty = std::env::var_os("HOME")
                .map(PathBuf::from)
                .and_then(|home| cwd.strip_prefix(home).ok().map(|p| PathBuf::from("~").join(p)))
                .unwrap_or_else(|| cwd.clone());
            text.push_str(&format!(" in {}", pretty.display()));
        }
        if let Some(instance) = &self.instance {
            text.push_str(&format!(" (instance {instance})"));
        }
        if let Some(unit) = &self.unit {
            text.push_str(&format!(" (unit {unit})"));
        }
        text
    }
}
/// A reservation held by another process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortConflict {
    /// Instance whose reserved port is held.
    pub instance: InstanceId,
    /// Observed holder.
    pub holder: PortHolder,
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

//! Configurable listener visibility rules.
use super::procinfo::ProcInfo;
use lantern_core::model::{HiddenReason, HideOrigin, Listener};
use std::net::IpAddr;
use std::path::PathBuf;

/// A single port or inclusive port range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSpec {
    /// Match one port.
    Single(u16),
    /// Match an inclusive range.
    Range(u16, u16),
}
impl PortSpec {
    fn contains(self, port: u16) -> bool {
        match self {
            Self::Single(p) => port == p,
            Self::Range(a, b) => (a..=b).contains(&port),
        }
    }
}
/// A user hide rule; populated fields are ANDed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HideRule {
    /// Optional port or range.
    pub port: Option<PortSpec>,
    /// Optional exact address.
    pub addr: Option<IpAddr>,
    /// Optional owner uid.
    pub uid: Option<u32>,
    /// Optional glob on executable basename.
    pub exe: Option<String>,
    /// Optional glob on the raw joined command line, evaluated only internally.
    pub cmdline: Option<String>,
    /// Optional path prefix; `~` expands to the supplied home.
    pub cwd: Option<PathBuf>,
    /// Optional user-facing reason.
    pub reason: Option<String>,
}
/// Visibility configuration supplied by settings and system services.
#[derive(Debug, Clone)]
pub struct HideSettings {
    /// Whether compiled-in defaults apply.
    pub hide_defaults: bool,
    /// Whether non-loopback listeners are shown.
    pub include_non_loopback: bool,
    /// Additional user rules.
    pub rules: Vec<HideRule>,
    /// Listening ports used by Lantern infrastructure.
    pub self_ports: Vec<u16>,
    /// Hide arguments entirely when true.
    pub cmdline_hidden: bool,
}
impl Default for HideSettings {
    fn default() -> Self {
        Self {
            hide_defaults: true,
            include_non_loopback: true,
            rules: Vec::new(),
            self_ports: vec![4777, 2019, 80, 443, 5354],
            cmdline_hidden: false,
        }
    }
}
fn glob(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = value.chars().collect();
    let mut current = vec![false; s.len() + 1];
    current[0] = true;
    for c in p {
        let mut next = vec![false; s.len() + 1];
        if c == '*' {
            next[0] = current[0];
            for i in 1..=s.len() {
                next[i] = current[i] || next[i - 1];
            }
        } else {
            for i in 1..=s.len() {
                next[i] = current[i - 1] && (c == '?' || c == s[i - 1]);
            }
        }
        current = next;
    }
    current[s.len()]
}
fn reason(origin: HideOrigin, why: impl Into<String>) -> HiddenReason {
    HiddenReason {
        origin,
        why: why.into(),
    }
}
pub(super) fn hidden(
    listener: &Listener,
    proc: Option<&ProcInfo>,
    settings: &HideSettings,
    home: &std::path::Path,
    daemon_pid: u32,
) -> Option<HiddenReason> {
    if listener.managed {
        return None;
    }
    let ip = listener.addr.ip();
    if !settings.include_non_loopback && !ip.is_loopback() {
        return Some(reason(HideOrigin::User, "non_loopback"));
    }
    if settings.hide_defaults {
        if listener.port == 53
            && ["127.0.0.53", "127.0.0.54"]
                .iter()
                .any(|s| s.parse::<IpAddr>().ok() == Some(ip))
        {
            return Some(reason(HideOrigin::Default, "systemd-resolved stub"));
        }
        if listener.uid == 0 && [22, 631].contains(&listener.port) {
            return Some(reason(
                HideOrigin::Default,
                if listener.port == 22 { "sshd" } else { "CUPS" },
            ));
        }
        if listener.pid == Some(daemon_pid)
            || matches!(
                listener.unit.as_deref(),
                Some("lantern.service" | "lantern-caddy.service")
            )
            || settings.self_ports.contains(&listener.port)
        {
            return Some(reason(HideOrigin::Lantern, "Lantern infrastructure"));
        }
    }
    for rule in &settings.rules {
        if rule.port.is_none()
            && rule.addr.is_none()
            && rule.uid.is_none()
            && rule.exe.is_none()
            && rule.cmdline.is_none()
            && rule.cwd.is_none()
        {
            continue;
        }
        if rule.port.is_some_and(|p| !p.contains(listener.port))
            || rule.addr.is_some_and(|a| a != ip)
            || rule.uid.is_some_and(|u| u != listener.uid)
        {
            continue;
        }
        if let Some(pattern) = &rule.exe
            && !listener
                .exe
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .is_some_and(|s| glob(pattern, s))
        {
            continue;
        }
        if let Some(pattern) = &rule.cmdline
            && !proc.is_some_and(|p| glob(pattern, &p.cmdline_raw.join(" ")))
        {
            continue;
        }
        if let Some(path) = &rule.cwd {
            let expanded = path
                .strip_prefix("~")
                .map_or_else(|_| path.clone(), |p| home.join(p));
            if !listener.cwd.as_ref().is_some_and(|cwd| cwd.starts_with(expanded)) {
                continue;
            }
        }
        return Some(reason(
            HideOrigin::User,
            rule.reason.clone().unwrap_or_else(|| "user rule".into()),
        ));
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_hide_globs() {
        assert!(glob("steam*?", "steamwebhelper"));
        assert!(!glob("steam*", "node"));
        assert!(PortSpec::Range(20, 30).contains(25));
    }
}

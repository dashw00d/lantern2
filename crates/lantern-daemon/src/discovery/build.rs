//! Pure conversion of socket observations to stable listener rows.
use super::classify::classify;
use super::hide::{HideSettings, hidden};
use super::matcher::{CheckoutIndex, match_owner};
use super::procinfo::{ProcInfo, is_lantern_inst};
use super::redact::redact;
use super::source::RawSocket;
use lantern_core::model::{Exposure, Listener};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub(super) type Observation = (RawSocket, Option<ProcInfo>);
fn rank(addr: &SocketAddr) -> (u8, SocketAddr) {
    let ip = addr.ip();
    let rank = match ip {
        IpAddr::V4(ip) if ip == Ipv4Addr::LOCALHOST => 0,
        IpAddr::V6(ip) if ip == Ipv6Addr::LOCALHOST => 1,
        ip if ip.is_loopback() => 2,
        IpAddr::V4(ip) if ip.is_unspecified() => 3,
        IpAddr::V6(ip) if ip.is_unspecified() => 4,
        _ => 5,
    };
    (rank, *addr)
}
fn exposure(addrs: &[SocketAddr]) -> Exposure {
    if addrs.iter().any(|a| a.ip().is_unspecified()) {
        Exposure::AllInterfaces
    } else if addrs.iter().any(|a| !a.ip().is_loopback()) {
        Exposure::Interface
    } else {
        Exposure::Loopback
    }
}
pub(super) fn build(
    observations: Vec<Observation>,
    index: &CheckoutIndex,
    settings: &HideSettings,
    me: u32,
    daemon_pid: u32,
) -> (Vec<Listener>, Vec<Listener>) {
    let mut groups: BTreeMap<(u16, Option<u32>, u32), Vec<Observation>> = BTreeMap::new();
    for obs in observations {
        let pid = obs.1.as_ref().map(|p| p.key.pid);
        // Known pids are the identity; uid is the identity only for unresolved owners.
        let key = (obs.0.addr.port(), pid, if pid.is_some() { 0 } else { obs.0.uid });
        groups.entry(key).or_default().push(obs);
    }
    let mut visible = Vec::new();
    let mut hidden_rows = Vec::new();
    for (_, mut group) in groups {
        group.sort_by_key(|(socket, _)| rank(&socket.addr));
        let (first, proc) = &group[0];
        let mut addrs: Vec<_> = group.iter().map(|(s, _)| s.addr).collect();
        addrs.sort_by_key(rank);
        addrs.dedup();
        let matched = proc.as_ref().and_then(|p| match_owner(p, index, me, daemon_pid));
        let (class, label) = classify(proc.as_ref(), first.uid, me, index, matched.is_some(), daemon_pid);
        let unit = proc.as_ref().and_then(|p| p.unit.clone());
        let managed = unit.as_deref().is_some_and(is_lantern_inst);
        let mut listener = Listener {
            addr: addrs[0],
            port: first.addr.port(),
            other_addrs: addrs[1..].to_vec(),
            uid: first.uid,
            pid: proc.as_ref().map(|p| p.key.pid),
            exe: proc.as_ref().and_then(|p| p.exe.clone()),
            cmdline: if settings.cmdline_hidden {
                Vec::new()
            } else {
                proc.as_ref().map_or_else(Vec::new, |p| redact(&p.cmdline_raw))
            },
            cwd: proc.as_ref().and_then(|p| p.cwd.clone()),
            matched: matched.as_ref().map(|(c, _)| c.instance.clone()),
            managed,
            exposure: exposure(&addrs),
            started_at: proc.as_ref().and_then(|p| p.started_at),
            project: matched.as_ref().map(|(c, _)| c.project.clone()),
            branch: matched.as_ref().and_then(|(c, _)| c.branch.clone()),
            matched_by: matched.map(|(_, src)| src),
            unit,
            class,
            label,
            probe: None,
            docker: None,
            adopted: false,
            hidden: None,
        };
        listener.hidden = hidden(&listener, proc.as_ref(), settings, &index.home, daemon_pid);
        if listener.hidden.is_some() {
            hidden_rows.push(listener);
        } else {
            visible.push(listener);
        }
    }
    let sort = |v: &mut Vec<Listener>| v.sort_by_key(|l| (l.port, l.addr, l.pid));
    sort(&mut visible);
    sort(&mut hidden_rows);
    (visible, hidden_rows)
}

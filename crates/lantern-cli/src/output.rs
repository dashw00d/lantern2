//! Human output from the same core wire values used for JSON.
use lantern_core::{api::{InstanceView, ProjectSummary}, model::{InstanceStatus, Listener, LogLine, Stream}};

fn status(status: InstanceStatus) -> (&'static str, &'static str, &'static str) {
    match status {
        InstanceStatus::Running => ("●", "running", "32"),
        InstanceStatus::Preparing => ("◐", "preparing", "33"),
        InstanceStatus::Starting => ("◐", "starting", "33"),
        InstanceStatus::Stopping => ("◐", "stopping", "33"),
        InstanceStatus::Unhealthy => ("▲", "unhealthy", "33"),
        InstanceStatus::Crashed => ("✖", "crashed", "31"),
        InstanceStatus::Stopped => ("○", "stopped", "0"),
        InstanceStatus::External => ("◇", "external", "36"),
        InstanceStatus::NeedsConfig => ("?", "needs-config", "33"),
    }
}
/// Render an instance table with optional unmanaged listeners.
pub(crate) fn instances(items: &[InstanceView], listeners: &[Listener], color: bool) -> String {
    let mut result = String::from("PROJECT      INSTANCE                 BRANCH              STATUS          PORT   URL\n");
    let mut previous = "";
    for i in items {
        let (glyph, name, ansi) = status(i.runtime.status);
        let mark = if color { format!("\x1b[{ansi}m{glyph}\x1b[0m") } else { glyph.to_string() };
        let project = if previous == i.project.as_str() { "" } else { i.project.as_str() };
        let branch = i.checkout.branch.as_deref().unwrap_or("-");
        let port = i.runtime.port.map_or_else(|| "-".to_string(), |p| p.to_string());
        result.push_str(&format!("{project:<12} {:<24} {branch:<19} {mark} {name:<13} {port:<6} {}\n", i.id, i.url));
        if let Some(message) = &i.runtime.message { result.push_str(&format!("               {}\n", clean(message, false))); }
        previous = i.project.as_str();
    }
    if !listeners.is_empty() {
        result.push_str("\nUNMANAGED LISTENERS\nPORT   LABEL                PID      CWD                     MATCH\n");
        for l in listeners {
            let pid = l.pid.map_or_else(|| "-".to_string(), |n| n.to_string());
            let cwd = l.cwd.as_ref().map_or_else(|| "-".to_string(), |p| p.display().to_string());
            let matched = l.matched.as_ref().map_or("-", |id| id.as_str());
            result.push_str(&format!("{:<6} {:<20} {pid:<8} {cwd:<23} {matched}\n", l.port, clean(&l.label, false)));
        }
    }
    result
}
/// Render a project table, useful for scan results.
pub(crate) fn projects(projects: &[ProjectSummary]) -> String {
    let mut result = String::from("PROJECT      STATUS          PORT   URL\n");
    for p in projects {
        let (glyph, name, _) = status(p.status);
        let port = p.port.map_or_else(|| "-".to_string(), |n| n.to_string());
        result.push_str(&format!("{:<12} {glyph} {name:<13} {port:<6} {}\n", p.project.id, p.url));
    }
    result
}
/// Strip terminal control sequences, optionally retaining SGR colors on a TTY.
pub(crate) fn clean(input: &str, keep_sgr: bool) -> String {
    let mut out = String::new();
    let mut it = input.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.next() {
                Some('[') => {
                    let mut seq = String::new();
                    for ch in it.by_ref() {
                        seq.push(ch);
                        if ('@'..='~').contains(&ch) { break; }
                    }
                    if keep_sgr && seq.ends_with('m') && seq.chars().all(|x| x.is_ascii_digit() || matches!(x, ';' | ':' | 'm')) {
                        out.push_str("\x1b["); out.push_str(&seq);
                    }
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    let mut esc = false;
                    for ch in it.by_ref() { if ch == '\x07' || (esc && ch == '\\') { break; } esc = ch == '\x1b'; }
                }
                _ => {}
            }
        } else if !c.is_control() || c == '\t' { out.push(c); }
    }
    out
}
/// Format one safely escaped log line.
pub(crate) fn log(line: &LogLine, timestamps: bool, tty: bool) -> String {
    let timestamp = line.ts.to_string();
    let time = if timestamps { timestamp.as_str() } else { timestamp.split('T').nth(1).unwrap_or(&timestamp).get(..8).unwrap_or(&timestamp) };
    let stream = match line.stream { Stream::Stdout => "out", Stream::Stderr => "err", Stream::System => "sys", Stream::Setup => "setup" };
    format!("{time} {stream} {}\n", clean(&line.line, tty))
}

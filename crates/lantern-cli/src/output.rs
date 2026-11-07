//! Human output from the same core wire values used for JSON.
use lantern_core::{
    api::{BranchInfo, InstanceView, ProjectSummary, ServiceStatus},
    model::{InstanceStatus, Listener, LogLine, Stream},
};

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
    let mut result = String::from(
        "PROJECT      INSTANCE                 BRANCH              STATUS          PORT   URL\n",
    );
    let mut previous = "";
    for i in items {
        let (glyph, name, ansi) = status(i.runtime.status);
        let mark = if color {
            format!("\x1b[{ansi}m{glyph}\x1b[0m")
        } else {
            glyph.to_string()
        };
        let project = if previous == i.project.as_str() {
            ""
        } else {
            i.project.as_str()
        };
        let branch = clean(i.checkout.branch.as_deref().unwrap_or("-"), false);
        let port = i.runtime.port.map_or_else(|| "-".to_string(), |p| p.to_string());
        result.push_str(&format!(
            "{project:<12} {:<24} {branch:<19} {mark} {name:<13} {port:<6} {}\n",
            i.id.as_str(),
            clean(&i.url, false)
        ));
        if let Some(message) = &i.runtime.message {
            result.push_str(&format!("               {}\n", clean(message, false)));
        }
        previous = i.project.as_str();
    }
    if !listeners.is_empty() {
        result.push_str(
            "\nUNMANAGED LISTENERS\nPORT   LABEL                PID      CWD                     MATCH\n",
        );
        for l in listeners {
            let pid = l.pid.map_or_else(|| "-".to_string(), |n| n.to_string());
            let cwd = l
                .cwd
                .as_ref()
                .map_or_else(|| "-".to_string(), |p| clean(&p.display().to_string(), false));
            let matched = l.matched.as_ref().map_or("-", |id| id.as_str());
            result.push_str(&format!(
                "{:<6} {:<20} {pid:<8} {cwd:<23} {matched}\n",
                l.port,
                clean(&l.label, false)
            ));
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
        result.push_str(&format!(
            "{:<12} {glyph} {name:<13} {port:<6} {}\n",
            p.project.id.as_str(),
            clean(&p.url, false)
        ));
    }
    result
}
/// Render repository branches and their upstream state.
pub(crate) fn branches(branches: &[BranchInfo]) -> String {
    let mut result =
        String::from("BRANCH                       UPSTREAM                AHEAD  BEHIND  CHECKOUT\n");
    for branch in branches {
        let name = clean(&branch.name, false);
        let upstream = clean(branch.upstream.as_deref().unwrap_or("-"), false);
        let path = branch.checked_out_at.as_ref().map_or_else(
            || "-".to_string(),
            |path| clean(&path.display().to_string(), false),
        );
        result.push_str(&format!(
            "{name:<28} {upstream:<23} {:<6} {:<7} {path}\n",
            branch.ahead, branch.behind
        ));
    }
    result
}
/// Render shared-service states and their UI addresses.
pub(crate) fn services(services: &[ServiceStatus]) -> String {
    let mut result = String::from("SERVICE        STATUS          MODE      URL\n");
    for service in services {
        let name = serde_json::to_value(service.name)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let state = serde_json::to_value(service.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let mode = serde_json::to_value(service.mode)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let url = clean(service.ui_url.as_deref().unwrap_or("-"), false);
        result.push_str(&format!("{name:<14} {state:<15} {mode:<9} {url}\n"));
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
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                    if keep_sgr
                        && seq.ends_with('m')
                        && seq
                            .chars()
                            .all(|x| x.is_ascii_digit() || matches!(x, ';' | ':' | 'm'))
                    {
                        out.push_str("\x1b[");
                        out.push_str(&seq);
                    }
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    let mut esc = false;
                    for ch in it.by_ref() {
                        if ch == '\x07' || (esc && ch == '\\') {
                            break;
                        }
                        esc = ch == '\x1b';
                    }
                }
                _ => {}
            }
        } else if !c.is_control() || c == '\t' {
            out.push(c);
        }
    }
    out
}
/// Format one safely escaped log line.
pub(crate) fn log(line: &LogLine, timestamps: bool, tty: bool) -> String {
    let timestamp = line.ts.to_string();
    let time = if timestamps {
        timestamp.as_str()
    } else {
        timestamp
            .split('T')
            .nth(1)
            .unwrap_or(&timestamp)
            .get(..8)
            .unwrap_or(&timestamp)
    };
    let stream = match line.stream {
        Stream::Stdout => "out",
        Stream::Stderr => "err",
        Stream::System => "sys",
        Stream::Setup => "setup",
    };
    format!("{time} {stream} {}\n", clean(&line.line, tty))
}

#[cfg(test)]
mod tests {
    use super::clean;
    #[test]
    fn strips_osc_and_cursor_controls_but_keeps_tty_colors() {
        let input = "a\x1b[31mb\x1b[0m\x1b]0;title\x07\x1b[2Jc";
        assert_eq!(clean(input, false), "abc");
        assert_eq!(clean(input, true), "a\x1b[31mb\x1b[0mc");
    }
}

/// Render doctor checks grouped by their ID's area.
pub(crate) fn doctor(checks: &[serde_json::Value]) -> String {
    let mut groups: Vec<(String, Vec<&serde_json::Value>)> = Vec::new();
    for check in checks {
        let id = check
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("other");
        let area = id.split(['.', '_']).next().unwrap_or("other").to_owned();
        if let Some((_, group)) = groups.iter_mut().find(|(name, _)| *name == area) {
            group.push(check);
        } else {
            groups.push((area, vec![check]));
        }
    }
    let mut text = format!("Lantern doctor — {} checks\n", checks.len());
    for (area, group) in groups {
        text.push_str(&format!("\n{}\n", clean(&area.to_uppercase(), false)));
        for check in group {
            let status = check
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let glyph = match status {
                "ok" => "✓",
                "warn" => "⚠",
                "fail" => "✗",
                "skipped" => "–",
                _ => "?",
            };
            let title = check
                .get("title")
                .and_then(serde_json::Value::as_str)
                .or_else(|| check.get("id").and_then(serde_json::Value::as_str))
                .unwrap_or("Check");
            let detail = check
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            text.push_str(&format!(
                "  {glyph} {}: {}\n",
                clean(title, false),
                clean(detail, false)
            ));
            if matches!(status, "warn" | "fail") {
                if let Some(fix) = check.get("fix").and_then(serde_json::Value::as_object) {
                    let description = fix
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let command = fix
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    if !description.is_empty() || !command.is_empty() {
                        text.push_str(&format!("    fix: {}", clean(description, false)));
                        if !command.is_empty() {
                            text.push_str(&format!(" — {}", clean(command, false)));
                        }
                        text.push('\n');
                    }
                }
            }
        }
    }
    text
}

/// Render initialization or uninstall step results and any elevation plan.
pub(crate) fn system_steps(value: &serde_json::Value) -> String {
    let mut text = String::new();
    for (key, label) in [("user_steps", "User steps"), ("system_steps", "System steps")] {
        if let Some(steps) = value.get(key).and_then(serde_json::Value::as_array) {
            text.push_str(&format!("{label}:\n"));
            for step in steps {
                let id = step
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("step");
                let state = step
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let detail = step
                    .get("detail")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                text.push_str(&format!("  {}: {}", clean(id, false), clean(state, false)));
                if !detail.is_empty() {
                    text.push_str(&format!(" — {}", clean(detail, false)));
                }
                text.push('\n');
            }
        }
    }
    if let Some(command) = value.get("pkexec").and_then(serde_json::Value::as_object) {
        let program = command
            .get("program")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("pkexec");
        let args = command
            .get("args")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(shlex::try_quote)
            .collect::<Result<Vec<_>, _>>();
        if let Ok(args) = args {
            text.push_str(&format!(
                "Root steps pending: {} {}\n",
                clean(program, false),
                args.join(" ")
            ));
        }
    }
    if text.is_empty() {
        text.push_str("No system steps reported.\n");
    }
    text
}

/// Render the v1 import plan without exposing JSON in the human CLI.
pub(crate) fn migration(value: &serde_json::Value, dry_run: bool) -> String {
    let mut text = if dry_run {
        "v1 migration plan:\n"
    } else {
        "v1 migration results:\n"
    }
    .to_string();
    if let Some(actions) = value.get("actions").and_then(serde_json::Value::as_array) {
        if actions.is_empty() {
            text.push_str("  No changes.\n");
        }
        for action in actions {
            let kind = action
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let what = action
                .get("what")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("item");
            text.push_str(&format!("  {}: {}", clean(kind, false), clean(what, false)));
            if let Some(reason) = action.get("reason").and_then(serde_json::Value::as_str) {
                text.push_str(&format!(" — {}", clean(reason, false)));
            }
            text.push('\n');
        }
    }
    if let Some(warnings) = value.get("warnings").and_then(serde_json::Value::as_array) {
        for warning in warnings.iter().filter_map(serde_json::Value::as_str) {
            text.push_str(&format!("  ⚠ {}\n", clean(warning, false)));
        }
    }
    text
}

#[cfg(test)]
mod system_output_tests {
    use super::migration;
    use serde_json::json;

    #[test]
    fn migration_plan_is_human_readable() {
        let text = migration(
            &json!({"actions":[{"kind":"skip","what":"old-app","reason":"already imported"}],"warnings":["missing file"]}),
            true,
        );
        assert_eq!(
            text,
            "v1 migration plan:\n  skip: old-app — already imported\n  ⚠ missing file\n"
        );
    }
}

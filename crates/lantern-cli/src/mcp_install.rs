//! MCP configuration rendering and atomic client-file updates.

use crate::args::{McpScope, McpTarget};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use toml_edit::{DocumentMut, Item, Table};

/// Format a portable or client-specific configuration without touching files.
pub fn snippet(target: McpTarget, url: &str) -> String {
    match target {
        McpTarget::Codex => format!(
            "[mcp_servers.lantern]\nurl = {url:?}\nstartup_timeout_sec = 10\ntool_timeout_sec = 120\n"
        ),
        McpTarget::ClaudeCode => {
            serde_json::to_string_pretty(&json!({"mcpServers":{"lantern":{"type":"http","url":url}}}))
                .unwrap()
                + "\n"
        }
        McpTarget::Cursor | McpTarget::Json => {
            serde_json::to_string_pretty(&json!({"mcpServers":{"lantern":{"url":url}}})).unwrap() + "\n"
        }
    }
}

/// Resolve a client config path using injected home and working directory.
pub fn config_path(target: McpTarget, scope: McpScope, home: &Path, cwd: &Path) -> Option<PathBuf> {
    let root = match scope {
        McpScope::User => home,
        McpScope::Project => cwd,
    };
    match target {
        McpTarget::Codex => Some(root.join(".codex/config.toml")),
        McpTarget::Cursor => Some(root.join(".cursor/mcp.json")),
        McpTarget::ClaudeCode if matches!(scope, McpScope::Project) => Some(root.join(".mcp.json")),
        _ => None,
    }
}
fn merge_json(source: &str, target: McpTarget, url: &str, force: bool) -> Result<String, String> {
    let mut doc: Value = if source.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(source).map_err(|e| format!("invalid JSON: {e}; use --print"))?
    };
    let root = doc
        .as_object_mut()
        .ok_or("JSON root must be an object; use --print")?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("mcpServers must be an object; use --print")?;
    if let Some(old) = servers.get("lantern") {
        let old_url = old.get("url").and_then(Value::as_str);
        if old_url != Some(url) && !force {
            return Err("lantern is already configured with a different URL; use --force".into());
        }
        if !old.is_object() {
            return Err("existing lantern entry must be an object; use --print".into());
        }
    }
    let entry = servers
        .entry("lantern")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .unwrap();
    entry.insert("url".into(), Value::String(url.into()));
    if matches!(target, McpTarget::ClaudeCode) {
        entry.insert("type".into(), Value::String("http".into()));
    }
    Ok(serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())? + "\n")
}
fn set_value(table: &mut Table, key: &str, mut next: toml_edit::Value) {
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        if old.as_str().zip(next.as_str()).is_some_and(|(a, b)| a == b)
            || old
                .as_integer()
                .zip(next.as_integer())
                .is_some_and(|(a, b)| a == b)
        {
            return;
        }
        *next.decor_mut() = old.decor().clone();
    }
    table.insert(key, Item::Value(next));
}
fn merge_toml(source: &str, url: &str, force: bool) -> Result<String, String> {
    let mut doc = source
        .parse::<DocumentMut>()
        .map_err(|e| format!("invalid TOML: {e}; use --print"))?;
    if !doc.as_table().contains_key("mcp_servers") {
        doc["mcp_servers"] = Item::Table(Table::new());
    }
    let servers = doc["mcp_servers"]
        .as_table_mut()
        .ok_or("mcp_servers must be a table; use --print")?;
    if let Some(old) = servers.get("lantern") {
        let table = old.as_table().ok_or("lantern must be a table; use --print")?;
        if table.get("url").and_then(Item::as_str) != Some(url) && !force {
            return Err("lantern is already configured with a different URL; use --force".into());
        }
    } else {
        servers["lantern"] = Item::Table(Table::new());
    }
    let entry = servers["lantern"].as_table_mut().unwrap();
    set_value(entry, "url", toml_edit::Value::from(url));
    set_value(entry, "startup_timeout_sec", toml_edit::Value::from(10));
    set_value(entry, "tool_timeout_sec", toml_edit::Value::from(120));
    Ok(doc.to_string())
}
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lantern-bak");
    PathBuf::from(name)
}
fn atomic_update(path: &Path, bytes: &[u8], old: Option<&[u8]>, mode: u32) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or("config path has no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    if let Some(old) = old {
        let backup = backup_path(path);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&backup)
        {
            Ok(mut file) => {
                file.write_all(old)
                    .and_then(|()| file.sync_all())
                    .map_err(|e| format!("{}: {e}", backup.display()))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("{}: {e}", backup.display())),
        }
    }
    let temp = parent.join(format!(
        ".lantern-mcp-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.map_err(|e| format!("{}: {e}", path.display()))
}
/// Merge an injected Codex, Cursor or Claude project file, keeping other entries and a backup.
/// Returns true if the file changed.
pub fn install_file(path: &Path, target: McpTarget, url: &str, force: bool) -> Result<bool, String> {
    if matches!(target, McpTarget::Json) {
        return Err("JSON is a print-only target".into());
    }
    let (old, mode) = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(format!("{} is not a regular file", path.display()));
        }
        Ok(meta) => {
            let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
            let mut old = String::new();
            file.read_to_string(&mut old)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            (Some(old), meta.permissions().mode() & 0o777)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, 0o600),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let source = old.as_deref().unwrap_or("");
    let next = match target {
        McpTarget::Codex => merge_toml(source, url, force),
        _ => merge_json(source, target, url, force),
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;
    // Compare semantics for JSON, so an already-correct entry does not reformat a file.
    let same = if matches!(target, McpTarget::Codex) {
        source == next
    } else {
        serde_json::from_str::<Value>(source).ok() == serde_json::from_str::<Value>(&next).ok()
    };
    if same {
        return Ok(false);
    }
    atomic_update(path, next.as_bytes(), old.as_deref().map(str::as_bytes), mode)?;
    Ok(true)
}

/// Invoke Claude Code's own installer, falling back only to a project-scoped file.
pub fn install_claude(
    scope: McpScope,
    url: &str,
    force: bool,
    cwd: &Path,
    project_path: Option<&Path>,
) -> Result<String, String> {
    install_claude_with_program(Path::new("claude"), scope, url, force, cwd, project_path)
}

/// Invoke an injectable Claude executable; project fallback is limited to the supplied path.
pub fn install_claude_with_program(
    program: &Path,
    scope: McpScope,
    url: &str,
    force: bool,
    cwd: &Path,
    project_path: Option<&Path>,
) -> Result<String, String> {
    use std::process::Command;
    let scope_name = match scope {
        McpScope::User => "user",
        McpScope::Project => "project",
    };
    let get = Command::new(program)
        .current_dir(cwd)
        .args(["mcp", "get", "lantern"])
        .output();
    let existing = match get {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let hint = format!("claude mcp add --transport http --scope {scope_name} lantern {url}");
            if matches!(scope, McpScope::Project) {
                let default = cwd.join(".mcp.json");
                let path = project_path.unwrap_or(&default);
                let changed = install_file(path, McpTarget::ClaudeCode, url, force)?;
                return Ok(format!(
                    "{} {} (Claude CLI unavailable; command: {hint})",
                    if changed { "Updated" } else { "Already installed:" },
                    path.display()
                ));
            }
            return Err(format!(
                "Claude CLI not found. Run `{hint}` after installing it, or use --print."
            ));
        }
        Err(e) => return Err(format!("cannot run claude: {e}")),
    };
    if existing.status.success() {
        let text = String::from_utf8_lossy(&existing.stdout);
        if text
            .split_whitespace()
            .any(|word| word.trim_matches(|c: char| "\"',;".contains(c)) == url)
        {
            return Ok("Claude Code already configured".into());
        }
        if !force {
            return Err("Claude Code already has a different lantern configuration; use --force".into());
        }
        let removed = Command::new(program)
            .current_dir(cwd)
            .args(["mcp", "remove", "lantern", "-s", scope_name])
            .output()
            .map_err(|e| format!("claude remove: {e}"))?;
        if !removed.status.success() {
            return Err(format!(
                "claude remove failed: {}",
                String::from_utf8_lossy(&removed.stderr).trim()
            ));
        }
    }
    let added = Command::new(program)
        .current_dir(cwd)
        .args([
            "mcp",
            "add",
            "--transport",
            "http",
            "--scope",
            scope_name,
            "lantern",
            url,
        ])
        .output()
        .map_err(|e| format!("claude add: {e}"))?;
    if !added.status.success() {
        return Err(format!(
            "claude add failed: {}",
            String::from_utf8_lossy(&added.stderr).trim()
        ));
    }
    Ok("Configured Claude Code".into())
}

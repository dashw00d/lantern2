//! Capture and cache the login shell's environment without importing session state.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use thiserror::Error;
use tokio::process::Command;

use super::launcher::sh_quote;

const BEGIN: &[u8] = b"<<LANTERN_ENV_BEGIN>>";
const END: &[u8] = b"<<LANTERN_ENV_END>>";
/// Shell command passed to the user's interactive login shell.
pub const CAPTURE_COMMAND: &str = "printf '<<LANTERN_ENV_BEGIN>>'; env -0; printf '<<LANTERN_ENV_END>>'";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Failure while capturing or parsing a shell environment.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// The shell, session wrapper or process could not be run.
    #[error("shell environment capture failed: {0}")]
    Io(#[from] io::Error),
    /// The shell did not finish by the deadline; its process group was killed.
    #[error("shell environment capture timed out")]
    Timeout,
    /// The shell exited unsuccessfully.
    #[error("shell environment capture exited with {status}: {stderr}")]
    Exit { status: String, stderr: String },
    /// Capture markers or NUL-separated entries were missing or malformed.
    #[error("invalid shell environment capture: {0}")]
    Invalid(&'static str),
}

/// Successfully captured environment and shell diagnostics.
#[derive(Debug, Clone)]
pub struct CapturedEnv {
    /// Variables safe to cache, before PATH merging.
    pub vars: BTreeMap<String, String>,
    /// The shell's stderr for diagnostic use.
    pub stderr: String,
}

/// Capture a login shell in a new session and kill its entire group on timeout.
///
/// Requires the standard util-linux `setsid` and `/bin/kill` utilities. The
/// caller chooses the passwd or configured shell and applies fallback on error.
pub async fn capture(shell: &Path, timeout: Duration) -> Result<CapturedEnv, CaptureError> {
    let mut command = Command::new("setsid");
    command
        .arg(shell)
        .args(["-l", "-i", "-c", CAPTURE_COMMAND])
        .env("LANTERN_RESOLVING_ENV", "1")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("capture process has no pid"))?;
    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let output = match tokio::time::timeout(timeout, &mut wait).await {
        Ok(result) => result?,
        Err(_) => {
            // setsid execs the shell as the session/group leader. Signal the group,
            // not just the shell: interactive dotfiles can leave children behind.
            let group = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            // If the deadline expired before setsid established the session,
            // also kill the wrapper itself and retry the group once.
            if !group.is_ok_and(|status| status.success()) {
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", "--", &pid.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            return Err(CaptureError::Timeout);
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(CaptureError::Exit {
            status: output.status.to_string(),
            stderr,
        });
    }
    Ok(CapturedEnv {
        vars: parse_capture(&output.stdout)?,
        stderr,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn valid_name(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a shell variable must stay under the live user manager's control.
pub fn denied(key: &str) -> bool {
    matches!(
        key,
        "DISPLAY"
            | "WAYLAND_DISPLAY"
            | "XAUTHORITY"
            | "XDG_RUNTIME_DIR"
            | "XDG_SEAT"
            | "XDG_VTNR"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "SESSION_MANAGER"
            | "SSH_CONNECTION"
            | "SSH_CLIENT"
            | "SSH_TTY"
            | "_"
            | "SHLVL"
            | "PWD"
            | "OLDPWD"
            | "TERM"
            | "COLORTERM"
            | "LS_COLORS"
            | "LANTERN_RESOLVING_ENV"
    ) || key.starts_with("XDG_SESSION_")
        || key.starts_with("TERM_PROGRAM")
}

/// Parse marked NUL-separated environment output, ignoring shell startup noise.
pub fn parse_capture(output: &[u8]) -> Result<BTreeMap<String, String>, CaptureError> {
    let start = find(output, BEGIN).ok_or(CaptureError::Invalid("missing begin marker"))? + BEGIN.len();
    let remaining = &output[start..];
    let end = remaining
        .windows(END.len())
        .rposition(|window| window == END)
        .ok_or(CaptureError::Invalid("missing end marker"))?;
    let data = &remaining[..end];
    if data.is_empty() || !data.ends_with(&[0]) {
        return Err(CaptureError::Invalid("empty or unterminated environment"));
    }
    let mut vars = BTreeMap::new();
    for entry in data[..data.len() - 1].split(|byte| *byte == 0) {
        let eq = entry
            .iter()
            .position(|b| *b == b'=')
            .ok_or(CaptureError::Invalid("entry without equals"))?;
        let key = std::str::from_utf8(&entry[..eq])
            .map_err(|_| CaptureError::Invalid("non-UTF-8 variable name"))?;
        if valid_name(key) && !denied(key) {
            vars.insert(
                key.to_owned(),
                String::from_utf8_lossy(&entry[eq + 1..]).into_owned(),
            );
        }
    }
    Ok(vars)
}

/// Merge PATH values in precedence order, retaining the first occurrence of each entry.
pub fn merge_path(shell: &str, manager: &str, safety_dirs: &[PathBuf]) -> String {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in shell.split(':').chain(manager.split(':')) {
        if seen.insert(part.to_owned()) {
            out.push(part.to_owned());
        }
    }
    for dir in safety_dirs {
        if dir.is_dir()
            && let Some(part) = dir.to_str()
            && seen.insert(part.to_owned())
        {
            out.push(part.to_owned());
        }
    }
    out.join(":")
}

/// Return the documented safety-net directories, in priority order.
pub fn safety_net_dirs(home: &Path) -> Vec<PathBuf> {
    [
        ".local/bin",
        ".cargo/bin",
        ".volta/bin",
        ".asdf/shims",
        ".pyenv/shims",
        ".local/share/mise/shims",
        ".local/share/pnpm",
        ".bun/bin",
        ".deno/bin",
    ]
    .into_iter()
    .map(|p| home.join(p))
    .chain(std::iter::once(PathBuf::from("/home/linuxbrew/.linuxbrew/bin")))
    .collect()
}

/// Build cacheable exports, merging captured and manager PATH plus existing safety directories.
pub fn merge_environment(
    captured: &BTreeMap<String, String>,
    manager: &BTreeMap<String, String>,
    home: &Path,
) -> BTreeMap<String, String> {
    let mut vars: BTreeMap<_, _> = captured
        .iter()
        .filter(|(key, _)| valid_name(key) && !denied(key))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // An absent PATH must not introduce an empty entry (the current directory).
    let shell_path = captured.get("PATH").map_or("", String::as_str);
    let manager_path = manager.get("PATH").map_or("", String::as_str);
    let path = match (shell_path.is_empty(), manager_path.is_empty()) {
        (true, false) => merge_path(manager_path, manager_path, &safety_net_dirs(home)),
        (false, true) => merge_path(shell_path, shell_path, &safety_net_dirs(home)),
        (true, true) => safety_net_dirs(home)
            .into_iter()
            .filter(|p| p.is_dir())
            .filter_map(|p| p.to_str().map(str::to_owned))
            .collect::<Vec<_>>()
            .join(":"),
        (false, false) => merge_path(shell_path, manager_path, &safety_net_dirs(home)),
    };
    vars.insert("PATH".into(), path);
    vars
}

/// Atomically replace an environment script with mode 0600 in its existing parent directory.
pub fn write_shell_env(path: &Path, vars: &BTreeMap<String, String>) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
    let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        seq
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        for (key, value) in vars {
            if !valid_name(key) || denied(key) || value.contains('\0') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid or denied environment entry",
                ));
            }
            writeln!(file, "export {key}={}", sh_quote(value))?;
        }
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// File metadata snapshot used to detect login-shell configuration changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMtimes(BTreeMap<PathBuf, Option<SystemTime>>);

/// Snapshot shell startup files and the environment.d directory and its entries.
pub fn source_mtimes(home: &Path) -> SourceMtimes {
    let mut paths: Vec<_> = [
        ".profile",
        ".bash_profile",
        ".bashrc",
        ".zshenv",
        ".zprofile",
        ".zshrc",
        ".config/fish/config.fish",
        ".config/environment.d",
    ]
    .into_iter()
    .map(|p| home.join(p))
    .collect();
    let dir = home.join(".config/environment.d");
    if let Ok(entries) = fs::read_dir(dir) {
        paths.extend(entries.flatten().map(|e| e.path()));
    }
    SourceMtimes(
        paths
            .into_iter()
            .map(|p| {
                let mtime = fs::metadata(&p).and_then(|m| m.modified()).ok();
                (p, mtime)
            })
            .collect(),
    )
}

/// Check whether any observed source was created, removed, or modified.
pub fn sources_changed(home: &Path, previous: &SourceMtimes) -> bool {
    source_mtimes(home) != *previous
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parsing_drops_session_noise_and_preserves_values() {
        let data = b"motd\n<<LANTERN_ENV_BEGIN>>PATH=/shell/bin\0DISPLAY=:9\0XDG_SESSION_ID=9\0TERM_PROGRAM_VERSION=1\0A=a=b\nline\0_=/bin/env\0<<LANTERN_ENV_END>>trailer";
        let vars = parse_capture(data).unwrap();
        assert_eq!(vars.get("A").unwrap(), "a=b\nline");
        assert_eq!(vars.get("PATH").unwrap(), "/shell/bin");
        assert_eq!(vars.len(), 2);
        assert!(parse_capture(b"no markers").is_err());
        assert!(parse_capture(b"<<LANTERN_ENV_BEGIN>>BAD<<LANTERN_ENV_END>>").is_err());
    }

    #[test]
    fn path_merge_and_atomic_write_and_mtime() {
        let root = std::env::temp_dir().join(format!("lantern-runtime-env-{}", std::process::id()));
        fs::create_dir_all(root.join(".cargo/bin")).unwrap();
        let saved = source_mtimes(&root);
        fs::write(root.join(".bashrc"), "export FOO=bar").unwrap();
        assert!(sources_changed(&root, &saved));
        let manager = BTreeMap::from([
            ("PATH".into(), "/bin:/manager".into()),
            ("DISPLAY".into(), ":1".into()),
        ]);
        let captured = BTreeMap::from([
            ("PATH".into(), "/shell:/bin".into()),
            ("A".into(), "a'b".into()),
            ("DISPLAY".into(), ":9".into()),
        ]);
        let vars = merge_environment(&captured, &manager, &root);
        assert_eq!(
            vars["PATH"],
            format!("/shell:/bin:/manager:{}", root.join(".cargo/bin").display())
        );
        assert!(!vars.contains_key("DISPLAY"));
        let path = root.join("shell-env.sh");
        write_shell_env(&path, &vars).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("export A='a'\\''b'\n")
        );
        let old = fs::read(&path).unwrap();
        assert!(write_shell_env(&path, &BTreeMap::from([("bad-key".into(), "x".into())])).is_err());
        assert_eq!(old, fs::read(&path).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn hanging_shell_is_bounded() {
        let root = std::env::temp_dir().join(format!("lantern-runtime-timeout-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let shell = root.join("shell");
        fs::write(&shell, "#!/bin/sh\nsleep 20 &\nwait\n").unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        let start = std::time::Instant::now();
        assert!(matches!(
            capture(&shell, Duration::from_millis(150)).await,
            Err(CaptureError::Timeout)
        ));
        assert!(start.elapsed() < Duration::from_secs(3));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "real system: runs the user's interactive login shell"]
    async fn captures_own_login_shell() {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/bash"));
        let result = capture(&shell, Duration::from_secs(10)).await.unwrap();
        assert!(result.vars.contains_key("PATH"));
        assert!(!result.vars.contains_key("LANTERN_RESOLVING_ENV"));
    }
}

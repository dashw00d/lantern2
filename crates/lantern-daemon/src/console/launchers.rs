//! Saved command-line launchers, each run as a transient systemd user unit.
//!
//! A launcher is a name, a shell command, an optional working directory and an optional URL
//! (for commands that serve a web page, such as ComfyUI). The list lives in
//! `<config>/launchers.json`. Running state is read back from systemd, never cached, so a
//! command started before a daemon restart still shows as running.

use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::{process::Command, sync::Mutex};

/// One saved command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Launcher {
    /// Stable slug, used in unit names and URLs.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Shell command line, run with `bash -lc`.
    pub cmd: String,
    /// Working directory; `~` expands to the home directory. Default: home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Page the command serves, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Fields a client sends to create or replace a launcher.
#[derive(Debug, Clone, Deserialize)]
pub struct LauncherInput {
    /// Display name.
    pub name: String,
    /// Shell command line.
    pub cmd: String,
    /// Working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Page the command serves.
    #[serde(default)]
    pub url: Option<String>,
}

/// A launcher with its live systemd state.
#[derive(Debug, Clone, Serialize)]
pub struct LauncherView {
    /// The saved launcher.
    #[serde(flatten)]
    pub launcher: Launcher,
    /// `running`, `starting`, `stopping`, `failed` or `stopped`.
    pub status: String,
    /// Exit status of the last run, when it failed.
    pub exit_code: Option<i32>,
    /// When the current run started, as systemd reports it.
    pub started_at: Option<String>,
    /// The systemd unit name.
    pub unit: String,
}

/// Errors with an API code.
#[derive(Debug)]
pub struct LauncherError {
    /// HTTP status.
    pub status: u16,
    /// Stable error code.
    pub code: &'static str,
    /// Human message.
    pub message: String,
}
impl LauncherError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}
impl From<io::Error> for LauncherError {
    fn from(e: io::Error) -> Self {
        Self::new(500, "io_error", e.to_string())
    }
}

/// The launcher store and runner.
#[derive(Debug)]
pub struct Launchers {
    file: PathBuf,
    home: PathBuf,
    unit_prefix: String,
    slice: String,
    list: Mutex<Vec<Launcher>>,
    /// systemd invocation id of each launcher's latest run, so output shows only that run.
    runs: Mutex<std::collections::HashMap<String, String>>,
}

impl Launchers {
    /// Load `file` (missing means empty). Units are named `<unit_prefix><id>.service`.
    pub fn load(file: PathBuf, home: PathBuf, unit_prefix: String, slice: String) -> Self {
        let list = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<Launcher>>(&bytes).ok())
            .unwrap_or_default();
        Self {
            file,
            home,
            unit_prefix,
            slice,
            list: Mutex::new(list),
            runs: Mutex::default(),
        }
    }

    fn unit(&self, id: &str) -> String {
        format!("{}{id}.service", self.unit_prefix)
    }

    /// Every launcher with its live state, in saved order.
    pub async fn list(&self) -> Vec<LauncherView> {
        let list = self.list.lock().await.clone();
        let mut out = Vec::with_capacity(list.len());
        for launcher in list {
            out.push(self.view(launcher).await);
        }
        out
    }

    async fn find(&self, id: &str) -> Result<Launcher, LauncherError> {
        self.list
            .lock()
            .await
            .iter()
            .find(|l| l.id == id)
            .cloned()
            .ok_or_else(|| LauncherError::new(404, "not_found", format!("no command named {id}")))
    }

    async fn view(&self, launcher: Launcher) -> LauncherView {
        let unit = self.unit(&launcher.id);
        let props = systemctl_show(&unit).await;
        let get = |k: &str| props.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        let active = get("ActiveState").unwrap_or("inactive");
        let status = match active {
            "active" | "reloading" => "running",
            "activating" => "starting",
            "deactivating" => "stopping",
            "failed" => "failed",
            _ => "stopped",
        };
        let exit_code = (status == "failed")
            .then(|| get("ExecMainStatus").and_then(|v| v.parse().ok()))
            .flatten();
        let started_at = (status == "running")
            .then(|| {
                get("ExecMainStartTimestamp")
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            })
            .flatten();
        LauncherView {
            launcher,
            status: status.into(),
            exit_code,
            started_at,
            unit,
        }
    }

    /// Add a launcher, deriving a unique id from its name.
    pub async fn create(&self, input: LauncherInput) -> Result<LauncherView, LauncherError> {
        let input = validate(input)?;
        let mut list = self.list.lock().await;
        let base = slug(&input.name);
        let mut id = base.clone();
        let mut n = 2;
        while list.iter().any(|l| l.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        let launcher = Launcher {
            id,
            name: input.name,
            cmd: input.cmd,
            cwd: input.cwd,
            url: input.url,
        };
        list.push(launcher.clone());
        save(&self.file, &list)?;
        drop(list);
        Ok(self.view(launcher).await)
    }

    /// Replace a launcher's fields, keeping its id.
    pub async fn update(&self, id: &str, input: LauncherInput) -> Result<LauncherView, LauncherError> {
        let input = validate(input)?;
        let mut list = self.list.lock().await;
        let slot = list
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or_else(|| LauncherError::new(404, "not_found", format!("no command named {id}")))?;
        slot.name = input.name;
        slot.cmd = input.cmd;
        slot.cwd = input.cwd;
        slot.url = input.url;
        let launcher = slot.clone();
        save(&self.file, &list)?;
        drop(list);
        Ok(self.view(launcher).await)
    }

    /// Stop (if running) and forget a launcher.
    pub async fn delete(&self, id: &str) -> Result<(), LauncherError> {
        self.find(id).await?;
        let unit = self.unit(id);
        let _ = systemctl(&["stop", &unit]).await;
        let _ = systemctl(&["reset-failed", &unit]).await;
        let mut list = self.list.lock().await;
        list.retain(|l| l.id != id);
        save(&self.file, &list)?;
        Ok(())
    }

    /// Start a launcher as `<prefix><id>.service`.
    pub async fn start(&self, id: &str) -> Result<LauncherView, LauncherError> {
        let launcher = self.find(id).await?;
        let unit = self.unit(id);
        let current = self.view(launcher.clone()).await;
        if matches!(current.status.as_str(), "running" | "starting") {
            return Err(LauncherError::new(
                409,
                "already_running",
                format!("{} is already running", launcher.name),
            ));
        }
        let _ = systemctl(&["reset-failed", &unit]).await;
        let cwd = expand(launcher.cwd.as_deref().unwrap_or("~"), &self.home);
        if !cwd.is_dir() {
            return Err(LauncherError::new(
                400,
                "bad_cwd",
                format!("working directory {} does not exist", cwd.display()),
            ));
        }
        let output = Command::new("systemd-run")
            .arg("--user")
            .arg(format!("--unit={unit}"))
            .arg(format!("--slice={}", self.slice))
            .arg(format!("--description=Lantern command: {}", launcher.name))
            .arg(format!("--working-directory={}", cwd.display()))
            .arg("--property=KillMode=control-group")
            .arg("--")
            .arg("/bin/bash")
            .arg("-lc")
            .arg(&launcher.cmd)
            .output()
            .await?;
        if !output.status.success() {
            return Err(LauncherError::new(
                500,
                "start_failed",
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        if let Some(invocation) = invocation_id(&unit).await {
            self.runs.lock().await.insert(launcher.id.clone(), invocation);
        }
        Ok(self.view(launcher).await)
    }

    /// Stop a running launcher.
    pub async fn stop(&self, id: &str) -> Result<LauncherView, LauncherError> {
        let launcher = self.find(id).await?;
        let unit = self.unit(id);
        let _ = systemctl(&["stop", &unit]).await;
        Ok(self.view(launcher).await)
    }

    /// The last `tail` journal lines of a launcher's unit.
    pub async fn logs(&self, id: &str, tail: usize) -> Result<Vec<String>, LauncherError> {
        self.find(id).await?;
        let unit = self.unit(id);
        // Only the latest run: the live unit's invocation, else the one recorded at start.
        let invocation = match invocation_id(&unit).await {
            Some(live) => Some(live),
            None => self.runs.lock().await.get(id).cloned(),
        };
        let mut journal = Command::new("journalctl");
        journal
            .args(["--user", "--no-pager", "-o", "short-iso", "-n"])
            .arg(tail.clamp(1, 5000).to_string());
        match invocation {
            Some(inv) => journal.arg(format!("_SYSTEMD_INVOCATION_ID={inv}")),
            None => journal.arg("-u").arg(&unit),
        };
        let output = journal.output().await?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.starts_with("-- "))
            .map(str::to_owned)
            .collect())
    }
}

fn validate(mut input: LauncherInput) -> Result<LauncherInput, LauncherError> {
    input.name = input.name.trim().to_string();
    input.cmd = input.cmd.trim().to_string();
    input.cwd = input.cwd.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    input.url = input.url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if input.name.is_empty() || input.cmd.is_empty() {
        return Err(LauncherError::new(
            400,
            "bad_request",
            "a command needs a name and a command line",
        ));
    }
    if input.cmd.contains('\n') {
        return Err(LauncherError::new(
            400,
            "bad_request",
            "the command must be one line",
        ));
    }
    if let Some(url) = &input.url
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err(LauncherError::new(
            400,
            "bad_request",
            "the URL must start with http:// or https://",
        ));
    }
    Ok(input)
}

/// A unit-name-safe slug: lowercase ASCII letters, digits and single dashes.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "command".into() } else { out }
}

fn expand(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

fn save(file: &Path, list: &[Launcher]) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(list).map_err(io::Error::other)?)?;
    std::fs::rename(tmp, file)
}

async fn systemctl(args: &[&str]) -> io::Result<std::process::Output> {
    Command::new("systemctl").arg("--user").args(args).output().await
}

async fn invocation_id(unit: &str) -> Option<String> {
    let output = systemctl(&["show", unit, "-p", "InvocationID", "--value"])
        .await
        .ok()?;
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}

async fn systemctl_show(unit: &str) -> Vec<(String, String)> {
    let Ok(output) = systemctl(&[
        "show",
        unit,
        "-p",
        "ActiveState",
        "-p",
        "ExecMainStatus",
        "-p",
        "ExecMainStartTimestamp",
    ])
    .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, cmd: &str) -> LauncherInput {
        LauncherInput {
            name: name.into(),
            cmd: cmd.into(),
            cwd: None,
            url: None,
        }
    }

    #[test]
    fn slugs_are_unit_safe() {
        assert_eq!(slug("ComfyUI (GPU)"), "comfyui-gpu");
        assert_eq!(slug("  Blender 4.5 "), "blender-4-5");
        assert_eq!(slug("!!!"), "command");
    }

    #[test]
    fn validation_trims_and_rejects() {
        let ok = validate(LauncherInput {
            cwd: Some("  ".into()),
            url: Some(" http://127.0.0.1:8188 ".into()),
            ..input(" Comfy ", " python main.py ")
        })
        .unwrap();
        assert_eq!((ok.name.as_str(), ok.cmd.as_str()), ("Comfy", "python main.py"));
        assert_eq!(ok.cwd, None);
        assert_eq!(ok.url.as_deref(), Some("http://127.0.0.1:8188"));
        assert!(validate(input("", "x")).is_err());
        assert!(validate(input("x", "a\nb")).is_err());
        assert!(
            validate(LauncherInput {
                url: Some("javascript:alert(1)".into()),
                ..input("x", "y")
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn create_update_delete_persist_with_unique_ids() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("launchers.json");
        let store = Launchers::load(
            file.clone(),
            dir.path().into(),
            "lantern-test-console-cmd-".into(),
            "app-lantern-test.slice".into(),
        );
        let a = store.create(input("Comfy", "true")).await.unwrap();
        let b = store.create(input("Comfy", "true")).await.unwrap();
        assert_eq!(
            (a.launcher.id.as_str(), b.launcher.id.as_str()),
            ("comfy", "comfy-2")
        );
        assert_eq!(a.unit, "lantern-test-console-cmd-comfy.service");
        store
            .update("comfy-2", input("Blender", "blender"))
            .await
            .unwrap();
        let reloaded = Launchers::load(file, dir.path().into(), "p-".into(), "s.slice".into());
        let names: Vec<_> = reloaded
            .list
            .lock()
            .await
            .iter()
            .map(|l| l.name.clone())
            .collect();
        assert_eq!(names, ["Comfy", "Blender"]);
        reloaded.delete("comfy").await.unwrap();
        assert_eq!(reloaded.list.lock().await.len(), 1);
        assert_eq!(reloaded.find("nope").await.unwrap_err().status, 404);
    }

    #[test]
    fn home_expansion() {
        let home = Path::new("/home/u");
        assert_eq!(expand("~", home), PathBuf::from("/home/u"));
        assert_eq!(expand("~/dev", home), PathBuf::from("/home/u/dev"));
        assert_eq!(expand("/opt", home), PathBuf::from("/opt"));
    }
}

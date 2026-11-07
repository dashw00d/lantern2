//! Read-only v1 detection and an explicitly guarded fake-root cleanup exercise.
use super::run_checked;
use crate::{
    command::{CommandRunner, CommandSpec},
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome, StepState, preflight},
};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

/// One detected v1 artifact and the proposed action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Artifact path within the selected root.
    pub path: PathBuf,
    /// Intended action; informational and not a shell command.
    pub action: &'static str,
    /// True when this item is reported without deletion.
    pub report_only: bool,
}
/// Build the v1 cleanup plan without modifying files or services.
pub fn detect(paths: &LanternPaths) -> Vec<Artifact> {
    let mut result = Vec::new();
    let mut add = |path: PathBuf, action, report_only| {
        if fs::symlink_metadata(&path).is_ok() {
            result.push(Artifact {
                path,
                action,
                report_only,
            });
        }
    };
    for (p, action) in [
        (
            "/etc/systemd/system/lanternd.service",
            "disable and remove v1 daemon unit",
        ),
        ("/etc/sudoers.d/lantern", "remove v1 sudoers rule"),
        (
            "/etc/NetworkManager/dnsmasq.d/lantern.conf",
            "remove NetworkManager DNS override",
        ),
        ("/etc/dnsmasq.d/lantern.conf", "remove dnsmasq DNS override"),
    ] {
        add(paths.system(Path::new(p)), action, false);
    }
    let sites = paths.system(Path::new("/etc/caddy/sites.d"));
    if let Ok(entries) = fs::read_dir(&sites) {
        let mut found: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "caddy"))
            .collect();
        found.sort();
        for p in found {
            add(p, "remove v1 Caddy site", false);
        }
    }
    let caddy = paths.system(Path::new("/etc/caddy/Caddyfile"));
    if fs::read_to_string(&caddy).is_ok_and(|s| s.contains("import /etc/caddy/sites.d")) {
        add(
            caddy,
            "check and disable v1 system Caddy; preserve Caddyfile",
            true,
        );
    }
    for part in ["daemon", "desktop", "cli", "packaging"] {
        add(
            paths.system(Path::new("/opt/lantern")).join(part),
            "check package ownership before removal",
            true,
        );
    }
    let cli = paths.system(Path::new("/usr/local/bin/lantern"));
    if fs::read_link(&cli).is_ok_and(|target| target.starts_with("/opt/lantern")) {
        add(cli, "remove v1 symlink after ownership check", true);
    }
    let trust = paths.system(Path::new("/usr/local/share/ca-certificates"));
    if let Ok(entries) = fs::read_dir(trust) {
        let mut found: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with("Caddy") && s.ends_with(".crt"))
            })
            .collect();
        found.sort();
        for p in found {
            add(p, "report old Caddy CA; do not remove", true);
        }
    }
    result
}
fn no_symlink_parents(root: &Path, path: &Path) -> Result<(), String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| "artifact is outside fake root")?;
    let mut cursor = root.to_path_buf();
    for part in rel.components().take(rel.components().count().saturating_sub(1)) {
        if !matches!(part, Component::Normal(_)) {
            return Err("invalid fake-root path".into());
        }
        cursor.push(part);
        if fs::symlink_metadata(&cursor)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("symlinked parent in fake root".into());
        }
    }
    Ok(())
}
/// Report detected artifacts; cleanup is permitted only with an explicit fake-root guard.
pub struct V1MigrateStep<'a> {
    /// Resolved paths.
    pub paths: &'a LanternPaths,
    /// Command execution boundary.
    pub runner: &'a dyn CommandRunner,
    /// Explicit authorization to exercise cleanup inside the fake root.
    pub allow_fake_cleanup: bool,
}
impl std::fmt::Debug for V1MigrateStep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V1MigrateStep")
            .field("allow_fake_cleanup", &self.allow_fake_cleanup)
            .finish_non_exhaustive()
    }
}
impl Step for V1MigrateStep<'_> {
    fn id(&self) -> &'static str {
        "migrate_v1"
    }
    fn description(&self) -> &'static str {
        "Report v1 system artifacts"
    }
    fn needs_root(&self) -> bool {
        true
    }
    fn fix_command(&self) -> &'static str {
        "sudo lantern init --system-steps --migrate-v1"
    }
    fn check(&self) -> StepState {
        let artifacts = detect(self.paths);
        if artifacts.iter().all(|a| a.report_only) {
            if artifacts.is_empty() {
                StepState::Done("no v1 artifacts found".into())
            } else {
                StepState::Skipped(format!("{} artifacts require manual review", artifacts.len()))
            }
        } else {
            StepState::Pending(format!("{} v1 artifacts found", artifacts.len()))
        }
    }
    fn apply(&self) -> StepOutcome {
        if let Err(out) = preflight(self.id(), self.check()) {
            return out;
        }
        let Some(root) = self.paths.fake_root.as_ref().filter(|_| self.allow_fake_cleanup) else {
            return StepOutcome::new(
                self.id(),
                OutcomeState::Skipped,
                "v1 cleanup is report-only outside an explicitly authorized fake root",
            );
        };
        let result = (|| -> Result<(), String> {
            for artifact in detect(self.paths).into_iter().filter(|a| !a.report_only) {
                no_symlink_parents(root, &artifact.path)?;
                let path = &artifact.path;
                if path
                    == &self
                        .paths
                        .system(Path::new("/etc/systemd/system/lanternd.service"))
                {
                    run_checked(
                        self.runner,
                        &CommandSpec::new("/usr/bin/systemctl", ["disable", "--now", "lanternd"], true),
                    )?;
                }
                fs::remove_file(path).map_err(|e| e.to_string())?;
                if path
                    == &self
                        .paths
                        .system(Path::new("/etc/systemd/system/lanternd.service"))
                {
                    run_checked(
                        self.runner,
                        &CommandSpec::new("/usr/bin/systemctl", ["daemon-reload"], true),
                    )?;
                }
                if path
                    == &self
                        .paths
                        .system(Path::new("/etc/NetworkManager/dnsmasq.d/lantern.conf"))
                {
                    run_checked(
                        self.runner,
                        &CommandSpec::new("/usr/bin/nmcli", ["general", "reload", "dns-full"], true),
                    )?;
                }
                if path == &self.paths.system(Path::new("/etc/dnsmasq.d/lantern.conf")) {
                    run_checked(
                        self.runner,
                        &CommandSpec::new("/usr/bin/systemctl", ["reload-or-restart", "dnsmasq"], true),
                    )?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => StepOutcome::new(
                self.id(),
                OutcomeState::Changed,
                "removed guarded fake-root artifacts; report-only items remain",
            ),
            Err(e) => StepOutcome::new(self.id(), OutcomeState::Failed, e),
        }
    }
}

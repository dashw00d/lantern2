//! Transient user units, with a fake backend for supervisor tests.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use lantern_core::{Timestamp, ids::InstanceId};
use thiserror::Error;
use tokio::process::Command;

/// Naming and slice configuration for managed and one-shot units.
#[derive(Debug, Clone)]
pub struct UnitNames {
    /// Prefix for persistent instance units, including the trailing dash.
    pub instance_prefix: String,
    /// Prefix for operation units, including the trailing dash.
    pub operation_prefix: String,
    /// User slice to which units belong.
    pub slice: String,
}

impl Default for UnitNames {
    fn default() -> Self {
        Self {
            instance_prefix: "lantern-inst-".into(),
            operation_prefix: "lantern-op-".into(),
            slice: "app-lantern.slice".into(),
        }
    }
}

impl UnitNames {
    /// Form a systemd unit name from the safe instance name.
    pub fn instance(&self, id: &InstanceId) -> String {
        format!("{}{}.service", self.instance_prefix, id.safe_name())
    }
    /// Form a one-shot unit name, using a safe, nonempty operation verb.
    pub fn operation(&self, id: &InstanceId, verb: &str, seq: u64) -> String {
        assert!(
            !verb.is_empty() && verb.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "invalid operation verb"
        );
        format!("{}{}-{verb}-{seq}.service", self.operation_prefix, id.safe_name())
    }
}

/// Description of a launcher-backed transient unit.
#[derive(Debug, Clone)]
pub struct UnitStart {
    /// Full service name, including `.service`.
    pub unit: String,
    /// Raw instance identifier exported into the unit environment.
    pub instance: InstanceId,
    /// The slice to use, such as `app-lantern.slice`.
    pub slice: String,
    /// Unit working directory.
    pub cwd: PathBuf,
    /// Main launcher script, interpreted by Bash.
    pub script: PathBuf,
    /// Optional post-stop script, interpreted by Bash.
    pub post_stop: Option<PathBuf>,
    /// systemd's TERM-to-KILL grace period.
    pub stop_timeout: Duration,
}

/// Failure from a user-systemd command or malformed command input.
#[derive(Debug, Error)]
pub enum UnitError {
    /// A command could not be spawned or read.
    #[error("systemd command failed: {0}")]
    Io(#[from] std::io::Error),
    /// A command exited nonzero.
    #[error("{command} failed ({status}): {stderr}")]
    Command {
        command: String,
        status: String,
        stderr: String,
    },
    /// A systemd command exceeded its deadline.
    #[error("{0} timed out")]
    Timeout(String),
    /// A supplied unit name, slice, path or output was malformed.
    #[error("invalid systemd input: {0}")]
    Invalid(String),
}

fn path_text(path: &Path) -> Result<&str, UnitError> {
    path.to_str()
        .filter(|s| !s.contains('\0'))
        .ok_or_else(|| UnitError::Invalid("path is not NUL-free UTF-8".into()))
}

fn valid_unit(unit: &str, suffix: &str) -> bool {
    !unit.starts_with('-')
        && unit.ends_with(suffix)
        && unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn exec_path(path: &Path) -> Result<String, UnitError> {
    let path = path_text(path)?;
    // ExecStopPost is a systemd command line, not a shell command.
    Ok(format!(
        "\"{}\"",
        path.replace('%', "%%").replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Build `systemd-run` arguments without invoking a shell.
pub fn start_args(spec: &UnitStart) -> Result<Vec<String>, UnitError> {
    if !valid_unit(&spec.unit, ".service") || !valid_unit(&spec.slice, ".slice") {
        return Err(UnitError::Invalid("invalid unit or slice name".into()));
    }
    let unit_stem = spec.unit.strip_suffix(".service").unwrap_or(&spec.unit);
    let timeout = spec.stop_timeout.as_secs_f64();
    let mut args = vec![
        "--user".into(),
        format!("--unit={}", spec.unit),
        format!("--slice={}", spec.slice),
        format!("--description=Lantern: {}", spec.instance),
        "--service-type=exec".into(),
        "--expand-environment=no".into(),
        "--quiet".into(),
        "--property=RemainAfterExit=yes".into(),
        format!("--property=WorkingDirectory={}", path_text(&spec.cwd)?),
        "--property=KillMode=control-group".into(),
        format!("--property=TimeoutStopSec={timeout}s"),
        "--property=StandardInput=null".into(),
        "--property=StandardOutput=journal".into(),
        "--property=StandardError=journal".into(),
        format!("--property=SyslogIdentifier={unit_stem}"),
        "--property=LogRateLimitIntervalSec=10s".into(),
        "--property=LogRateLimitBurst=20000".into(),
        format!("--property=Environment=LANTERN_INSTANCE_ID={}", spec.instance),
    ];
    if let Some(post) = &spec.post_stop {
        args.push(format!("--property=ExecStopPost=/bin/bash {}", exec_path(post)?));
    }
    args.extend(["--".into(), "/bin/bash".into(), path_text(&spec.script)?.into()]);
    Ok(args)
}

/// systemd's active state, retaining unrecognized values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Active {
    /// Active unit.
    Active,
    /// Inactive unit.
    Inactive,
    /// Failed unit.
    Failed,
    /// Transitioning to active.
    Activating,
    /// Transitioning to inactive.
    Deactivating,
    /// A systemd value not known by this version.
    Other(String),
}

/// systemd's load state, retaining unrecognized values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load {
    /// Unit definition loaded.
    Loaded,
    /// No such unit is loaded.
    NotFound,
    /// A systemd value not known by this version.
    Other(String),
}

/// The meaning of `ExecMainStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCodeKind {
    /// The process exited normally.
    Exited,
    /// The process was killed by a signal.
    Killed,
    /// The process dumped core.
    Dumped,
    /// No exit has been observed or the code is not recognized.
    Unknown,
}

/// One parsed `systemctl show` property block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitSnapshot {
    /// The exact systemd service name.
    pub unit: String,
    /// Unit load state.
    pub load: Load,
    /// Unit active state.
    pub active: Active,
    /// Unit substate, such as `running` or `exited`.
    pub sub: String,
    /// systemd Result property.
    pub result: String,
    /// Current main PID, if nonzero.
    pub main_pid: Option<u32>,
    /// Exit status or signal, if an exit was recorded.
    pub exit_code: Option<i32>,
    /// Interpretation of the recorded exit status.
    pub exit_kind: ExitCodeKind,
    /// Main process start timestamp, falling back to active-enter time.
    pub started_at: Option<Timestamp>,
    /// Invocation identifier, if available.
    pub invocation_id: Option<String>,
    /// Control-group path, if available.
    pub cgroup: Option<String>,
}

fn unix_timestamp(value: &str) -> Option<Timestamp> {
    let value = value.trim().strip_prefix('@')?;
    let (seconds, fraction) = value.split_once('.').unwrap_or((value, ""));
    let sec = seconds.parse::<i64>().ok()?;
    let nanos = if fraction.is_empty() {
        0
    } else {
        if fraction.len() > 9 || !fraction.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        format!("{fraction:0<9}").parse::<i32>().ok()?
    };
    Timestamp::new(sec, nanos).ok()
}

/// Parse blank-line-separated blocks from one batched `systemctl show` call.
pub fn parse_show(output: &str) -> Result<Vec<UnitSnapshot>, UnitError> {
    let mut snapshots = Vec::new();
    for block in output.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let props: BTreeMap<&str, &str> = block.lines().filter_map(|line| line.split_once('=')).collect();
        let get = |key| props.get(key).copied().unwrap_or("");
        let unit = get("Id");
        if unit.is_empty() {
            return Err(UnitError::Invalid("systemctl show block missing Id".into()));
        }
        let exit_kind = match get("ExecMainCode") {
            "1" | "exited" => ExitCodeKind::Exited,
            "2" | "killed" => ExitCodeKind::Killed,
            "3" | "dumped" => ExitCodeKind::Dumped,
            _ => ExitCodeKind::Unknown,
        };
        let nonempty = |key| match get(key) {
            "" | "0" | "n/a" => None,
            text => Some(text.to_owned()),
        };
        snapshots.push(UnitSnapshot {
            unit: unit.to_owned(),
            load: match get("LoadState") {
                "loaded" => Load::Loaded,
                "not-found" => Load::NotFound,
                v => Load::Other(v.into()),
            },
            active: match get("ActiveState") {
                "active" => Active::Active,
                "inactive" => Active::Inactive,
                "failed" => Active::Failed,
                "activating" => Active::Activating,
                "deactivating" => Active::Deactivating,
                v => Active::Other(v.into()),
            },
            sub: get("SubState").into(),
            result: get("Result").into(),
            main_pid: get("MainPID").parse::<u32>().ok().filter(|pid| *pid != 0),
            exit_code: if exit_kind == ExitCodeKind::Unknown {
                None
            } else {
                get("ExecMainStatus").parse().ok()
            },
            exit_kind,
            started_at: unix_timestamp(get("ExecMainStartTimestamp"))
                .or_else(|| unix_timestamp(get("ActiveEnterTimestamp"))),
            invocation_id: nonempty("InvocationID"),
            cgroup: nonempty("ControlGroup"),
        });
    }
    Ok(snapshots)
}

/// Boxed asynchronous operation returned by a user-systemd backend.
pub type SystemdFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, UnitError>> + Send + 'a>>;

/// OS boundary for starting, observing and cleaning up user units.
pub trait UserSystemd: Send + Sync {
    /// Stop/reset a stale unit and start a transient unit.
    fn start<'a>(&'a self, spec: &'a UnitStart) -> SystemdFuture<'a, ()>;
    /// Stop a unit, capped at its stop timeout plus 15 seconds.
    fn stop<'a>(&'a self, unit: &'a str, stop_timeout: Duration) -> SystemdFuture<'a, ()>;
    /// Clear a failed unit so the same name can be started again.
    fn reset_failed<'a>(&'a self, unit: &'a str) -> SystemdFuture<'a, ()>;
    /// Poll explicitly named units in one `systemctl show` command.
    fn poll<'a>(&'a self, units: &'a [String]) -> SystemdFuture<'a, Vec<UnitSnapshot>>;
    /// List loaded service units matching a caller-supplied prefix.
    fn list_prefix<'a>(&'a self, prefix: &'a str) -> SystemdFuture<'a, Vec<String>>;
}

/// A launcher-backed one-shot unit with a wall-clock deadline.
#[derive(Debug, Clone)]
pub struct OneShotSpec {
    /// Unit to start, normally named with [`UnitNames::operation`].
    pub unit: UnitStart,
    /// Total allowed time, including starting the unit.
    pub timeout: Duration,
    /// Delay between durable-state polls.
    pub poll_interval: Duration,
}

/// Recorded exit information from a one-shot unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotResult {
    /// Exit status or signal number, when reported by systemd.
    pub exit_code: Option<i32>,
    /// Whether the main process exited or was signaled.
    pub exit_kind: ExitCodeKind,
    /// systemd's result string.
    pub result: String,
}

/// Failure to run a one-shot or clean up its unit.
#[derive(Debug, Error)]
pub enum OneShotError {
    /// A systemd operation failed.
    #[error(transparent)]
    Unit(#[from] UnitError),
    /// The one-shot ran beyond its deadline and was stopped.
    #[error("one-shot timed out")]
    Timeout,
    /// Cancellation was requested and the unit was stopped.
    #[error("one-shot cancelled")]
    Cancelled,
}

async fn cleanup_one_shot(backend: &dyn UserSystemd, spec: &OneShotSpec) -> Result<(), UnitError> {
    let stop = backend.stop(&spec.unit.unit, spec.unit.stop_timeout).await;
    let reset = backend.reset_failed(&spec.unit.unit).await;
    stop.and(reset)
}

/// Run a one-shot through journald, stopping its cgroup on cancellation or timeout.
///
/// The caller supplies a cancellation future; an actor can wrap this primitive in
/// its `OperationHandle`. The transient unit is stopped and reset on every exit.
pub async fn run_oneshot(
    backend: &dyn UserSystemd,
    spec: &OneShotSpec,
    cancel: impl Future<Output = ()> + Send,
) -> Result<OneShotResult, OneShotError> {
    let deadline = tokio::time::sleep(spec.timeout);
    tokio::pin!(deadline);
    tokio::pin!(cancel);
    let started = tokio::select! {
        biased;
        () = &mut cancel => Err(OneShotError::Cancelled),
        () = &mut deadline => Err(OneShotError::Timeout),
        result = backend.start(&spec.unit) => result.map_err(OneShotError::Unit),
    };
    if let Err(error) = started {
        cleanup_one_shot(backend, spec).await?;
        return Err(error);
    }
    let units = [spec.unit.unit.clone()];
    let result = loop {
        let polled = tokio::select! {
            biased;
            () = &mut cancel => break Err(OneShotError::Cancelled),
            () = &mut deadline => break Err(OneShotError::Timeout),
            result = backend.poll(&units) => result,
        };
        match polled {
            Err(error) => break Err(OneShotError::Unit(error)),
            Ok(rows) => {
                if let Some(row) = rows.into_iter().find(|row| {
                    row.active == Active::Failed || (row.active == Active::Active && row.sub == "exited")
                }) {
                    break Ok(OneShotResult {
                        exit_code: row.exit_code,
                        exit_kind: row.exit_kind,
                        result: row.result,
                    });
                }
            }
        }
        tokio::select! {
            biased;
            () = &mut cancel => break Err(OneShotError::Cancelled),
            () = &mut deadline => break Err(OneShotError::Timeout),
            () = tokio::time::sleep(spec.poll_interval) => {},
        }
    };
    cleanup_one_shot(backend, spec).await?;
    result
}

/// Production backend that shells out to systemd user commands.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemctlBackend;

async fn run(program: &str, args: &[String], timeout: Duration) -> Result<String, UnitError> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| UnitError::Timeout(program.into()))??;
    if !output.status.success() {
        return Err(UnitError::Command {
            command: program.into(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl SystemctlBackend {
    async fn absent(&self, unit: &str) -> bool {
        self.poll(&[unit.to_owned()])
            .await
            .is_ok_and(|rows| rows.is_empty() || rows.iter().all(|row| row.load == Load::NotFound))
    }
}

impl UserSystemd for SystemctlBackend {
    fn start<'a>(&'a self, spec: &'a UnitStart) -> SystemdFuture<'a, ()> {
        Box::pin(async move {
            let args = start_args(spec)?;
            let _ = self.stop(&spec.unit, spec.stop_timeout).await;
            let _ = self.reset_failed(&spec.unit).await;
            run("systemd-run", &args, Duration::from_secs(30)).await?;
            Ok(())
        })
    }
    fn stop<'a>(&'a self, unit: &'a str, stop_timeout: Duration) -> SystemdFuture<'a, ()> {
        Box::pin(async move {
            if !valid_unit(unit, ".service") {
                return Err(UnitError::Invalid("invalid unit".into()));
            }
            let result = run(
                "systemctl",
                &["--user".into(), "stop".into(), "--".into(), unit.into()],
                stop_timeout.saturating_add(Duration::from_secs(15)),
            )
            .await;
            match result {
                Ok(_) => Ok(()),
                Err(_) if self.absent(unit).await => Ok(()),
                Err(error) => Err(error),
            }
        })
    }
    fn reset_failed<'a>(&'a self, unit: &'a str) -> SystemdFuture<'a, ()> {
        Box::pin(async move {
            if !valid_unit(unit, ".service") {
                return Err(UnitError::Invalid("invalid unit".into()));
            }
            let result = run(
                "systemctl",
                &["--user".into(), "reset-failed".into(), "--".into(), unit.into()],
                Duration::from_secs(15),
            )
            .await;
            match result {
                Ok(_) => Ok(()),
                Err(_) if self.absent(unit).await => Ok(()),
                Err(error) => Err(error),
            }
        })
    }
    fn poll<'a>(&'a self, units: &'a [String]) -> SystemdFuture<'a, Vec<UnitSnapshot>> {
        Box::pin(async move {
            if units.is_empty() {
                return Ok(Vec::new());
            }
            if units.iter().any(|u| !valid_unit(u, ".service")) {
                return Err(UnitError::Invalid("invalid unit".into()));
            }
            let mut args: Vec<String> = ["--user", "--timestamp=unix", "show", "--no-pager", "--property=Id,LoadState,ActiveState,SubState,Result,MainPID,ExecMainStatus,ExecMainCode,ExecMainStartTimestamp,ActiveEnterTimestamp,InvocationID,ControlGroup", "--"].into_iter().map(str::to_owned).collect();
            args.extend_from_slice(units);
            parse_show(&run("systemctl", &args, Duration::from_secs(15)).await?)
        })
    }
    fn list_prefix<'a>(&'a self, prefix: &'a str) -> SystemdFuture<'a, Vec<String>> {
        Box::pin(async move {
            if prefix.is_empty()
                || prefix.starts_with('-')
                || !prefix
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            {
                return Err(UnitError::Invalid("invalid unit prefix".into()));
            }
            let args: Vec<String> = [
                "--user",
                "list-units",
                "--all",
                "--plain",
                "--no-legend",
                "--no-pager",
                "--",
            ]
            .into_iter()
            .map(str::to_owned)
            .chain(std::iter::once(format!("{prefix}*.service")))
            .collect();
            Ok(run("systemctl", &args, Duration::from_secs(15))
                .await?
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .filter(|name| name.starts_with(prefix) && name.ends_with(".service"))
                .map(str::to_owned)
                .collect())
        })
    }
}

/// In-memory backend whose snapshots can be supplied by unit tests.
#[derive(Debug, Default)]
pub struct FakeSystemd {
    snapshots: Mutex<BTreeMap<String, UnitSnapshot>>,
    calls: Mutex<Vec<String>>,
}

impl FakeSystemd {
    /// Set or replace a snapshot returned by future polls.
    pub fn set_snapshot(&self, snapshot: UnitSnapshot) {
        self.snapshots
            .lock()
            .unwrap()
            .insert(snapshot.unit.clone(), snapshot);
    }
    /// Return the sequence of recorded backend calls.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl UserSystemd for FakeSystemd {
    fn start<'a>(&'a self, spec: &'a UnitStart) -> SystemdFuture<'a, ()> {
        Box::pin(async move {
            start_args(spec)?;
            self.record(format!("start {}", spec.unit));
            Ok(())
        })
    }
    fn stop<'a>(&'a self, unit: &'a str, _: Duration) -> SystemdFuture<'a, ()> {
        Box::pin(async move {
            self.record(format!("stop {unit}"));
            Ok(())
        })
    }
    fn reset_failed<'a>(&'a self, unit: &'a str) -> SystemdFuture<'a, ()> {
        Box::pin(async move {
            self.record(format!("reset-failed {unit}"));
            Ok(())
        })
    }
    fn poll<'a>(&'a self, units: &'a [String]) -> SystemdFuture<'a, Vec<UnitSnapshot>> {
        Box::pin(async move {
            self.record("poll".into());
            let snapshots = self.snapshots.lock().unwrap();
            Ok(units
                .iter()
                .filter_map(|name| snapshots.get(name).cloned())
                .collect())
        })
    }
    fn list_prefix<'a>(&'a self, prefix: &'a str) -> SystemdFuture<'a, Vec<String>> {
        Box::pin(async move {
            self.record(format!("list {prefix}"));
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .keys()
                .filter(|name| name.starts_with(prefix))
                .cloned()
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn example_spec() -> UnitStart {
        UnitStart {
            unit: "lantern-inst-app_feature.service".into(),
            instance: InstanceId::parse("app@feature").unwrap(),
            slice: "app-lantern.slice".into(),
            cwd: "/work/a b".into(),
            script: "/run/a b.sh".into(),
            post_stop: Some("/run/a %b.sh".into()),
            stop_timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn command_properties_and_names() {
        let names = UnitNames::default();
        let id = InstanceId::parse("app@feature").unwrap();
        assert_eq!(names.instance(&id), "lantern-inst-app_feature.service");
        assert_eq!(
            names.operation(&id, "setup", 4),
            "lantern-op-app_feature-setup-4.service"
        );
        let args = start_args(&example_spec()).unwrap();
        for arg in [
            "--user",
            "--slice=app-lantern.slice",
            "--service-type=exec",
            "--expand-environment=no",
            "--property=RemainAfterExit=yes",
            "--property=KillMode=control-group",
            "--property=TimeoutStopSec=10s",
            "--property=StandardOutput=journal",
            "--property=StandardError=journal",
            "--property=Environment=LANTERN_INSTANCE_ID=app@feature",
            "--property=ExecStopPost=/bin/bash \"/run/a %%b.sh\"",
        ] {
            assert!(args.iter().any(|a| a == arg), "missing {arg}");
        }
        assert!(
            start_args(&UnitStart {
                unit: "-bad.service".into(),
                ..example_spec()
            })
            .is_err()
        );
    }

    #[test]
    fn parses_batched_show_and_exit_details() {
        let text = "Id=lantern-inst-app.service\nLoadState=loaded\nActiveState=failed\nSubState=failed\nResult=exit-code\nMainPID=0\nExecMainCode=exited\nExecMainStatus=3\nExecMainStartTimestamp=@1760000000.123456\nInvocationID=abc\nControlGroup=\n\nId=lantern-inst-other.service\nLoadState=not-found\nActiveState=inactive\nExecMainCode=0\nExecMainStatus=0\n";
        let rows = parse_show(text).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].active, Active::Failed);
        assert_eq!(rows[0].exit_code, Some(3));
        assert_eq!(rows[0].exit_kind, ExitCodeKind::Exited);
        assert_eq!(
            rows[0].started_at.unwrap().as_microsecond(),
            1_760_000_000_123_456
        );
        assert_eq!(rows[1].load, Load::NotFound);
        assert_eq!(rows[1].exit_code, None);
        assert!(parse_show("ActiveState=failed\n").is_err());
    }

    #[tokio::test]
    async fn fake_observes_only_requested_units() {
        let fake = FakeSystemd::default();
        let snap =
            parse_show("Id=lantern-inst-app.service\nLoadState=loaded\nActiveState=active\nExecMainCode=0\n")
                .unwrap()
                .remove(0);
        fake.set_snapshot(snap);
        assert_eq!(
            fake.poll(&["lantern-inst-app.service".into(), "other.service".into()])
                .await
                .unwrap()
                .len(),
            1
        );
        fake.stop("lantern-inst-app.service", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(fake.calls(), ["poll", "stop lantern-inst-app.service"]);
    }

    #[tokio::test]
    async fn oneshot_returns_exit_and_cleans_up_or_cancels() {
        let fake = FakeSystemd::default();
        let spec = OneShotSpec {
            unit: example_spec(),
            timeout: Duration::from_secs(1),
            poll_interval: Duration::from_millis(1),
        };
        let snap = parse_show("Id=lantern-inst-app_feature.service\nLoadState=loaded\nActiveState=failed\nResult=exit-code\nExecMainCode=exited\nExecMainStatus=3\n").unwrap().remove(0);
        fake.set_snapshot(snap);
        let result = run_oneshot(&fake, &spec, std::future::pending()).await.unwrap();
        assert_eq!(result.exit_code, Some(3));
        assert_eq!(
            fake.calls(),
            [
                "start lantern-inst-app_feature.service",
                "poll",
                "stop lantern-inst-app_feature.service",
                "reset-failed lantern-inst-app_feature.service"
            ]
        );
        let fake = FakeSystemd::default();
        assert!(matches!(
            run_oneshot(&fake, &spec, async {}).await,
            Err(OneShotError::Cancelled)
        ));
        assert_eq!(
            fake.calls(),
            [
                "stop lantern-inst-app_feature.service",
                "reset-failed lantern-inst-app_feature.service"
            ]
        );
    }

    struct Cleanup {
        unit: String,
        dir: PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "stop", "--", &self.unit])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "reset-failed", "--", &self.unit])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    #[ignore = "real system: creates a dedicated transient user unit in app-lantern-test.slice"]
    async fn real_unit_exit_reset_and_restart() {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let id = InstanceId::parse(&format!("rt-{unique}")).unwrap();
        let names = UnitNames {
            instance_prefix: "lantern-test-runtime-inst-".into(),
            operation_prefix: "lantern-test-runtime-op-".into(),
            slice: "app-lantern-test.slice".into(),
        };
        let unit = names.instance(&id);
        let dir = std::env::temp_dir().join(format!("lantern-test-runtime-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        let _cleanup = Cleanup {
            unit: unit.clone(),
            dir: dir.clone(),
        };
        let script = dir.join("exit.sh");
        fs::write(&script, "#!/bin/bash\necho lantern-runtime-c5-first\nexit 3\n").unwrap();
        let spec = UnitStart {
            unit: unit.clone(),
            instance: id,
            slice: names.slice.clone(),
            cwd: dir.clone(),
            script: script.clone(),
            post_stop: None,
            stop_timeout: Duration::from_secs(1),
        };
        let backend = SystemctlBackend;
        backend.start(&spec).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let rows = backend.poll(std::slice::from_ref(&unit)).await.unwrap();
            if let Some(row) = rows.first().filter(|row| row.active == Active::Failed) {
                assert_eq!(row.exit_kind, ExitCodeKind::Exited);
                assert_eq!(row.exit_code, Some(3));
                assert!(row.started_at.is_some(), "missing real start timestamp: {row:?}");
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "unit did not fail in time: {rows:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let output = Command::new("journalctl")
                .args([
                    "--user",
                    "--output=json",
                    "--all",
                    "--no-pager",
                    "-n",
                    "100",
                    &format!("_SYSTEMD_USER_UNIT={unit}"),
                ])
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8_lossy(&output.stdout);
            let found = text
                .lines()
                .filter_map(|line| super::super::journal::parse_line(line, &names).ok().flatten())
                .any(|row| row.line.instance == spec.instance && row.line.line == "lantern-runtime-c5-first");
            if found {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "crashed test unit's journal line was not found: {text}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        backend.reset_failed(&unit).await.unwrap();
        fs::write(script, "#!/bin/bash\necho lantern-runtime-c5-second\nexit 0\n").unwrap();
        backend.start(&spec).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let rows = backend.poll(std::slice::from_ref(&unit)).await.unwrap();
            if let Some(row) = rows
                .first()
                .filter(|row| row.active == Active::Active && row.sub == "exited")
            {
                assert_eq!(row.exit_code, Some(0));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "restarted unit did not exit: {rows:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    #[ignore = "real system: runs and cancels its own one-shot unit in app-lantern-test.slice"]
    async fn real_oneshot_exit_and_cancellation() {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let id = InstanceId::parse(&format!("os-{unique}")).unwrap();
        let names = UnitNames {
            instance_prefix: "lantern-test-runtime-inst-".into(),
            operation_prefix: "lantern-test-runtime-op-".into(),
            slice: "app-lantern-test.slice".into(),
        };
        let unit = names.operation(&id, "setup", 1);
        let dir = std::env::temp_dir().join(format!("lantern-test-runtime-oneshot-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        let _cleanup = Cleanup {
            unit: unit.clone(),
            dir: dir.clone(),
        };
        let script = dir.join("script.sh");
        fs::write(&script, "#!/bin/bash\necho lantern-runtime-oneshot\nexit 3\n").unwrap();
        let spec = OneShotSpec {
            unit: UnitStart {
                unit,
                instance: id,
                slice: names.slice.clone(),
                cwd: dir,
                script: script.clone(),
                post_stop: None,
                stop_timeout: Duration::from_secs(1),
            },
            timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(50),
        };
        let backend = SystemctlBackend;
        let result = run_oneshot(&backend, &spec, std::future::pending())
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(3));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let output = Command::new("journalctl")
                .args([
                    "--user",
                    "--output=json",
                    "--all",
                    "--no-pager",
                    "-n",
                    "100",
                    &format!("_SYSTEMD_USER_UNIT={}", spec.unit.unit),
                ])
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8_lossy(&output.stdout);
            let found = text
                .lines()
                .filter_map(|line| super::super::journal::parse_line(line, &names).ok().flatten())
                .any(|row| {
                    row.line.instance == spec.unit.instance
                        && row.line.stream == lantern_core::model::Stream::Setup
                        && row.line.line == "lantern-runtime-oneshot"
                });
            if found {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "setup test unit's journal line was not found: {text}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        fs::write(script, "#!/bin/bash\nexec sleep 30\n").unwrap();
        let result = run_oneshot(&backend, &spec, tokio::time::sleep(Duration::from_millis(250))).await;
        assert!(matches!(result, Err(OneShotError::Cancelled)), "{result:?}");
        let snapshots = backend.poll(std::slice::from_ref(&spec.unit.unit)).await.unwrap();
        assert!(snapshots.iter().all(|row| row.active != Active::Active));
        let immediate = run_oneshot(&backend, &spec, async {}).await;
        assert!(matches!(immediate, Err(OneShotError::Cancelled)), "{immediate:?}");
        let mut timeout_spec = spec;
        timeout_spec.timeout = Duration::from_millis(250);
        let timed_out = run_oneshot(&backend, &timeout_spec, std::future::pending()).await;
        assert!(matches!(timed_out, Err(OneShotError::Timeout)), "{timed_out:?}");
    }
}

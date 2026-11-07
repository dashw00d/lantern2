//! Command execution against the typed client and offline core.
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::CommandFactory;
use futures_util::{StreamExt, stream::FuturesUnordered};
use lantern_client::{
    Client, ClientConfig, EventsOptions, InstanceFilter, ListenerFilter, ProjectFilter, RunOptions,
    ServiceAction, StreamEvent, Wait,
};
use lantern_core::{
    api::{
        self, InstanceView, LogParams, OperationResponse, OperationStatus, PatchProjectRequest,
        RegisterProjectRequest, RemoveInstanceParams, RunTarget, UnregisterParams, WireEvent,
    },
    ids::{InstanceId, ProjectId},
    model::{CheckoutKind, InstanceStatus, Stream},
    settings::{Dirs, ProcessEnv},
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{CliAction, SystemActions, SystemCommand, args::*, error::Failure, output, target};

type Result<T> = std::result::Result<T, Failure>;

struct Reply {
    json: Value,
    text: String,
    code: i32,
}
impl Reply {
    fn value<T: Serialize>(value: &T, text: String) -> Result<Self> {
        Ok(Self {
            json: serde_json::to_value(value).map_err(|e| Failure::new(1, "serialization", e.to_string()))?,
            text,
            code: 0,
        })
    }
    fn pretty<T: Serialize>(value: &T) -> Result<Self> {
        let text = serde_json::to_string_pretty(value)
            .map_err(|e| Failure::new(1, "serialization", e.to_string()))?
            + "\n";
        Self::value(value, text)
    }
}
struct Context<'a> {
    cli: &'a Cli,
    host: String,
    cwd: PathBuf,
    tty: bool,
    color: bool,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
}
impl Context<'_> {
    fn client(&self) -> Result<Client> {
        Client::new(ClientConfig {
            base_url: Some(self.host.clone()),
        })
        .map_err(|e| Failure::from_client(e, &self.host))
    }
    fn failure(&self, e: lantern_client::ClientError) -> Failure {
        Failure::from_client(e, &self.host)
    }
    fn emit(&mut self, reply: &Reply) -> Result<()> {
        if self.cli.json {
            writeln!(self.out, "{}", reply.json)?;
        } else {
            write!(self.out, "{}", reply.text)?;
        }
        Ok(())
    }
    fn wait(&self, args: &WaitArgs) -> Wait {
        Wait {
            wait: !args.no_wait && (!self.tty || self.cli.json),
            timeout: Some(Duration::from_secs(args.timeout)),
        }
    }
}

/// Execute parsed arguments with injected output and system actions.
pub async fn dispatch(
    cli: Cli,
    system: &dyn SystemActions,
    out: &mut dyn Write,
    err: &mut dyn Write,
    tty: bool,
) -> CliAction {
    match &cli.command {
        Some(Command::Daemon {
            command: None | Some(DaemonCommand::Run),
            dev,
        }) => return CliAction::RunDaemon(crate::DaemonArgs { dev: *dev }),
        Some(Command::Gui(args)) => return CliAction::RunGui(args.clone()),
        _ => {}
    }
    let host = cli
        .host
        .clone()
        .or_else(|| std::env::var("LANTERN_DAEMON_URL").ok())
        .unwrap_or_else(|| lantern_client::DEFAULT_DAEMON_URL.into());
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return CliAction::Exit(1);
        }
    };
    let color = tty && !cli.no_color && std::env::var_os("NO_COLOR").is_none();
    let mut ctx = Context {
        cli: &cli,
        host,
        cwd,
        tty,
        color,
        out,
        err,
    };
    let result = match ensure_daemon(&mut ctx).await {
        Ok(()) => execute(&mut ctx, system).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(Some(reply)) => {
            let code = reply.code;
            match ctx.emit(&reply) {
                Ok(()) => CliAction::Exit(code),
                Err(e) => {
                    let _ = write!(ctx.err, "{}", e.human());
                    CliAction::Exit(e.code)
                }
            }
        }
        Ok(None) => CliAction::Exit(0),
        Err(e) => {
            if ctx.cli.json {
                let _ = writeln!(ctx.err, "{}", e.json());
            } else {
                let _ = write!(ctx.err, "{}", e.human());
            }
            CliAction::Exit(e.code)
        }
    }
}

fn needs_daemon(command: Option<&Command>) -> bool {
    !matches!(
        command,
        Some(
            Command::Daemon { .. }
                | Command::Gui(_)
                | Command::Completions { .. }
                | Command::Schema { .. }
                | Command::Init { .. }
                | Command::Doctor { .. }
                | Command::Uninstall { .. }
        ) | Some(Command::Manifest {
            command: ManifestCommand::Check { .. }
        }) | Some(Command::Mcp { .. })
            | Some(Command::Config {
                command: Some(ConfigCommand::Path)
            })
    )
}

async fn ensure_daemon(ctx: &mut Context<'_>) -> Result<()> {
    let automatic =
        ctx.cli.start_daemon || std::env::var("LANTERN_START_DAEMON").ok().as_deref() == Some("1");
    let interactive = ctx.tty && !ctx.cli.json;
    if !needs_daemon(ctx.cli.command.as_ref()) || (!automatic && !interactive) {
        return Ok(());
    }
    let client = ctx.client()?;
    match client.ping().await {
        Ok(_) => Ok(()),
        Err(lantern_client::ClientError::DaemonUnreachable(_)) => {
            if !automatic {
                write!(
                    ctx.err,
                    "{}Start it now? [Y/n] ",
                    Failure::from_client(
                        lantern_client::ClientError::DaemonUnreachable(String::new()),
                        &ctx.host
                    )
                    .human()
                )?;
                ctx.err.flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes") {
                    return Err(Failure::from_client(
                        lantern_client::ClientError::DaemonUnreachable(String::new()),
                        &ctx.host,
                    ));
                }
            }
            let status = std::process::Command::new("systemctl")
                .args(["--user", "start", "lantern"])
                .output()?;
            if !status.status.success() {
                return Err(Failure::from_client(
                    lantern_client::ClientError::DaemonUnreachable(String::new()),
                    &ctx.host,
                ));
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                if client.ping().await.is_ok() {
                    return Ok(());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(Failure::from_client(
                        lantern_client::ClientError::DaemonUnreachable(String::new()),
                        &ctx.host,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        Err(error) => Err(ctx.failure(error)),
    }
}

async fn list_all(ctx: &Context<'_>, client: &Client) -> Result<Vec<InstanceView>> {
    client
        .instances(InstanceFilter::default())
        .await
        .map_err(|e| ctx.failure(e))
}
async fn resolve(ctx: &Context<'_>, client: &Client, name: Option<&str>) -> Result<InstanceId> {
    target::resolve(&list_all(ctx, client).await?, name, &ctx.cwd)
}
async fn project(ctx: &Context<'_>, client: &Client, name: Option<&str>) -> Result<ProjectId> {
    target::project(&list_all(ctx, client).await?, name, &ctx.cwd)
}
fn parse_project(name: &str) -> Result<ProjectId> {
    ProjectId::parse(name).map_err(|e| Failure::usage(e.to_string()))
}
fn branch_target(name: &str, options: &BranchOptions) -> RunTarget {
    if let Some(pr) = options.pr {
        RunTarget::Pr(pr)
    } else if options.detach {
        RunTarget::Detached(name.to_string())
    } else if options.new {
        RunTarget::NewBranch {
            name: name.to_string(),
            base: options.base.clone(),
        }
    } else {
        RunTarget::Branch(name.to_string())
    }
}
fn check_operation(result: &OperationResponse, waiting: bool) -> Result<()> {
    match result.operation.status {
        OperationStatus::Failed | OperationStatus::Cancelled => {
            Err(result.operation.error.as_ref().map_or_else(
                || Failure::new(5, "operation_failed", "operation failed"),
                |error| Failure {
                    code: 5,
                    error: error.clone(),
                },
            ))
        }
        OperationStatus::Queued | OperationStatus::Running if waiting => Err(Failure::new(
            6,
            "timeout",
            "operation is still running; check `lantern status`",
        )),
        _ => Ok(()),
    }
}
async fn wait_request<T>(
    ctx: &mut Context<'_>,
    request: impl std::future::Future<Output = std::result::Result<T, lantern_client::ClientError>>,
) -> Result<T> {
    tokio::select! {
        response = request => response.map_err(|error| ctx.failure(error)),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            if ctx.tty && !ctx.cli.json { writeln!(ctx.err, "Stopped waiting; the operation continues in the daemon.")?; }
            Err(Failure::new(1, "interrupted", "stopped waiting; the operation continues in the daemon"))
        }
    }
}

async fn finish_operation(
    ctx: &mut Context<'_>,
    client: &Client,
    mut result: OperationResponse,
    args: &WaitArgs,
) -> Result<OperationResponse> {
    if ctx.tty
        && !ctx.cli.json
        && !args.no_wait
        && matches!(
            result.operation.status,
            OperationStatus::Queued | OperationStatus::Running
        )
    {
        let mut events = client.events(EventsOptions {
            state: false,
            logs: vec![],
            tail: 0,
            op: Some(result.operation.id.clone()),
            reconnect: false,
        });
        let operation_id = result.operation.id.clone();
        let waiting = client.operation(&operation_id, Some(Duration::from_secs(args.timeout)));
        tokio::pin!(waiting);
        loop {
            tokio::select! {
                operation = &mut waiting => { result.operation = operation.map_err(|e| ctx.failure(e))?; break; }
                event = events.next() => match event {
                    Some(StreamEvent::Event(WireEvent::OperationProgress { step, .. })) => { writeln!(ctx.err, "◐ {}", output::clean(&step, false))?; }
                    None | Some(StreamEvent::Lost { .. }) => { result.operation = waiting.await.map_err(|e| ctx.failure(e))?; break; }
                    _ => {}
                },
                signal = tokio::signal::ctrl_c() => { signal?; writeln!(ctx.err, "Stopped waiting; the operation continues in the daemon.")?; return Err(Failure::new(1, "interrupted", "stopped waiting")); }
            }
        }
    }
    check_operation(&result, !args.no_wait)?;
    Ok(result)
}
fn operation_reply(result: &OperationResponse, no_wait: bool) -> Result<Reply> {
    let text = if no_wait {
        format!("{}\n", result.operation.id)
    } else {
        format!("{}  {:?}\n", result.operation.id, result.operation.status)
    };
    Reply::value(result, text)
}

async fn execute(ctx: &mut Context<'_>, system: &dyn SystemActions) -> Result<Option<Reply>> {
    let Some(command) = &ctx.cli.command else {
        return ls(ctx, &LsArgs::default()).await.map(Some);
    };
    match command {
        Command::Schema { name } => {
            let schema = match name {
                SchemaName::Manifest => lantern_core::manifest::schema::manifest_json_schema(),
                SchemaName::Profile => lantern_core::manifest::schema::profile_json_schema(),
                SchemaName::Config => lantern_core::manifest::schema::config_json_schema(),
            };
            Ok(Some(Reply::pretty(&schema)?))
        }
        Command::Manifest {
            command: ManifestCommand::Check { path },
        } => manifest_check(path.as_deref(), &ctx.cwd).map(Some),
        Command::Completions { shell } => {
            let mut bytes = Vec::new();
            clap_complete::generate(*shell, &mut Cli::command(), "lantern", &mut bytes);
            let text = String::from_utf8(bytes).map_err(|e| Failure::new(1, "encoding", e.to_string()))?;
            Ok(Some(Reply::value(&text, text.clone())?))
        }
        Command::Mcp {
            command: McpCommand::Url,
        } => {
            let url = format!("{}/mcp", ctx.host.trim_end_matches('/'));
            Ok(Some(Reply::value(&url, format!("{url}\n"))?))
        }
        Command::Mcp {
            command:
                McpCommand::Install {
                    target,
                    scope,
                    force,
                    print,
                    config_path,
                },
        } => mcp_install(ctx, *target, *scope, *force, *print, config_path.as_deref())
            .await
            .map(Some),
        Command::MigrateV1 {
            from,
            dry_run,
            yes,
            force,
        } => {
            if !dry_run && !yes {
                return Err(Failure::usage(
                    "migrate-v1 requires --yes to apply changes; use --dry-run to preview",
                ));
            }
            let result = ctx
                .client()?
                .import_v1(api::ImportV1Request {
                    from: from.clone(),
                    dry_run: *dry_run,
                    force: *force,
                })
                .await
                .map_err(|e| ctx.failure(e))?;
            Ok(Some(Reply::pretty(&result)?))
        }
        Command::Env { command } => {
            let (method, path) = match command {
                EnvCommand::Show => (reqwest::Method::GET, vec!["api", "system", "env"]),
                EnvCommand::Refresh => (reqwest::Method::POST, vec!["api", "system", "env", "refresh"]),
            };
            let value = auxiliary_request(ctx, method, &path, None).await?;
            Ok(Some(Reply::pretty(&value)?))
        }
        Command::Init {
            yes,
            system_steps,
            uid,
        } => {
            let command = if *system_steps {
                SystemCommand::InitSystem {
                    uid: uid.expect("clap requires uid"),
                }
            } else {
                SystemCommand::Init { yes: *yes }
            };
            run_system(system, command).map(Some)
        }
        Command::Doctor { fix } => run_system(system, SystemCommand::Doctor { fix: *fix }).map(Some),
        Command::Uninstall { purge_data, yes } => run_system(
            system,
            SystemCommand::Uninstall {
                purge_data: *purge_data,
                yes: *yes,
            },
        )
        .map(Some),
        Command::Config {
            command: Some(ConfigCommand::Path),
        } => {
            let dirs = Dirs::from_env(&ProcessEnv).map_err(|e| Failure::new(1, "environment", e))?;
            let path = dirs.config.join("config.toml");
            Ok(Some(Reply::value(&path, format!("{}\n", path.display()))?))
        }
        Command::Daemon {
            command: Some(cmd), ..
        } => daemon_control(cmd, ctx).map(Some),
        Command::Ls(args) => ls(ctx, args).await.map(Some),
        Command::Status { target: name } => status(ctx, name.as_deref()).await.map(Some),
        Command::On(args) => actions(ctx, &args.targets, &args.wait, Action::Start, args.open)
            .await
            .map(Some),
        Command::Off(args) if args.all => stop_all(ctx, args).await.map(Some),
        Command::Off(args) => actions(ctx, &args.targets, &args.wait, Action::Stop, false)
            .await
            .map(Some),
        Command::Restart(args) => actions(ctx, &args.targets, &args.wait, Action::Restart, false)
            .await
            .map(Some),
        Command::Run(args) => {
            let (name, branch) = args
                .target
                .split_once('@')
                .filter(|(p, b)| !p.is_empty() && !b.is_empty() && !b.contains('@'))
                .ok_or_else(|| Failure::usage("run target must be project@branch"))?;
            let client = ctx.client()?;
            let project = project(ctx, &client, Some(name)).await?;
            let request = client.run_branch(
                &project,
                branch_target(branch, &args.branch),
                RunOptions {
                    start: !args.no_start,
                    ..RunOptions::default()
                },
                ctx.wait(&args.wait),
            );
            let result = wait_request(ctx, request).await?;
            let result = finish_operation(ctx, &client, result, &args.wait).await?;
            if args.open && !args.wait.no_wait {
                let view = if let Some(view) = &result.instance {
                    view.clone()
                } else {
                    let id = result.operation.instance.as_ref().ok_or_else(|| {
                        Failure::new(
                            1,
                            "protocol",
                            "created operation did not identify an instance to open",
                        )
                    })?;
                    client.instance(id).await.map_err(|e| ctx.failure(e))?
                };
                launch("xdg-open", &view.url)?;
            }
            Ok(Some(operation_reply(&result, args.wait.no_wait)?))
        }
        Command::Setup(args) => {
            let client = ctx.client()?;
            let id = resolve(ctx, &client, Some(&args.target)).await?;
            let request = client.setup(&id, args.force, ctx.wait(&args.wait));
            let result = wait_request(ctx, request).await?;
            let result = finish_operation(ctx, &client, result, &args.wait).await?;
            Ok(Some(operation_reply(&result, args.wait.no_wait)?))
        }
        Command::Logs(args) => logs(ctx, args).await,
        Command::Events { logs } => events(ctx, logs.as_deref()).await,
        Command::Open(args) => open_instance(ctx, args).await.map(Some),
        Command::Wt { command } => worktree(ctx, command).await.map(Some),
        Command::Adopt { target: name, port } => {
            let client = ctx.client()?;
            let (id, port) = if let Ok(number) = name.parse::<u16>() {
                let listeners = client
                    .listeners_filtered(ListenerFilter {
                        port: Some(number),
                        ..ListenerFilter::default()
                    })
                    .await
                    .map_err(|e| ctx.failure(e))?;
                let ids: Vec<_> = listeners.iter().filter_map(|l| l.matched.clone()).collect();
                let id = ids
                    .first()
                    .ok_or_else(|| Failure::not_found("no project here; run `lantern add <cwd>`"))?;
                if ids.iter().any(|i| i != id) {
                    return Err(Failure::not_found(
                        "port matches multiple instances; specify a target",
                    ));
                }
                (id.clone(), Some(number))
            } else {
                (resolve(ctx, &client, Some(name)).await?, *port)
            };
            let view = client.adopt(&id, port).await.map_err(|e| ctx.failure(e))?;
            Ok(Some(Reply::value(
                &view,
                output::instances(std::slice::from_ref(&view), &[], ctx.color),
            )?))
        }
        Command::Unadopt { target: name } => {
            let client = ctx.client()?;
            let id = resolve(ctx, &client, Some(name)).await?;
            let view = client.unadopt(&id).await.map_err(|e| ctx.failure(e))?;
            Ok(Some(Reply::value(
                &view,
                output::instances(std::slice::from_ref(&view), &[], ctx.color),
            )?))
        }
        Command::Scan { paths } => {
            let client = ctx.client()?;
            let report = client
                .scan((!paths.is_empty()).then(|| paths.clone()))
                .await
                .map_err(|e| ctx.failure(e))?;
            let text = format!(
                "{} registered; {} already known; {} skipped\n{}",
                report.registered.len(),
                report.already_known,
                report.skipped.len(),
                output::projects(&report.registered)
            );
            Ok(Some(Reply::value(&report, text)?))
        }
        Command::Add { path, write_manifest } => {
            let client = ctx.client()?;
            let result = client
                .register_project(RegisterProjectRequest {
                    path: path.clone().unwrap_or_else(|| ctx.cwd.clone()),
                    id: None,
                    name: None,
                    enabled: None,
                    overrides: None,
                    write_manifest: *write_manifest,
                })
                .await
                .map_err(|e| ctx.failure(e))?;
            Ok(Some(Reply::value(
                &result,
                output::projects(std::slice::from_ref(&result)),
            )?))
        }
        Command::Project { command } => project_cmd(ctx, command).await.map(Some),
        Command::Manifest {
            command: ManifestCommand::Show { target: name },
        } => {
            let client = ctx.client()?;
            let id = resolve(ctx, &client, name.as_deref()).await?;
            let manifest = client.manifest(&id).await.map_err(|e| ctx.failure(e))?;
            Ok(Some(Reply::pretty(&manifest)?))
        }
        Command::Services { command } => services(ctx, command.as_ref()).await.map(Some),
        Command::Config { command } => config(ctx, command.as_ref()).await.map(Some),
        Command::Gui(_) | Command::Daemon { command: None, .. } => {
            unreachable!("process action is dispatched earlier")
        }
    }
}

async fn auxiliary_request(
    ctx: &Context<'_>,
    method: reqwest::Method,
    path: &[&str],
    body: Option<Value>,
) -> Result<Value> {
    let mut url = reqwest::Url::parse(&ctx.host).map_err(|e| Failure::new(1, "protocol", e.to_string()))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Failure::new(1, "protocol", "invalid daemon URL"))?;
        segments.pop_if_empty();
        for part in path {
            segments.push(part);
        }
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| Failure::new(1, "protocol", e.to_string()))?;
    let mut request = client.request(method.clone(), url);
    if method == reqwest::Method::POST {
        request = request.json(&body.unwrap_or_else(|| json!({})));
    }
    let response = request.send().await.map_err(|e| {
        if e.is_connect() {
            Failure::new(3, "daemon_unreachable", e.to_string())
        } else if e.is_timeout() {
            Failure::new(6, "timeout", e.to_string())
        } else {
            Failure::new(1, "protocol", e.to_string())
        }
    })?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|e| Failure::new(1, "protocol", e.to_string()))?;
    if !status.is_success() {
        let error: api::ApiError = serde_json::from_value(value.get("error").cloned().unwrap_or(Value::Null))
            .map_err(|e| Failure::new(1, "protocol", e.to_string()))?;
        return Err(Failure {
            code: crate::error::code_for_api(status.as_u16(), &error.code),
            error,
        });
    }
    Ok(value)
}

fn run_system(system: &dyn SystemActions, command: SystemCommand) -> Result<Reply> {
    let value = system.execute(&command).map_err(Failure::unsupported)?;
    let checks = value
        .as_array()
        .or_else(|| value.get("checks").and_then(Value::as_array));
    let failed = matches!(command, SystemCommand::Doctor { .. })
        && checks.is_some_and(|checks| {
            checks
                .iter()
                .any(|check| check.get("status").and_then(Value::as_str) == Some("fail"))
        });
    let setup_failed = matches!(
        command,
        SystemCommand::Init { .. } | SystemCommand::InitSystem { .. }
    ) && ["user_steps", "system_steps"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_array))
        .flatten()
        .any(|step| {
            step.get("state")
                .and_then(Value::as_str)
                .is_some_and(|state| state.starts_with("Failed"))
        });
    let mut result = Reply::pretty(&value)?;
    if failed {
        result.code = 8;
    } else if setup_failed {
        result.code = 1;
    }
    Ok(result)
}
fn manifest_check(path: Option<&Path>, cwd: &Path) -> Result<Reply> {
    let path = path.unwrap_or(cwd);
    let parsed = if path.is_dir() {
        lantern_core::manifest::load(path)?
            .ok_or_else(|| Failure::not_found(format!("no lantern.yaml in {}", path.display())))?
    } else {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut text = String::new();
        std::io::Read::by_ref(&mut file)
            .take((lantern_core::yaml::MAX_BYTES + 1) as u64)
            .read_to_string(&mut text)?;
        if text.len() > lantern_core::yaml::MAX_BYTES {
            return Err(Failure::new(
                8,
                "manifest_too_large",
                "manifest exceeds the maximum size",
            ));
        }
        lantern_core::manifest::parse_manifest(&text, path)
    };
    let mut text = String::new();
    for diag in &parsed.diagnostics {
        text.push_str(&format!("{diag}\n"));
    }
    if parsed.diagnostics.is_empty() {
        text.push_str(&format!("{}: OK\n", parsed.file.display()));
    }
    let mut result = Reply::value(&parsed, text)?;
    if !parsed.usable {
        result.code = 8;
    }
    Ok(result)
}
fn daemon_control(cmd: &DaemonCommand, ctx: &Context<'_>) -> Result<Reply> {
    let (program, args): (&str, Vec<&str>) = match cmd {
        DaemonCommand::Start => ("systemctl", vec!["--user", "start", "lantern"]),
        DaemonCommand::Stop => ("systemctl", vec!["--user", "stop", "lantern"]),
        DaemonCommand::Restart => ("systemctl", vec!["--user", "restart", "lantern"]),
        DaemonCommand::Status => ("systemctl", vec!["--user", "status", "lantern"]),
        DaemonCommand::Enable => ("systemctl", vec!["--user", "enable", "lantern"]),
        DaemonCommand::Disable => ("systemctl", vec!["--user", "disable", "lantern"]),
        DaemonCommand::Logs { follow } => {
            let mut args = vec!["--user", "-u", "lantern", "--no-pager"];
            if *follow {
                args.push("-f");
            }
            ("journalctl", args)
        }
        DaemonCommand::Run => return Err(Failure::usage("daemon run must be dispatched by the binary")),
    };
    // Capture output so --json remains one document and non-follow commands remain testable.
    if matches!(cmd, DaemonCommand::Logs { follow: true }) && !ctx.cli.json {
        let status = std::process::Command::new(program).args(&args).status()?;
        return Ok(Reply {
            json: json!({"exit_code": status.code().unwrap_or(1)}),
            text: String::new(),
            code: status.code().unwrap_or(1),
        });
    }
    let out = std::process::Command::new(program).args(&args).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let code = out.status.code().unwrap_or(1);
    let hint = if matches!(cmd, DaemonCommand::Stop) && code == 0 {
        "Dev servers continue running.\n"
    } else {
        ""
    };
    Ok(Reply {
        json: json!({"exit_code":code,"stdout":stdout,"stderr":stderr}),
        text: format!("{stdout}{stderr}{hint}"),
        code,
    })
}
async fn ls(ctx: &Context<'_>, args: &LsArgs) -> Result<Reply> {
    let client = ctx.client()?;
    let projects = client
        .projects(ProjectFilter {
            enabled: if args.all {
                Some("any".into())
            } else {
                Some("true".into())
            },
            ..ProjectFilter::default()
        })
        .await
        .map_err(|e| ctx.failure(e))?;
    let mut instances = list_all(ctx, &client).await?;
    let selected = args
        .project
        .as_deref()
        .map(|p| target::project(&instances, Some(p), &ctx.cwd))
        .transpose()?;
    instances.retain(|i| {
        (args.all || i.visible)
            && (args.all
                || projects
                    .iter()
                    .any(|p| p.project.id == i.project && p.project.enabled))
            && selected.as_ref().is_none_or(|p| &i.project == p)
            && (!args.running
                || matches!(
                    i.runtime.status,
                    InstanceStatus::Running
                        | InstanceStatus::Starting
                        | InstanceStatus::Preparing
                        | InstanceStatus::Unhealthy
                        | InstanceStatus::Crashed
                        | InstanceStatus::External
                ))
    });
    instances.sort_by(|a, b| a.project.cmp(&b.project).then(a.id.cmp(&b.id)));
    let listeners = if args.no_listeners {
        vec![]
    } else {
        client
            .listeners()
            .await
            .map_err(|e| ctx.failure(e))?
            .into_iter()
            .filter(|l| !l.managed && selected.as_ref().is_none_or(|p| l.project.as_ref() == Some(p)))
            .collect()
    };
    let text = output::instances(&instances, &listeners, ctx.color);
    Reply::value(&json!({"instances":instances,"listeners":listeners}), text)
}
async fn status(ctx: &Context<'_>, name: Option<&str>) -> Result<Reply> {
    let client = ctx.client()?;
    if let Some(name) = name {
        let id = resolve(ctx, &client, Some(name)).await?;
        let view = client.instance(&id).await.map_err(|e| ctx.failure(e))?;
        let mut text = output::instances(std::slice::from_ref(&view), &[], ctx.color);
        if view.runtime.status != InstanceStatus::Running {
            if let Some(issue) = &view.runtime.issue {
                text.push_str(&format!("Why: {}\n", output::clean(&issue.detail, false)));
                if let Some(hint) = &issue.hint {
                    text.push_str(&format!("  {}\n", output::clean(hint, false)));
                }
            }
            if let Some(exit) = &view.runtime.last_exit {
                text.push_str(&format!("Last exit: {:?} code {:?}\n", exit.kind, exit.code));
            }
            if let Some(retry) = &view.runtime.next_retry_at {
                text.push_str(&format!("Retry at: {retry}\n"));
            }
        }
        Reply::value(&view, text)
    } else {
        let (system, health, instances, services) = tokio::join!(
            client.system(),
            client.health(),
            client.instances(InstanceFilter::default()),
            client.services()
        );
        let system = system.map_err(|e| ctx.failure(e))?;
        let health = health.map_err(|e| ctx.failure(e))?;
        let instances = instances.map_err(|e| ctx.failure(e))?;
        let services = services.map_err(|e| ctx.failure(e))?;
        let running = instances
            .iter()
            .filter(|i| i.runtime.status == InstanceStatus::Running)
            .count();
        let text = format!(
            "Daemon {} (pid {})\nSystem health: {}\nInstances: {running} running / {} total\nServices: {}\n",
            system.version,
            system.pid,
            if health.ok { "ok" } else { "problems found" },
            instances.len(),
            services.len()
        );
        Reply::value(
            &json!({"system":system,"health":health,"instances":instances,"services":services}),
            text,
        )
    }
}
#[derive(Clone, Copy)]
enum Action {
    Start,
    Stop,
    Restart,
}
// Each tracked operation has its own SSE subscription and authoritative REST wait.
async fn track_operation(
    client: &Client,
    mut result: OperationResponse,
    args: &WaitArgs,
    tty: bool,
    host: &str,
    progress: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<OperationResponse> {
    if tty
        && !args.no_wait
        && matches!(
            result.operation.status,
            OperationStatus::Queued | OperationStatus::Running
        )
    {
        let id = result.operation.id.clone();
        let mut events = client.events_once(EventsOptions {
            state: false,
            logs: vec![],
            tail: 0,
            op: Some(id.clone()),
            reconnect: false,
        });
        let waiting = client.operation(&id, Some(Duration::from_secs(args.timeout)));
        tokio::pin!(waiting);
        loop {
            tokio::select! {
                operation = &mut waiting => { result.operation = operation.map_err(|e| Failure::from_client(e, host))?; break; }
                event = events.next() => match event {
                    Some(StreamEvent::Event(WireEvent::OperationProgress { instance, step, .. })) => { let _ = progress.send(format!("◐ {instance} {}", output::clean(&step, false))); }
                    None | Some(StreamEvent::Lost { .. }) => { result.operation = waiting.await.map_err(|e| Failure::from_client(e, host))?; break; }
                    _ => {}
                }
            }
        }
    }
    check_operation(&result, !args.no_wait)?;
    Ok(result)
}

async fn gather_operations<F>(
    ctx: &mut Context<'_>,
    mut tasks: FuturesUnordered<F>,
    mut progress: tokio::sync::mpsc::UnboundedReceiver<String>,
    count: usize,
) -> Result<Vec<OperationResponse>>
where
    F: std::future::Future<Output = (usize, Result<OperationResponse>)>,
{
    let mut results = vec![None; count];
    let mut worst: Option<Failure> = None;
    while !tasks.is_empty() {
        tokio::select! {
            result = tasks.next() => if let Some((index, result)) = result {
                match result {
                    Ok(result) => results[index] = Some(result),
                    Err(error) => if worst.as_ref().is_none_or(|current| current.code < error.code) { worst = Some(error); },
                }
            },
            line = progress.recv() => if let Some(line) = line { writeln!(ctx.err, "{line}")?; },
            signal = tokio::signal::ctrl_c() => {
                signal?;
                if ctx.tty && !ctx.cli.json { writeln!(ctx.err, "Stopped waiting; operations continue in the daemon.")?; }
                return Err(Failure::new(1, "interrupted", "stopped waiting; operations continue in the daemon"));
            }
        }
    }
    while let Ok(line) = progress.try_recv() {
        writeln!(ctx.err, "{line}")?;
    }
    if let Some(error) = worst {
        return Err(error);
    }
    results
        .into_iter()
        .map(|result| result.ok_or_else(|| Failure::new(1, "internal", "operation result missing")))
        .collect()
}

async fn actions(
    ctx: &mut Context<'_>,
    names: &[String],
    args: &WaitArgs,
    action: Action,
    should_open: bool,
) -> Result<Reply> {
    let client = ctx.client()?;
    let instances = list_all(ctx, &client).await?;
    let ids: Vec<_> = names
        .iter()
        .map(|n| target::resolve(&instances, Some(n), &ctx.cwd))
        .collect::<Result<_>>()?;
    let wait = ctx.wait(args);
    let tty = ctx.tty && !ctx.cli.json;
    let host = ctx.host.clone();
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    let tasks: FuturesUnordered<_> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let client = client.clone();
            let id = id.clone();
            let args = args.clone();
            let progress = send.clone();
            let host = host.clone();
            async move {
                let result = match action {
                    Action::Start => client.start(&id, wait).await,
                    Action::Stop => client.stop(&id, wait).await,
                    Action::Restart => client.restart(&id, wait).await,
                };
                let result = match result {
                    Ok(response) => track_operation(&client, response, &args, tty, &host, progress).await,
                    Err(error) => Err(Failure::from_client(error, &host)),
                };
                (index, result)
            }
        })
        .collect();
    drop(send);
    let completed = gather_operations(ctx, tasks, receive, ids.len()).await?;
    if should_open && !args.no_wait {
        for (result, id) in completed.iter().zip(&ids) {
            let view = if let Some(view) = &result.instance {
                view.clone()
            } else {
                client.instance(id).await.map_err(|e| ctx.failure(e))?
            };
            launch("xdg-open", &view.url)?;
        }
    }
    if let [one] = completed.as_slice() {
        return operation_reply(one, args.no_wait);
    }
    let text = completed
        .iter()
        .map(|r| format!("{}\n", r.operation.id))
        .collect();
    Reply::value(&completed, text)
}

async fn stop_all(ctx: &mut Context<'_>, args: &OffArgs) -> Result<Reply> {
    if !args.yes {
        if !ctx.tty || ctx.cli.json {
            return Err(Failure::usage(
                "off --all requires -y on a non-interactive terminal",
            ));
        }
        if !confirm(ctx, "Stop all Lantern instances?")? {
            return Err(Failure::usage("cancelled"));
        }
    }
    let client = ctx.client()?;
    let request = client.stop_all(ctx.wait(&args.wait));
    let operations = wait_request(ctx, request).await?;
    let count = operations.len();
    let tty = ctx.tty && !ctx.cli.json;
    let host = ctx.host.clone();
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    let tasks: FuturesUnordered<_> = operations
        .into_iter()
        .enumerate()
        .map(|(index, operation)| {
            let client = client.clone();
            let args = args.wait.clone();
            let host = host.clone();
            let progress = send.clone();
            async move {
                let response = OperationResponse {
                    operation,
                    instance: None,
                    created: None,
                    forgotten: None,
                };
                (
                    index,
                    track_operation(&client, response, &args, tty, &host, progress).await,
                )
            }
        })
        .collect();
    drop(send);
    let operations = gather_operations(ctx, tasks, receive, count)
        .await?
        .into_iter()
        .map(|response| response.operation)
        .collect::<Vec<_>>();
    let text = operations
        .iter()
        .map(|o| format!("{}  {:?}\n", o.id, o.status))
        .collect();
    Reply::value(&api::OperationsResponse { operations }, text)
}

fn confirm(ctx: &mut Context<'_>, prompt: &str) -> Result<bool> {
    write!(ctx.err, "{prompt} [y/N] ")?;
    ctx.err.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

async fn worktree(ctx: &mut Context<'_>, command: &WorktreeCommand) -> Result<Reply> {
    let client = ctx.client()?;
    match command {
        WorktreeCommand::Ls { project: name } => {
            let p = project(ctx, &client, name.as_deref()).await?;
            let views = client.checkouts(&p).await.map_err(|e| ctx.failure(e))?;
            Reply::value(&views, output::instances(&views, &[], ctx.color))
        }
        WorktreeCommand::Branches { project: name } => {
            let p = project(ctx, &client, name.as_deref()).await?;
            let branches = client.branches(&p).await.map_err(|e| ctx.failure(e))?;
            Reply::value(&branches, output::branches(&branches))
        }
        WorktreeCommand::Add {
            project: name,
            branch,
            options,
            update,
            wait,
        } => {
            if *update {
                return Err(Failure::unsupported("PR update is not assembled yet"));
            }
            let p = project(ctx, &client, Some(name)).await?;
            let request = client.run_branch(
                &p,
                branch_target(branch, options),
                RunOptions {
                    start: false,
                    ..RunOptions::default()
                },
                ctx.wait(wait),
            );
            let result = wait_request(ctx, request).await?;
            let result = finish_operation(ctx, &client, result, wait).await?;
            operation_reply(&result, wait.no_wait)
        }
        WorktreeCommand::Pin { target: name } | WorktreeCommand::Unpin { target: name } => {
            let id = resolve(ctx, &client, Some(name)).await?;
            let view = client
                .pin(&id, matches!(command, WorktreeCommand::Pin { .. }))
                .await
                .map_err(|e| ctx.failure(e))?;
            Reply::value(
                &view,
                output::instances(std::slice::from_ref(&view), &[], ctx.color),
            )
        }
        WorktreeCommand::Relabel { target: name, slug } => {
            let id = resolve(ctx, &client, Some(name)).await?;
            let view = client
                .rename_instance(&id, slug.clone())
                .await
                .map_err(|e| ctx.failure(e))?;
            Reply::value(
                &view,
                output::instances(std::slice::from_ref(&view), &[], ctx.color),
            )
        }
        WorktreeCommand::Gc {
            project: name,
            apply,
            fetch,
        } => {
            let p = project(ctx, &client, name.as_deref()).await?;
            let value = auxiliary_request(
                ctx,
                reqwest::Method::POST,
                &["api", "projects", p.as_str(), "gc"],
                Some(json!({"apply":apply, "fetch":fetch})),
            )
            .await?;
            Reply::pretty(&value)
        }
        WorktreeCommand::Rm {
            target: name,
            force,
            delete_branch,
            yes,
        } => {
            let all = list_all(ctx, &client).await?;
            let id = target::resolve(&all, Some(name), &ctx.cwd)?;
            let view = all
                .iter()
                .find(|i| i.id == id)
                .ok_or_else(|| Failure::not_found("instance disappeared"))?;
            let check = client.removal_check(&id).await.map_err(|e| ctx.failure(e))?;
            if !check.blockers.is_empty() && !ctx.cli.json {
                writeln!(ctx.err, "Removal blockers: {:?}", check.blockers)?;
            }
            if view.checkout.kind == CheckoutKind::Primary {
                return Err(Failure::new(
                    7,
                    "cannot_remove_primary",
                    "cannot remove the primary checkout",
                ));
            }
            // User-created worktrees are forgotten by the server, never deleted.
            let refused = check.blockers.iter().any(|b| match b {
                api::Blocker::NotManaged => view.checkout.kind == CheckoutKind::Managed,
                api::Blocker::Locked(_) => true,
                api::Blocker::Running => false,
                api::Blocker::Dirty(_) | api::Blocker::Unpushed(_) | api::Blocker::Orphaned(_) => {
                    !*force && view.checkout.kind == CheckoutKind::Managed
                }
            });
            if refused {
                return Err(Failure::new(
                    7,
                    "removal_refused",
                    "checkout has blockers; resolve them or use --force for dirty or unpushed work",
                ));
            }
            if !*yes {
                if !ctx.tty || ctx.cli.json {
                    return Err(Failure::usage("wt rm requires -y on a non-interactive terminal"));
                }
                if !confirm(ctx, &format!("Remove or forget {id}?"))? {
                    return Err(Failure::usage("cancelled"));
                }
            }
            let result = client
                .remove_instance(
                    &id,
                    RemoveInstanceParams {
                        force: *force,
                        delete_branch: *delete_branch,
                        stop: true,
                    },
                    Wait {
                        wait: true,
                        timeout: Some(Duration::from_secs(120)),
                    },
                )
                .await
                .map_err(|e| ctx.failure(e))?;
            check_operation(&result, true)?;
            operation_reply(&result, false)
        }
    }
}
async fn project_cmd(ctx: &Context<'_>, command: &ProjectCommand) -> Result<Reply> {
    let client = ctx.client()?;
    match command {
        ProjectCommand::Rm {
            id,
            keep_running,
            remove_worktrees,
        } => {
            let id = parse_project(id)?;
            client
                .unregister_project(
                    &id,
                    UnregisterParams {
                        stop: !keep_running,
                        remove_worktrees: *remove_worktrees,
                    },
                )
                .await
                .map_err(|e| ctx.failure(e))?;
            Reply::value(&json!({"id":id,"removed":true}), format!("Unregistered {id}\n"))
        }
        ProjectCommand::Enable { id } | ProjectCommand::Disable { id } => {
            let id = parse_project(id)?;
            let enabled = matches!(command, ProjectCommand::Enable { .. });
            let detail = client
                .patch_project(
                    &id,
                    PatchProjectRequest {
                        id: None,
                        name: None,
                        enabled: Some(enabled),
                        tags: None,
                        overrides: None,
                    },
                )
                .await
                .map_err(|e| ctx.failure(e))?;
            Reply::pretty(&detail)
        }
        ProjectCommand::Show { id } => {
            let detail = client
                .project(&parse_project(id)?)
                .await
                .map_err(|e| ctx.failure(e))?;
            Reply::pretty(&detail)
        }
    }
}
async fn services(ctx: &Context<'_>, cmd: Option<&ServiceCommand>) -> Result<Reply> {
    let client = ctx.client()?;
    match cmd {
        None => {
            let services = client.services().await.map_err(|e| ctx.failure(e))?;
            Reply::value(&services, output::services(&services))
        }
        Some(ServiceCommand::Open { name }) => {
            let service = client.service_status(name).await.map_err(|e| ctx.failure(e))?;
            let url = service
                .ui_url
                .as_deref()
                .ok_or_else(|| Failure::not_found("service has no UI URL"))?;
            launch("xdg-open", url)?;
            Reply::value(&service, format!("{url}\n"))
        }
        Some(
            ServiceCommand::Start { name } | ServiceCommand::Stop { name } | ServiceCommand::Restart { name },
        ) => {
            let action = match cmd {
                Some(ServiceCommand::Start { .. }) => ServiceAction::Start,
                Some(ServiceCommand::Stop { .. }) => ServiceAction::Stop,
                _ => ServiceAction::Restart,
            };
            let service = client.service(name, action).await.map_err(|e| ctx.failure(e))?;
            Reply::pretty(&service)
        }
    }
}
async fn config(ctx: &Context<'_>, cmd: Option<&ConfigCommand>) -> Result<Reply> {
    let client = ctx.client()?;
    match cmd {
        None => {
            let settings = client.settings().await.map_err(|e| ctx.failure(e))?;
            Reply::pretty(&settings)
        }
        Some(ConfigCommand::Get { key }) => {
            let settings = client.settings().await.map_err(|e| ctx.failure(e))?;
            let value = key
                .split('.')
                .try_fold(&settings, |v, key| v.get(key))
                .ok_or_else(|| Failure::not_found(format!("setting {key:?} not found")))?;
            Reply::pretty(value)
        }
        Some(ConfigCommand::Set { key, value }) => {
            let mut value: Value =
                serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.clone()));
            let parts: Vec<_> = key.split('.').collect();
            if parts.iter().any(|s| s.is_empty()) {
                return Err(Failure::usage("setting key must be a dotted path"));
            }
            for part in parts.into_iter().rev() {
                let mut map = serde_json::Map::new();
                map.insert(part.into(), value);
                value = Value::Object(map);
            }
            let result = client.patch_settings(value).await.map_err(|e| ctx.failure(e))?;
            Reply::pretty(&result)
        }
        Some(ConfigCommand::Reload) => {
            let result = client.reload_settings().await.map_err(|e| ctx.failure(e))?;
            Reply::pretty(&result)
        }
        Some(ConfigCommand::Path) => unreachable!("config path is handled offline"),
    }
}
fn render_command(template: &str, path: &str) -> Result<Vec<String>> {
    let mut words = shlex::split(template)
        .ok_or_else(|| Failure::usage("invalid editor or terminal command template"))?;
    if words.is_empty() {
        return Err(Failure::usage("empty editor or terminal command"));
    }
    let has_path = words.iter().any(|word| word.contains("{path}"));
    for word in &mut words {
        *word = word.replace("{path}", path);
    }
    if !has_path {
        words.push(path.to_string());
    }
    Ok(words)
}
fn spawn_command(words: &[String]) -> std::io::Result<()> {
    let mut command = std::process::Command::new(&words[0]);
    command.args(&words[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn()?;
    Ok(())
}
fn launch(program: &str, arg: &str) -> Result<()> {
    spawn_command(&[program.to_owned(), arg.to_owned()])
        .map_err(|e| Failure::new(1, "open_failed", e.to_string()))
}
fn launch_client(setting: &str, template: &str, path: &str) -> Result<()> {
    if template != "auto" {
        let words = render_command(template, path)?;
        return spawn_command(&words)
            .map_err(|e| Failure::new(1, "open_failed", format!("{}: {e}", words[0])));
    }
    let candidates: Vec<String> = if setting == "editor" {
        [
            "code {path}",
            "codium {path}",
            "zed {path}",
            "kate {path}",
            "xdg-open {path}",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    } else {
        std::env::var("TERMINAL")
            .ok()
            .into_iter()
            .chain(
                [
                    "konsole --workdir {path}",
                    "gnome-terminal --working-directory={path}",
                    "kitty --directory {path}",
                    "foot --working-directory={path}",
                ]
                .into_iter()
                .map(str::to_string),
            )
            .collect()
    };
    let mut errors = Vec::new();
    for candidate in candidates {
        let words = render_command(&candidate, path)?;
        match spawn_command(&words) {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(format!("{}: {error}", words[0])),
        }
    }
    Err(Failure::new(
        1,
        "open_failed",
        format!(
            "no {setting} could be opened ({}); configure clients.{setting}",
            errors.join(", ")
        ),
    ))
}

async fn open_instance(ctx: &mut Context<'_>, args: &OpenArgs) -> Result<Reply> {
    let client = ctx.client()?;
    let id = resolve(ctx, &client, args.target.as_deref()).await?;
    let mut view = client.instance(&id).await.map_err(|e| ctx.failure(e))?;
    if !args.editor
        && !args.terminal
        && !args.no_start
        && !args.print
        && view.runtime.status == InstanceStatus::Stopped
    {
        let wait = WaitArgs {
            wait: true,
            no_wait: false,
            timeout: 120,
        };
        let result = client
            .start(&id, ctx.wait(&wait))
            .await
            .map_err(|e| ctx.failure(e))?;
        let result = finish_operation(ctx, &client, result, &wait).await?;
        if let Some(updated) = result.instance {
            view = updated;
        }
    }
    let destination = if args.editor || args.terminal {
        view.checkout.path.to_string_lossy().into_owned()
    } else {
        view.url.clone()
    };
    if !args.print {
        let setting = if args.editor {
            Some("editor")
        } else if args.terminal {
            Some("terminal")
        } else {
            None
        };
        if let Some(setting) = setting {
            let settings = client.settings().await.map_err(|e| ctx.failure(e))?;
            let program = settings
                .get("clients")
                .and_then(|v| v.get(setting))
                .and_then(Value::as_str)
                .unwrap_or("auto");
            launch_client(setting, program, &destination)?;
        } else {
            launch("xdg-open", &destination)?;
        }
    }
    Reply::value(&destination, format!("{destination}\n"))
}

fn log_filter(line: &lantern_core::model::LogLine, filter: Option<LogStream>) -> bool {
    match filter {
        None => true,
        Some(LogStream::Stdout) => line.stream == Stream::Stdout,
        Some(LogStream::Stderr) => line.stream == Stream::Stderr,
    }
}
async fn logs(ctx: &mut Context<'_>, args: &LogsArgs) -> Result<Option<Reply>> {
    let client = ctx.client()?;
    let id = resolve(ctx, &client, args.target.as_deref()).await?;
    if !args.follow {
        let stream = args.stream.map(|s| match s {
            LogStream::Stdout => Stream::Stdout,
            LogStream::Stderr => Stream::Stderr,
        });
        let page = client
            .logs(
                &id,
                LogParams {
                    tail: Some(args.lines),
                    since: None,
                    stream,
                    grep: None,
                    format: None,
                },
            )
            .await
            .map_err(|e| ctx.failure(e))?;
        let text = page
            .lines
            .iter()
            .map(|line| output::log(line, args.timestamps, ctx.tty && !ctx.cli.json))
            .collect();
        return Ok(Some(Reply::value(&page, text)?));
    }
    if args.lines > 1000 {
        return Err(Failure::usage("logs -f supports at most 1000 backlog lines"));
    }
    let mut initial = true;
    let mut ever_connected = false;
    loop {
        let mut events = client.events_once(EventsOptions {
            state: false,
            logs: vec![id.clone()],
            tail: if initial { args.lines as usize } else { 0 },
            op: None,
            reconnect: false,
        });
        let mut failed = false;
        loop {
            let event = tokio::select! {
                signal = tokio::signal::ctrl_c() => { signal?; return Ok(None); }
                event = events.next() => event,
            };
            match event {
                Some(StreamEvent::Connected) => {
                    if ever_connected && ctx.tty && !ctx.cli.json {
                        writeln!(ctx.err, "-- reconnected; lines in between may be missing --")?;
                    }
                    ever_connected = true;
                    initial = false;
                }
                Some(StreamEvent::Event(WireEvent::Log { line })) if log_filter(&line, args.stream) => {
                    if ctx.cli.json {
                        writeln!(
                            ctx.out,
                            "{}",
                            serde_json::to_string(&line).map_err(|e| Failure::new(
                                1,
                                "serialization",
                                e.to_string()
                            ))?
                        )?;
                    } else {
                        write!(ctx.out, "{}", output::log(&line, args.timestamps, ctx.tty))?;
                    }
                    ctx.out.flush()?;
                }
                Some(StreamEvent::Lost {
                    daemon_unreachable: true,
                    ..
                }) if !ever_connected => {
                    return Err(Failure::from_client(
                        lantern_client::ClientError::DaemonUnreachable(String::new()),
                        &ctx.host,
                    ));
                }
                Some(StreamEvent::Lost { error, .. }) => {
                    if !ever_connected {
                        return Err(Failure::new(1, "stream_error", error));
                    }
                    failed = true;
                    break;
                }
                None => break,
                _ => {}
            }
        }
        if failed {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
async fn events(ctx: &mut Context<'_>, logs: Option<&str>) -> Result<Option<Reply>> {
    let client = ctx.client()?;
    let ids = if let Some(name) = logs {
        vec![resolve(ctx, &client, Some(name)).await?]
    } else {
        vec![]
    };
    let mut events = client.events(EventsOptions {
        logs: ids,
        ..EventsOptions::default()
    });
    let mut connected = false;
    loop {
        let event = tokio::select! {
            signal = tokio::signal::ctrl_c() => { signal?; return Ok(None); }
            event = events.next() => event,
        };
        match event {
            Some(StreamEvent::Connected) => connected = true,
            Some(StreamEvent::Event(event)) => {
                writeln!(
                    ctx.out,
                    "{}",
                    serde_json::to_string(&event).map_err(|e| Failure::new(
                        1,
                        "serialization",
                        e.to_string()
                    ))?
                )?;
                ctx.out.flush()?;
            }
            Some(StreamEvent::Lost {
                daemon_unreachable: true,
                ..
            }) if !connected => {
                return Err(Failure::from_client(
                    lantern_client::ClientError::DaemonUnreachable(String::new()),
                    &ctx.host,
                ));
            }
            Some(StreamEvent::Lost { error, .. }) if !connected => {
                return Err(Failure::new(1, "stream_error", error));
            }
            Some(StreamEvent::Lost { error, .. }) if ctx.tty && !ctx.cli.json => {
                writeln!(ctx.err, "connection lost: {}", output::clean(&error, false))?;
            }
            None => return Ok(None),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::render_command;
    #[test]
    fn editor_templates_split_before_substitution_without_shell_execution() {
        assert_eq!(
            render_command("editor --folder '{path}'", "/tmp/a b;$(id)").unwrap(),
            ["editor", "--folder", "/tmp/a b;$(id)"]
        );
        assert_eq!(
            render_command("terminal --dir={path}", "/tmp/a b").unwrap(),
            ["terminal", "--dir=/tmp/a b"]
        );
        assert!(render_command("editor 'unclosed", "/tmp").is_err());
    }
}

async fn mcp_install(
    ctx: &Context<'_>,
    target: McpTarget,
    scope: McpScope,
    force: bool,
    print: bool,
    config_path: Option<&Path>,
) -> Result<Reply> {
    let host = if ctx.cli.host.is_some() || std::env::var_os("LANTERN_DAEMON_URL").is_some() {
        ctx.host.clone()
    } else {
        let dirs = Dirs::from_env(&ProcessEnv).map_err(|e| Failure::new(1, "environment", e))?;
        let home = PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| Failure::new(1, "environment", "HOME is required"))?,
        );
        let settings =
            lantern_core::settings::load(&dirs.config.join("config.toml"), &ProcessEnv, &home, None)
                .map_err(|e| Failure::new(1, "settings", e.to_string()))?
                .settings;
        format!("http://127.0.0.1:{}", settings.daemon.port)
    };
    let parsed = reqwest::Url::parse(&host).map_err(|e| Failure::usage(e.to_string()))?;
    if parsed.scheme() != "http"
        || !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"))
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(Failure::usage(
            "MCP installation requires an HTTP loopback daemon URL",
        ));
    }
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| Failure::usage("daemon port missing"))?;
    let url = format!("http://127.0.0.1:{port}/mcp");
    if print || matches!(target, McpTarget::Json) {
        let variants = [
            ("Generic JSON", McpTarget::Json),
            ("Claude Code", McpTarget::ClaudeCode),
            ("Codex", McpTarget::Codex),
            ("Cursor", McpTarget::Cursor),
        ];
        let text = variants
            .into_iter()
            .map(|(name, client)| format!("{name}:\n{}", crate::mcp_install::snippet(client, &url)))
            .collect::<Vec<_>>()
            .join("\n");
        return Reply::value(&json!({"url":url, "configuration":text}), text);
    }
    let home = PathBuf::from(
        std::env::var_os("HOME").ok_or_else(|| Failure::new(1, "environment", "HOME is required"))?,
    );
    let message = if matches!(target, McpTarget::ClaudeCode) {
        if matches!(scope, McpScope::User) && config_path.is_some() {
            return Err(Failure::usage(
                "Claude user configuration is managed by the Claude CLI; --config-path is not supported",
            ));
        }
        crate::mcp_install::install_claude(scope, &url, force, &ctx.cwd, config_path)
            .map_err(|e| Failure::new(1, "mcp_install", e))?
    } else {
        let path = config_path
            .map(Path::to_path_buf)
            .or_else(|| crate::mcp_install::config_path(target, scope, &home, &ctx.cwd))
            .ok_or_else(|| Failure::usage("no client configuration path"))?;
        let changed = crate::mcp_install::install_file(&path, target, &url, force)
            .map_err(|e| Failure::new(1, "mcp_install", e))?;
        format!(
            "{} {}",
            if changed { "Updated" } else { "Already installed:" },
            path.display()
        )
    };
    let available = match reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build() {
        Ok(client) => client.post(&url).header("accept", "application/json, text/event-stream").json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"lantern-install","version":"1"}}})).send().await.is_ok_and(|r| r.status().is_success()),
        Err(_) => false,
    };
    let text = format!(
        "{message}\n{}\n",
        if available {
            "Lantern MCP initialized successfully."
        } else {
            "Lantern MCP is unavailable; installation was saved."
        }
    );
    Reply::value(
        &json!({"url":url, "message":message, "daemon_available":available}),
        text,
    )
}

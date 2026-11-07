//! Daemon assembly: config, adapters between subsystem seams, `run()` (docs/01 §3, §8). Owner: wave-3 lane `assembly`.

/// Bridges registry, checkout preparation, and runtime interfaces.
pub mod adapters;

/// Demand-driven listener discovery integration.
pub mod discovery;

/// Concrete daemon API facade.
pub mod facade;

/// Gateway integration.
pub mod gateway;

/// Intel refresh worker.
pub mod intel_worker;

/// Read-only shared-service detection.
pub mod services;

/// Network construction and diagnostics.
pub mod network;

use self::{
    adapters::{CheckoutPreparer, RegistryAdapter, RuntimeOneShot},
    facade::AppFacade,
};
use crate::{
    api::{self, ApiConfig, ApiState},
    discovery::{Scanner, hide::HideSettings, procinfo::ProcRoot},
    events::EventBus,
    git::Git,
    instances::InstanceService,
    net::{
        dns::{Dns, DnsConfig},
        sync::NetRouter,
    },
    operations::OperationTracker,
    registry::{RegistryHandle, state_io::Recovery},
    runtime::{
        Runtime, RuntimeAdapters, RuntimeConfig,
        seams::ScannerProbe,
        systemd::{SystemctlBackend, UnitNames},
    },
};
use lantern_core::{
    Timestamp,
    detect::Templates,
    dirs::Dirs,
    settings::{self, Settings},
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Inputs for daemon startup; callers can supply isolated environment, settings, and unit names.
#[derive(Debug, Clone, Default)]
pub struct DaemonArgs {
    /// Use the development port, domain and private directories by default.
    pub dev: bool,
    /// Explicit environment, useful for isolated in-process runs; absent uses the process environment.
    pub environment: Option<BTreeMap<String, String>>,
    /// Explicit effective settings, bypassing config loading when provided.
    pub settings: Option<Settings>,
    /// Optional managed-unit prefixes and slice.
    pub unit_names: Option<UnitNames>,
    /// Restrict journal reads to these named units for isolated shared-slice tests.
    pub restrict_journal_to_units: bool,
}

/// Run the daemon until SIGINT or SIGTERM without stopping managed dev servers.
pub async fn run(args: DaemonArgs) -> Result<(), String> {
    let shutdown = CancellationToken::new();
    let signals = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signals.cancel();
    });
    run_until(args, shutdown).await
}

fn set_dev_defaults(env: &mut BTreeMap<String, String>, home: &std::path::Path) {
    let root = home.join(".local/share/lantern-dev");
    for (name, path) in [
        ("LANTERN_CONFIG_DIR", root.join("config")),
        ("LANTERN_STATE_DIR", root.join("state")),
        ("LANTERN_DATA_DIR", root.clone()),
        ("LANTERN_CACHE_DIR", root.join("cache")),
    ] {
        env.entry(name.into())
            .or_insert_with(|| path.to_string_lossy().into_owned());
    }
    env.entry("LANTERN_DAEMON_PORT".into())
        .or_insert_with(|| "4778".into());
    env.entry("LANTERN_TLD".into()).or_insert_with(|| "glow2".into());
}

/// Run an in-process daemon until the supplied cancellation token is cancelled.
pub async fn run_until(args: DaemonArgs, shutdown: CancellationToken) -> Result<(), String> {
    let _cancel_on_exit = shutdown.clone().drop_guard();
    let mut env: BTreeMap<String, String> = args.environment.unwrap_or_else(|| std::env::vars().collect());
    let home = PathBuf::from(env.get("HOME").ok_or("HOME must be set for daemon startup")?);
    if args.dev {
        set_dev_defaults(&mut env, &home);
    }
    let dirs = Dirs::from_env(&env)?;
    let settings_override = args.settings.is_some();
    let mut settings = match args.settings {
        Some(s) => s,
        None => {
            settings::load(&dirs.config.join("config.toml"), &env, &home, None)
                .map_err(|e| e.to_string())?
                .settings
        }
    };
    if args.dev && !settings_override {
        settings.proxy.http_port = 8080;
        settings.proxy.https_port = 8443;
    }
    let cache = env
        .get("LANTERN_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env.get("XDG_CACHE_HOME")
                .map_or_else(|| home.join(".cache"), PathBuf::from)
                .join(if args.dev { "lantern-dev" } else { "lantern" })
        });
    let runtime_dir = env
        .get("LANTERN_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env.get("XDG_RUNTIME_DIR")
                .map(|p| PathBuf::from(p).join(if args.dev { "lantern-dev" } else { "lantern" }))
        })
        .ok_or("XDG_RUNTIME_DIR or LANTERN_RUNTIME_DIR is required")?;
    let isolated_units = args.unit_names.is_some();
    let names = args.unit_names.unwrap_or_else(|| {
        if args.dev {
            UnitNames {
                instance_prefix: "lantern-dev-inst-".into(),
                operation_prefix: "lantern-dev-op-".into(),
                slice: "app-lantern-dev.slice".into(),
            }
        } else {
            UnitNames::default()
        }
    });
    // Saved commands run as `<prefix>cmd-<id>.service` next to instance units.
    let launcher_prefix = names.instance_prefix.replacen("-inst-", "-cmd-", 1);
    let launcher_slice = names.slice.clone();
    let launcher_home = home.clone();
    if args.dev && settings.dns.listen == "127.0.0.1:5354" {
        settings.dns.listen = "127.0.0.1:5355".into();
    }
    let (templates, diagnostics) =
        Templates::load_dir(&dirs.config.join("templates")).map_err(|e| e.to_string())?;
    for diagnostic in diagnostics {
        tracing::warn!(message=%diagnostic.message, "template diagnostic");
    }
    let bus = EventBus::new();
    let tracker = OperationTracker::new(bus.clone());
    let (registry, recovery) =
        RegistryHandle::open(&dirs, settings.clone(), templates, home.clone(), bus.clone())
            .map_err(|e| e.to_string())?;
    let initial_scan = registry.state_snapshot().projects.is_empty() || recovery != Recovery::None;
    let scanner = Arc::new(ScannerProbe::new(Scanner::new(
        ProcRoot::default(),
        registry.subscribe_checkout_index().borrow().clone(),
        HideSettings {
            self_ports: vec![
                settings.daemon.port,
                settings.proxy.http_port,
                settings.proxy.https_port,
            ],
            ..HideSettings::default()
        },
    )));
    let adapter = Arc::new(
        RegistryAdapter::new(registry.clone(), scanner.clone(), settings.clone())
            .with_profile_dir(dirs.config.join("profiles")),
    );
    let oneshot = Arc::new(RuntimeOneShot::default());
    let git = Git::detect().await.map_err(|e| e.to_string())?;
    let worktrees = if settings.worktrees.dir.is_empty() {
        dirs.data.join("worktrees")
    } else {
        PathBuf::from(
            settings
                .worktrees
                .dir
                .replace("~/", &format!("{}/", home.display())),
        )
    };
    let service = Arc::new(
        InstanceService::new(
            git.clone(),
            adapter.clone(),
            oneshot.clone(),
            worktrees,
            settings.worktrees.max_concurrent_setups as usize,
            Duration::from_secs(settings.worktrees.setup_timeout_secs),
        )
        .with_missing_retention_days(u64::from(settings.worktrees.missing_retention_days)),
    );
    let net_paths = network::paths(dirs.clone(), home.clone(), runtime_dir.clone(), cache.clone())?;
    let php_socket = network::php_socket(&settings.php.fpm_socket);
    let net_health = Arc::new(network::NetworkHealth::default());
    let search_path = env.get("PATH").cloned().unwrap_or_default();
    if network::resolve_caddy(&settings.caddy.binary, &dirs.data, &search_path).is_none() {
        net_health.set_caddy_error(Some("caddy_missing".into()));
    }
    let caddy_unit = if isolated_units {
        format!("{}caddy.service", names.operation_prefix)
    } else if args.dev {
        "lantern-dev-caddy.service".into()
    } else {
        "lantern-caddy.service".into()
    };
    let applier = Arc::new(network::CaddyApplier::new(
        settings.caddy.binary.clone(),
        search_path,
        net_paths.clone(),
        caddy_unit,
        names.slice.clone(),
        settings.domains.tld.clone(),
        network::render(&net_paths, &settings, php_socket.clone()),
        net_health.clone(),
    ));
    let router = Arc::new(NetRouter::new(
        applier.clone(),
        settings.domains.tld.clone(),
        false,
        php_socket,
        shutdown.clone(),
    ));
    router.replace_instances(network::registry_instances(&registry));
    router.set_mailpit(settings.services.mailpit.enabled, None);
    let mut network_events = bus.subscribe();
    let network_router = router.clone();
    let network_registry = registry.clone();
    let network_shutdown = shutdown.clone();
    let network_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        let mut binary_watch = tokio::time::interval(Duration::from_secs(1));
        let mut previous_binary = applier.binary();
        loop {
            tokio::select! {
                () = network_shutdown.cancelled() => break,
                _ = interval.tick() => {
                    tokio::select! { () = network_shutdown.cancelled() => break, () = applier.check_health() => {} }
                    network_router.reconcile();
                },
                _ = binary_watch.tick() => {
                    let current = applier.binary();
                    if current != previous_binary {
                        previous_binary = current;
                        network_router.reconcile();
                    }
                },
                event = network_events.recv() => match event {
                    Ok(lantern_core::api::Event::InstanceUpdated { .. } | lantern_core::api::Event::InstanceRemoved { .. } | lantern_core::api::Event::ProjectUpdated { .. } | lantern_core::api::Event::ProjectRemoved { .. } | lantern_core::api::Event::ListenersChanged { .. }) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => network_router.replace_instances(network::registry_instances(&network_registry)),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        }
    });
    let dns_health = net_health.clone();
    let dns_shutdown = shutdown.clone();
    let dns_config = settings.dns.listen.parse().map(|listen| DnsConfig {
        listen,
        tld: settings.domains.tld.clone(),
        ipv6: settings.proxy.ipv6,
    });
    let dns_task = tokio::spawn(async move {
        let result = match dns_config {
            Ok(config) => Dns::serve(config, dns_shutdown).await.map_err(|e| e.to_string()),
            Err(e) => Err(format!("invalid DNS listen address: {e}")),
        };
        if let Err(error) = result {
            tracing::error!(%error, "DNS responder unavailable");
            dns_health.set_dns_error(error);
        }
    });
    let services = Arc::new(services::ServiceObserver::new(settings.clone(), !isolated_units));
    let runtime = Arc::new(Runtime::with_scoped_journal(
        RuntimeConfig {
            settings: settings.clone(),
            run_dir: runtime_dir.join("run"),
            shell_env_file: cache.join("shell-env.sh"),
            home: home.clone(),
            names: names.clone(),
            poll_interval: Duration::from_secs(2),
            readiness_interval: Duration::from_millis(250),
            mismatch_after: Duration::from_secs(3),
        },
        adapter,
        scanner.clone(),
        Arc::new(SystemctlBackend),
        tracker.clone(),
        RuntimeAdapters {
            preparer: Arc::new(CheckoutPreparer(service.clone())),
            router: router.clone(),
            services: services.clone(),
        },
        args.restrict_journal_to_units,
    ));
    oneshot.bind(&runtime).map_err(str::to_owned)?;
    // Runtime state starts as stopped; restore port hints only, then let systemd decide status.
    for (id, lease) in registry.state_snapshot().port_leases {
        registry
            .update_runtime(&id, move |r| r.port = Some(lease.port))
            .map_err(|e| e.to_string())?;
    }
    let report = runtime.reconcile().await;
    for problem in report.errors {
        tracing::warn!(%problem, "runtime reconciliation");
    }
    let discovery = discovery::Discovery::spawn(
        scanner,
        registry.clone(),
        runtime.clone(),
        names,
        shutdown.clone(),
    );
    let policy = crate::mcp::upstream::UpstreamPolicy {
        tld: settings.domains.tld.clone(),
        allowed_hosts: settings.mcp.allowed_upstream_hosts.clone(),
        infra_ports: vec![
            settings.daemon.port,
            settings
                .dns
                .listen
                .parse::<std::net::SocketAddrV4>()
                .map_or(5354, |addr| addr.port()),
            2019,
            settings.proxy.http_port,
            settings.proxy.https_port,
        ],
    };
    let intel = Arc::new(
        crate::intel::Intel::new(
            dirs.state.join("intel-cache.json"),
            policy.clone(),
            crate::mcp::upstream::default_client(),
        )
        .with_options(settings.intel.cache_file, settings.intel.max_openapi_bytes),
    );
    let boot_id = format!("{}-{}", std::process::id(), Timestamp::now().as_nanosecond());
    let facade = Arc::new(
        AppFacade::new(
            registry.clone(),
            runtime.clone(),
            service.clone(),
            git,
            discovery.clone(),
            tracker.clone(),
            settings.clone(),
            dirs.clone(),
            home,
            boot_id.clone(),
        )
        .with_services(services.clone())
        .with_intel(intel.clone())
        .with_network_health(net_health, isolated_units)
        .with_environment(env.clone())
        .with_system_paths(env.get("XDG_RUNTIME_DIR").map(PathBuf::from), cache.clone()),
    );
    if initial_scan {
        let reg = registry.clone();
        let dirs = dirs.clone();
        tokio::task::spawn_blocking(move || reg.scan_workspace(&dirs))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
    }
    let valid_ids = registry.instances().into_iter().map(|v| v.id).collect::<Vec<_>>();
    if let Err(error) = intel.load_cache(&valid_ids).await {
        tracing::warn!(%error, "intel cache unavailable");
    }
    let intel_task = intel_worker::spawn(facade.clone(), &bus, shutdown.clone());
    if let Some(name) = registry.active_profile() {
        use crate::api::facade::DaemonFacade;
        if let Err(error) = facade.activate_profile(name).await {
            tracing::warn!(message=%error.message, "active profile startup failed");
        }
    }
    let git_shutdown = shutdown.clone();
    let git_registry = registry.clone();
    let git_task = tokio::spawn(async move {
        loop {
            let repos: Vec<_> = git_registry.state_snapshot().repos.into_keys().collect();
            for repo in repos {
                if let Err(error) = service.tick(&repo, false).await {
                    tracing::warn!(%error, path=%repo.display(), "git refresh failed");
                }
            }
            let delay = InstanceService::refresh_interval(!git_registry.listeners().1.is_empty());
            tokio::select! { ()=git_shutdown.cancelled()=>break, ()=tokio::time::sleep(delay)=>{} }
        }
    });
    let services_shutdown = shutdown.clone();
    let services_router = router.clone();
    let services_bus = bus.clone();
    let service_enabled = settings.services.mailpit.enabled;
    let services_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        let mut previous = BTreeMap::new();
        loop {
            tokio::select! {
                () = services_shutdown.cancelled() => break,
                _ = interval.tick() => for name in ["mailpit", "redis", "postgres"] {
                    if let Some(status) = services.status(name).await && previous.get(name) != Some(&status) {
                        if name == "mailpit" {
                            let upstream = (status.state == lantern_core::api::ServiceState::Running).then(|| ([127,0,0,1], status.ports["ui"]).into());
                            services_router.set_mailpit(service_enabled, upstream);
                        }
                        services_bus.publish(lantern_core::api::Event::ServiceUpdated { service: status.clone() });
                        previous.insert(name, status);
                    }
                }
            }
        }
    });
    let mut errors = registry.subscribe_write_errors();
    let error_shutdown = shutdown.clone();
    let health_facade = facade.clone();
    let health_bus = bus.clone();
    let error_task = tokio::spawn(async move {
        use crate::api::facade::DaemonFacade;
        loop {
            tokio::select! {
                () = error_shutdown.cancelled() => break,
                result = errors.changed() => {
                    if result.is_err() { break; }
                    let message = errors.borrow().clone();
                    if let Some(message) = message { tracing::error!(%message, "state persistence failed"); }
                    if let Ok(health) = health_facade.system_health().await { health_bus.publish(lantern_core::api::Event::SystemHealth { health }); }
                }
            }
        }
    });
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, settings.daemon.port))
        .await
        .map_err(|e| e.to_string())?;
    if let Some(address) = env.get("NOTIFY_SOCKET")
        && let Err(error) = notify_systemd(address, b"READY=1")
    {
        tracing::warn!(%error, "systemd readiness notification failed");
    }
    let mut api_config = ApiConfig::new(settings.daemon.port, settings.domains.tld.clone(), boot_id);
    if settings.proxy.https_port != 443 {
        api_config.gateway_port = Some(settings.proxy.https_port);
    }
    let launchers = Arc::new(crate::console::launchers::Launchers::load(
        dirs.config.join("launchers.json"),
        launcher_home,
        launcher_prefix,
        launcher_slice,
    ));
    let console = api::guarded(crate::console::router(launchers), api_config.clone());
    let api_state = ApiState::new(facade.clone(), bus, tracker.clone(), api_config);
    let gateway = crate::net::gateway::router(crate::net::gateway::GatewayState {
        backend: Arc::new(gateway::GatewayAdapter::new(
            router,
            registry.clone(),
            facade.clone(),
        )),
        daemon_port: settings.daemon.port,
        https_port: settings.proxy.https_port,
        root_cert: net_paths.root_cert,
    });
    let mcp = crate::mcp::service(
        facade.clone(),
        crate::mcp::McpConfig {
            shutdown: shutdown.clone(),
            operations: tracker.clone(),
            upstream_policy: policy,
            trust_risk_metadata: true,
            allow_critical_calls: settings.mcp.allow_critical_calls,
            ..Default::default()
        },
    );
    let result = api::serve(
        listener,
        api::router_with_mcp(api_state, mcp).merge(gateway).merge(console),
        shutdown.clone(),
    )
    .await
    .map_err(|e| e.to_string());
    if let Some(address) = env.get("NOTIFY_SOCKET")
        && let Err(error) = notify_systemd(address, b"STOPPING=1")
    {
        tracing::warn!(%error, "systemd stopping notification failed");
    }
    shutdown.cancel();
    runtime.shutdown_background();
    let _ = git_task.await;
    let _ = error_task.await;
    let _ = network_task.await;
    let _ = dns_task.await;
    let _ = intel_task.await;
    if let Err(error) = intel.write_cache().await {
        tracing::warn!(%error, "intel cache flush failed");
    }
    let _ = services_task.await;
    discovery.join().await;
    loop {
        let running: Vec<_> = tracker
            .list()
            .into_iter()
            .filter(|op| {
                matches!(
                    op.status,
                    lantern_core::api::OperationStatus::Queued | lantern_core::api::OperationStatus::Running
                )
            })
            .map(|op| op.id)
            .collect();
        if running.is_empty() {
            break;
        }
        for id in running {
            let _ = tracker.wait(&id, Duration::from_secs(5)).await;
        }
    }
    registry.shutdown().await?;
    result
}

#[cfg(target_os = "linux")]
fn notify_systemd(address: &str, message: &[u8]) -> std::io::Result<()> {
    use std::os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    };
    let socket = UnixDatagram::unbound()?;
    if let Some(name) = address.strip_prefix('@') {
        socket.send_to_addr(message, &SocketAddr::from_abstract_name(name.as_bytes())?)?;
    } else {
        socket.send_to(message, address)?;
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn notify_systemd(_: &str, _: &[u8]) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod dev_tests {
    use super::*;

    #[test]
    fn app_dev_data_defaults_and_explicit_override() {
        let home = std::path::Path::new("/home/test");
        let mut env = BTreeMap::new();
        set_dev_defaults(&mut env, home);
        assert_eq!(env["LANTERN_DATA_DIR"], "/home/test/.local/share/lantern-dev");
        env.insert("LANTERN_DATA_DIR".into(), "/custom/data".into());
        set_dev_defaults(&mut env, home);
        assert_eq!(env["LANTERN_DATA_DIR"], "/custom/data");
    }
}

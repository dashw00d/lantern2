//! Consumer-defined interfaces for registry, discovery and checkout preparation.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::operations::OpHandle;
use lantern_core::ids::{InstanceId, ProjectId};
use lantern_core::manifest::ResolvedManifest;
use lantern_core::model::{Instance, Project, RuntimeState};
use tokio_util::sync::CancellationToken;

use crate::discovery::{PortBinding, Scanner};

/// Error reported by a registry adapter.
#[derive(Debug, thiserror::Error)]
#[error("runtime store: {0}")]
pub struct StoreError(pub String);

/// Registry operations needed by the runtime. Mutations must publish changed instance views.
pub trait RuntimeStore: Send + Sync {
    /// Read an instance together with its checkout and resolved manifest.
    fn instance(&self, id: &InstanceId) -> Result<Option<Instance>, StoreError>;
    /// Enumerate registered instances.
    fn instances(&self) -> Result<Vec<Instance>, StoreError>;
    /// Read a project, including its primary checkout path.
    fn project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError>;
    /// Read the persisted sticky port table.
    fn port_leases(&self) -> Result<BTreeMap<InstanceId, u16>, StoreError>;
    /// Atomically replace the table only if it still equals `expected`.
    fn compare_port_leases(
        &self,
        expected: &BTreeMap<InstanceId, u16>,
        replacement: BTreeMap<InstanceId, u16>,
    ) -> Result<bool, StoreError>;
    /// Mutate runtime state and publish an instance update when relevant fields change.
    fn update_runtime(
        &self,
        id: &InstanceId,
        update: Box<dyn FnOnce(&mut RuntimeState) + Send>,
    ) -> Result<(), StoreError>;
    /// Read whether a process-less site was activated.
    fn activated(&self, id: &InstanceId) -> Result<bool, StoreError>;
    /// Persist activation for a process-less site.
    fn set_activated(&self, id: &InstanceId, activated: bool) -> Result<(), StoreError>;
    /// Record the first and most recent successful start.
    fn record_started(&self, id: &InstanceId) -> Result<(), StoreError>;
    /// Current profile environment overlay for future launches.
    fn profile_environment(&self) -> Result<BTreeMap<String, String>, StoreError> {
        Ok(BTreeMap::new())
    }
    /// Current profile-specific port range, if configured.
    fn profile_port_range(&self) -> Result<Option<[u16; 2]>, StoreError> {
        Ok(None)
    }
    /// Resolve a dependency name to its primary instance, if it is a project.
    fn resolve_dependency(&self, name: &str) -> Result<Option<InstanceId>, StoreError>;
}

/// Fresh listener observations; implementations must not report errors as free ports.
pub trait PortProbe: Send + Sync {
    /// Find a listener on a port, optionally requiring the expected unit's ownership.
    fn port_binding(&self, port: u16, expected_unit: Option<&str>) -> io::Result<PortBinding>;
    /// Find other listening addresses owned by a unit, for mismatch diagnosis.
    fn unit_listeners(&self, unit: &str) -> io::Result<Vec<SocketAddr>>;
}

/// A synchronized adapter for the blocking discovery scanner.
#[derive(Debug)]
pub struct ScannerProbe(pub Mutex<Scanner>);

impl ScannerProbe {
    /// Wrap a scanner for use by the runtime's blocking workers.
    pub fn new(scanner: Scanner) -> Self {
        Self(Mutex::new(scanner))
    }
}

impl PortProbe for ScannerProbe {
    fn port_binding(&self, port: u16, expected_unit: Option<&str>) -> io::Result<PortBinding> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("scanner lock poisoned"))?
            .port_binding(port, expected_unit)
    }
    fn unit_listeners(&self, unit: &str) -> io::Result<Vec<SocketAddr>> {
        let snapshot = self
            .0
            .lock()
            .map_err(|_| io::Error::other("scanner lock poisoned"))?
            .scan()?;
        Ok(snapshot
            .listeners
            .into_iter()
            .chain(snapshot.hidden)
            .filter(|l| l.unit.as_deref() == Some(unit))
            .map(|l| l.addr)
            .collect())
    }
}

/// Resolved post-setup manifest and already-expanded worktree environment.
#[derive(Debug, Clone)]
pub struct PreparedInstance {
    /// Manifest resolved after preparation.
    pub manifest: Arc<ResolvedManifest>,
    /// Non-primary checkout environment, overriding run.env.
    pub worktree_env: BTreeMap<String, String>,
}

/// Asynchronous checkout-preparation result.
pub type PrepareFuture<'a> = Pin<Box<dyn Future<Output = Result<PreparedInstance, String>> + Send + 'a>>;
/// Asynchronous shared-service result.
pub type ServiceFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Hook implemented by the instances subsystem; cancellation must stop any child one-shot.
pub trait InstancePreparer: Send + Sync {
    /// Ensure the instance is ready to run, with its sticky port already reserved.
    fn ensure_ready<'a>(
        &'a self,
        instance: &'a Instance,
        port: Option<u16>,
        cancel: CancellationToken,
        progress: &'a OpHandle,
    ) -> PrepareFuture<'a>;
}

/// Default preparation hook for integrations that do not need checkout setup.
#[derive(Debug, Default)]
pub struct NoopPreparer;
impl InstancePreparer for NoopPreparer {
    fn ensure_ready<'a>(
        &'a self,
        instance: &'a Instance,
        _: Option<u16>,
        _: CancellationToken,
        _: &'a OpHandle,
    ) -> PrepareFuture<'a> {
        Box::pin(async move {
            Ok(PreparedInstance {
                manifest: instance.manifest.clone(),
                worktree_env: BTreeMap::new(),
            })
        })
    }
}

/// A route target supplied to the eventual networking adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteTarget {
    /// Reverse proxy to a ready managed listener.
    Socket(SocketAddr),
    /// Reverse proxy to a configured upstream URL.
    Url(String),
    /// Serve a PHP root through php-fpm.
    Php(std::path::PathBuf),
    /// Serve a static directory.
    Static(std::path::PathBuf),
}

/// An asynchronous router operation.
pub type RouteFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Routing and php-fpm boundary, implemented by the networking integration.
pub trait RuntimeRouter: Send + Sync {
    /// Publish the selected route for an instance.
    fn publish<'a>(&'a self, instance: &'a Instance, target: RouteTarget) -> RouteFuture<'a>;
    /// Remove an instance's route, without stopping its upstream.
    fn unpublish<'a>(&'a self, id: &'a InstanceId) -> RouteFuture<'a>;
    /// Verify that the configured php-fpm socket is reachable.
    fn php_ready<'a>(&'a self) -> RouteFuture<'a>;
}

/// Placeholder until the network subsystem provides a real adapter.
#[derive(Debug, Default)]
pub struct UnavailableRouter;
impl RuntimeRouter for UnavailableRouter {
    fn publish<'a>(&'a self, _: &'a Instance, _: RouteTarget) -> RouteFuture<'a> {
        Box::pin(async { Err("router adapter unavailable".into()) })
    }
    fn unpublish<'a>(&'a self, _: &'a InstanceId) -> RouteFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    fn php_ready<'a>(&'a self) -> RouteFuture<'a> {
        Box::pin(async { Err("php-fpm adapter unavailable".into()) })
    }
}

/// Boundary for shared services named in a project's dependencies.
pub trait ServiceControl: Send + Sync {
    /// Whether a service name is available in this integration.
    fn has(&self, name: &str) -> bool;
    /// Ensure the service is running without taking ownership of its lifecycle.
    fn ensure_running<'a>(
        &'a self,
        name: &'a str,
        timeout: std::time::Duration,
        cancel: CancellationToken,
    ) -> ServiceFuture<'a>;
}

/// Default shared-service adapter that recognizes no services.
#[derive(Debug, Default)]
pub struct UnavailableServices;
impl ServiceControl for UnavailableServices {
    fn has(&self, _: &str) -> bool {
        false
    }
    fn ensure_running<'a>(
        &'a self,
        _: &'a str,
        _: std::time::Duration,
        _: CancellationToken,
    ) -> ServiceFuture<'a> {
        Box::pin(async { Err("service adapter unavailable".into()) })
    }
}

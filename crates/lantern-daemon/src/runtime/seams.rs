//! Consumer-defined interfaces for registry, discovery and checkout preparation.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Mutex;

use lantern_core::ids::{InstanceId, ProjectId};
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
    /// Persist the replacement sticky port table.
    fn set_port_leases(&self, leases: BTreeMap<InstanceId, u16>) -> Result<(), StoreError>;
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

/// Asynchronous checkout-preparation result.
pub type PrepareFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Hook implemented by the instances subsystem; cancellation must stop any child one-shot.
pub trait InstancePreparer: Send + Sync {
    /// Ensure the instance is ready to run, with its sticky port already reserved.
    fn ensure_ready<'a>(
        &'a self,
        instance: &'a Instance,
        port: Option<u16>,
        cancel: CancellationToken,
    ) -> PrepareFuture<'a>;
}

/// Default preparation hook for integrations that do not need checkout setup.
#[derive(Debug, Default)]
pub struct NoopPreparer;
impl InstancePreparer for NoopPreparer {
    fn ensure_ready<'a>(
        &'a self,
        _: &'a Instance,
        _: Option<u16>,
        _: CancellationToken,
    ) -> PrepareFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

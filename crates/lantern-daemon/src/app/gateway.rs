//! Adapter from the live route plan to the registry and normal lifecycle gate.

use crate::{
    api::facade::DaemonFacade,
    net::{
        gateway::{GatewayBackend, GatewayInstance, GatewayStartFuture},
        routes::RouteOwner,
        sync::NetRouter,
    },
    registry::RegistryHandle,
};
use lantern_core::ids::InstanceId;
use std::sync::Arc;

/// Gateway backed by the same desired route plan and facade as the REST API.
#[derive(Clone)]
pub struct GatewayAdapter {
    router: Arc<NetRouter>,
    registry: RegistryHandle,
    facade: Arc<dyn DaemonFacade>,
}
impl std::fmt::Debug for GatewayAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayAdapter").finish_non_exhaustive()
    }
}
impl GatewayAdapter {
    /// Construct the gateway adapter from the live daemon components.
    pub fn new(router: Arc<NetRouter>, registry: RegistryHandle, facade: Arc<dyn DaemonFacade>) -> Self {
        Self {
            router,
            registry,
            facade,
        }
    }
}
impl GatewayBackend for GatewayAdapter {
    fn known_hosts(&self) -> Vec<String> {
        self.router
            .current_plan()
            .routes
            .into_iter()
            .map(|r| r.host)
            .collect()
    }
    fn instance_for_host(&self, host: &str) -> Option<GatewayInstance> {
        let id = self.router.current_plan().routes.into_iter().find_map(|route| {
            if route.host == host
                && let RouteOwner::Instance(id) = route.owner
            {
                return Some(id);
            }
            None
        })?;
        let instance = self.registry.instance(&id)?;
        Some(GatewayInstance {
            id,
            status: instance.runtime.status,
            message: instance.runtime.message,
            port: instance.runtime.port,
        })
    }
    fn start<'a>(&'a self, id: &'a InstanceId) -> GatewayStartFuture<'a> {
        Box::pin(async move {
            self.facade
                .start(id.clone())
                .await
                .map(|_| ())
                .map_err(|e| e.message)
        })
    }
}

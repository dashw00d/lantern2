//! Read-only status and dependency checks for externally managed services.

use crate::runtime::seams::{ServiceControl, ServiceFuture};
use lantern_core::{
    api::{ServiceMode, ServiceName, ServiceState, ServiceStatus},
    settings::{ServiceSettings, Settings},
};
use std::{collections::BTreeMap, time::Duration};
use tokio_util::sync::CancellationToken;

/// Read-only loopback service observer. It never starts or stops a service.
#[derive(Debug, Clone)]
pub struct ServiceObserver {
    settings: Settings,
    probes_enabled: bool,
}
impl ServiceObserver {
    /// Construct the observer, optionally suppressing probes in isolated tests.
    pub fn new(settings: Settings, probes_enabled: bool) -> Self {
        Self {
            settings,
            probes_enabled,
        }
    }
    fn config(&self, name: &str) -> Option<(ServiceName, &ServiceSettings, u16, &'static str)> {
        match name {
            "mailpit" => Some((ServiceName::Mailpit, &self.settings.services.mailpit, 8025, "ui")),
            "redis" => Some((ServiceName::Redis, &self.settings.services.redis, 6379, "default")),
            "postgres" => Some((
                ServiceName::Postgres,
                &self.settings.services.postgres,
                5432,
                "default",
            )),
            _ => None,
        }
    }
    /// Inspect the configured loopback port and report an explicitly read-only status.
    pub async fn status(&self, name: &str) -> Option<ServiceStatus> {
        let (service, settings, default_port, key) = self.config(name)?;
        let port = settings.port.unwrap_or(default_port);
        let running = settings.enabled
            && self.probes_enabled
            && tokio::time::timeout(
                Duration::from_millis(500),
                tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)),
            )
            .await
            .is_ok_and(|r| r.is_ok());
        let mut ports = BTreeMap::from([(key.to_string(), port)]);
        if name == "mailpit" {
            ports.insert("smtp".into(), 1025);
        }
        Some(ServiceStatus {
            name: service,
            state: if running {
                ServiceState::Running
            } else {
                ServiceState::Stopped
            },
            mode: ServiceMode::System,
            ports,
            ui_url: (name == "mailpit").then(|| {
                format!(
                    "https://mailpit.{}{}",
                    self.settings.domains.tld,
                    if self.settings.proxy.https_port == 443 {
                        String::new()
                    } else {
                        format!(":{}", self.settings.proxy.https_port)
                    }
                )
            }),
            unit: None,
            controllable: false,
            detail: Some(if !settings.enabled {
                "Disabled in settings".into()
            } else if !self.probes_enabled {
                "Service probing disabled in isolated mode".into()
            } else if running {
                "Loopback listener detected; ownership is not verified. Control it outside Lantern.".into()
            } else {
                "No loopback listener detected; start the service outside Lantern.".into()
            }),
        })
    }
}
impl ServiceControl for ServiceObserver {
    fn has(&self, name: &str) -> bool {
        self.config(name).is_some_and(|(_, config, _, _)| config.enabled)
    }
    fn ensure_running<'a>(
        &'a self,
        name: &'a str,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> ServiceFuture<'a> {
        Box::pin(async move {
            tokio::select! {
                () = cancel.cancelled() => Err("cancelled".into()),
                result = tokio::time::timeout(timeout, self.status(name)) => match result {
                    Ok(Some(status)) if status.state == ServiceState::Running => Ok(()),
                    Ok(Some(status)) => Err(status.detail.unwrap_or_else(|| "service not running".into())),
                    Ok(None) => Err("unknown service".into()),
                    Err(_) => Err("service status timed out".into()),
                }
            }
        })
    }
}

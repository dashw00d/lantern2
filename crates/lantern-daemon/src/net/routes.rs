//! Deterministic, side-effect-free route planning.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lantern_core::ids::InstanceId;
use lantern_core::model::{Instance, InstanceStatus, Scheme, ServeMode};
use lantern_core::settings::ProxySettings;

use crate::runtime::seams::RouteTarget;

/// Labels reserved for the daemon and its bundled service.
pub const RESERVED_LABELS: [&str; 2] = ["lantern", "mailpit"];

/// A route's registry owner, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOwner {
    /// Registered instance.
    Instance(InstanceId),
    /// A named shared service.
    Service(&'static str),
    /// The Lantern daemon hostname.
    Gateway,
}

/// The handler used for a hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A runtime-provided route and whether the upstream speaks HTTPS.
    Live { target: RouteTarget, tls: bool },
    /// A registered but unavailable instance, served by the gateway.
    Down,
    /// Lantern's own host.
    Gateway,
}

/// A literal host and its selected handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Lowercase DNS hostname.
    pub host: String,
    /// Instance or system owner.
    pub owner: RouteOwner,
    /// Handler for ordinary requests.
    pub target: Target,
}

/// Complete, sorted set of literal routes and rejected inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutePlan {
    /// Routes sorted by hostname.
    pub routes: Vec<Route>,
    /// Host and reason for each invalid or duplicate entry.
    pub skipped: Vec<(String, String)>,
}

/// Inputs for route planning; discovery's auto-adopt policy is supplied by the caller.
#[derive(Debug)]
pub struct PlanInput<'a> {
    /// Current registry instances.
    pub instances: &'a [Instance],
    /// Effective single-label TLD.
    pub tld: &'a str,
    /// Whether external listeners are adopted automatically.
    pub auto_adopt: bool,
}

/// Validate the ASCII DNS form accepted for literal hosts.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Normalize an unspecified listen address to loopback before dialing it.
pub fn normalize_upstream(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
    } else {
        addr
    }
}

fn selected(instance: &Instance, auto_adopt: bool) -> Target {
    let state = &instance.runtime;
    if state.status == InstanceStatus::External {
        if (instance.adopt || auto_adopt)
            && let Some(ext) = &state.external
        {
            return Target::Live {
                target: RouteTarget::Socket(normalize_upstream(ext.upstream)),
                tls: ext.scheme == Scheme::Https,
            };
        }
        return Target::Down;
    }
    if !matches!(state.status, InstanceStatus::Running | InstanceStatus::Unhealthy) {
        return Target::Down;
    }
    if let Some(url) = &instance.manifest.upstream_url {
        return Target::Live {
            target: RouteTarget::Url(url.clone()),
            tls: false,
        };
    }
    let relative_root = std::path::Path::new(instance.manifest.root.as_deref().unwrap_or("."));
    if matches!(instance.manifest.serve, ServeMode::Php | ServeMode::Static)
        && relative_root.components().any(|part| {
            !matches!(
                part,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Target::Down;
    }
    let root = instance.checkout.path.join(relative_root);
    let target = match instance.manifest.serve {
        ServeMode::Php => Some(RouteTarget::Php(root)),
        ServeMode::Static => Some(RouteTarget::Static(root)),
        _ => state
            .upstream
            .or_else(|| state.port.map(|port| SocketAddr::from(([127, 0, 0, 1], port))))
            .map(normalize_upstream)
            .map(RouteTarget::Socket),
    };
    target.map_or(Target::Down, |target| Target::Live { target, tls: false })
}

/// Build a deterministic plan, keeping stopped hosts available for gateway pages.
pub fn build_plan(input: &PlanInput<'_>) -> RoutePlan {
    let mut plan = RoutePlan::default();
    let gateway = format!("lantern.{}", input.tld);
    if valid_host(&gateway) && valid_host(input.tld) && !input.tld.contains('.') {
        plan.routes.push(Route {
            host: gateway.clone(),
            owner: RouteOwner::Gateway,
            target: Target::Gateway,
        });
    } else {
        plan.skipped.push((gateway.clone(), "invalid TLD".into()));
    }
    let mut instances: Vec<_> = input.instances.iter().collect();
    instances.sort_by(|a, b| a.domain.cmp(&b.domain).then_with(|| a.id.cmp(&b.id)));
    for instance in instances {
        let host = &instance.domain;
        let suffix = format!(".{}", input.tld);
        let label = host.strip_suffix(&suffix).unwrap_or("");
        let reason =
            if !valid_host(host) || label.is_empty() || !valid_host(input.tld) || input.tld.contains('.') {
                Some("invalid or out-of-zone hostname")
            } else if label == "lantern" || label == "mailpit" {
                Some("reserved hostname")
            } else if plan.routes.iter().any(|route| route.host == *host) {
                Some("duplicate hostname")
            } else {
                None
            };
        if let Some(reason) = reason {
            plan.skipped.push((host.clone(), reason.into()));
            continue;
        }
        plan.routes.push(Route {
            host: host.clone(),
            owner: RouteOwner::Instance(instance.id.clone()),
            target: selected(instance, input.auto_adopt),
        });
    }
    plan.routes.sort_by(|a, b| a.host.cmp(&b.host));
    plan
}

/// Add the Mailpit UI hostname, serving its stopped page when the service is down.
pub fn add_mailpit(plan: &mut RoutePlan, tld: &str, upstream: Option<SocketAddr>) {
    let host = format!("mailpit.{tld}");
    if !valid_host(tld) || tld.contains('.') || !valid_host(&host) {
        plan.skipped.push((host, "invalid TLD".into()));
        return;
    }
    plan.routes.retain(|route| route.host != host);
    let target = upstream.map_or(Target::Down, |addr| Target::Live {
        target: RouteTarget::Socket(normalize_upstream(addr)),
        tls: false,
    });
    plan.routes.push(Route {
        host,
        owner: RouteOwner::Service("mailpit"),
        target,
    });
    plan.routes.sort_by(|a, b| a.host.cmp(&b.host));
}

/// Format an instance URL with a nonstandard HTTPS port when needed.
pub fn public_url(settings: &ProxySettings, domain: &str) -> String {
    if settings.https_port == 443 {
        format!("https://{domain}")
    } else {
        format!("https://{domain}:{}", settings.https_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn net_host_validation_and_empty_plan() {
        assert!(valid_host("feature.app.glow"));
        for host in ["", "UPPER.glow", "-x.glow", "x..glow", "x_.glow"] {
            assert!(!valid_host(host));
        }
        let plan = build_plan(&PlanInput {
            instances: &[],
            tld: "glow",
            auto_adopt: false,
        });
        assert_eq!(plan.routes[0].host, "lantern.glow");
        assert_eq!(
            normalize_upstream("[::]:4567".parse().unwrap()).to_string(),
            "127.0.0.1:4567"
        );
        assert_eq!(
            normalize_upstream("[::1]:4567".parse().unwrap()).to_string(),
            "[::1]:4567"
        );
    }
}

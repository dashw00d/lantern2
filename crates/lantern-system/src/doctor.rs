//! Read-only health checks and an injectable network-probe boundary.
use crate::{
    command::{CommandRunner, CommandSpec},
    paths::LanternPaths,
    step::{Step, StepState},
    steps::{
        dns_delegate::DnsDelegateStep,
        nss_trust::{self, Browser},
        sysctl::{PortStrategy, PortsStep},
        v1_migrate,
    },
};
use lantern_core::{
    api::{Check, CheckStatus, Fix},
    settings::Settings,
};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

/// Network operation performed by the doctor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The daemon's loopback HTTP endpoint.
    Daemon,
    /// Caddy's Unix administration socket.
    CaddyAdmin,
    /// A DNS question to Lantern's responder.
    DnsResponder,
    /// System resolver lookup.
    DnsSystem,
    /// HTTPS with Lantern's root as the trust anchor.
    TlsServing,
    /// HTTPS with the system resolver and system trust.
    EndToEnd,
}
/// Read-only network operations; tests implement this without opening sockets.
pub trait NetworkProbe {
    /// Run one probe and return a human-readable observation or error.
    fn check(&self, probe: Probe) -> Result<String, String>;
}
/// Read-only network probes against the configured local services.
pub struct LocalNetworkProbe<'a> {
    /// Paths for the CA and administration socket.
    pub paths: &'a LanternPaths,
    /// Effective settings.
    pub settings: &'a Settings,
    /// Command runner for the HTTPS probes.
    pub runner: &'a dyn CommandRunner,
}
impl std::fmt::Debug for LocalNetworkProbe<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalNetworkProbe").finish_non_exhaustive()
    }
}
fn http_request(stream: &mut (impl Read + Write), host: &str, path: &str) -> Result<String, String> {
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    stream
        .take(65536)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&data);
    if !text.starts_with("HTTP/1.1 200 ") && !text.starts_with("HTTP/1.0 200 ") {
        return Err("HTTP endpoint did not return 200".into());
    }
    Ok("HTTP endpoint responded".into())
}
fn dns_query(tld: &str, address: SocketAddr) -> Result<String, String> {
    if !crate::steps::valid_tld(tld) || !address.ip().is_loopback() || address.port() == 0 {
        return Err("invalid DNS settings".into());
    }
    let mut packet = vec![0x4c, 0x4e, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["lantern-probe", tld] {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.extend_from_slice(&[0, 0, 1, 0, 1]);
    let socket = UdpSocket::bind(if address.is_ipv4() {
        "127.0.0.1:0"
    } else {
        "[::1]:0"
    })
    .map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    socket.connect(address).map_err(|e| e.to_string())?;
    socket.send(&packet).map_err(|e| e.to_string())?;
    let mut response = [0; 4096];
    let len = socket.recv(&mut response).map_err(|e| e.to_string())?;
    let response = &response[..len];
    if response.len() < 12
        || response[..2] != packet[..2]
        || response[2] & 0x80 == 0
        || response[3] & 0x0f != 0
    {
        return Err("invalid DNS response".into());
    }
    let questions = u16::from_be_bytes([response[4], response[5]]);
    let answers = u16::from_be_bytes([response[6], response[7]]);
    fn name_end(bytes: &[u8], mut i: usize) -> Option<usize> {
        loop {
            let n = *bytes.get(i)?;
            i += 1;
            if n == 0 {
                return Some(i);
            }
            if n & 0xc0 == 0xc0 {
                return (i < bytes.len()).then_some(i + 1);
            }
            if n & 0xc0 != 0 {
                return None;
            }
            i = i.checked_add(n as usize)?;
            if i > bytes.len() {
                return None;
            }
        }
    }
    let mut pos = 12;
    for _ in 0..questions {
        pos = name_end(response, pos)
            .and_then(|p| p.checked_add(4))
            .ok_or("invalid DNS question")?;
        if pos > response.len() {
            return Err("truncated DNS question".into());
        }
    }
    for _ in 0..answers {
        pos = name_end(response, pos).ok_or("invalid DNS answer")?;
        if pos + 10 > response.len() {
            return Err("truncated DNS answer".into());
        }
        let kind = u16::from_be_bytes([response[pos], response[pos + 1]]);
        let class = u16::from_be_bytes([response[pos + 2], response[pos + 3]]);
        let size = u16::from_be_bytes([response[pos + 8], response[pos + 9]]) as usize;
        pos += 10;
        if pos + size > response.len() {
            return Err("truncated DNS data".into());
        }
        if kind == 1 && class == 1 && response[pos..pos + size] == [127, 0, 0, 1] {
            return Ok("DNS responder returned 127.0.0.1".into());
        }
        pos += size;
    }
    Err("DNS response did not contain 127.0.0.1".into())
}
impl NetworkProbe for LocalNetworkProbe<'_> {
    fn check(&self, probe: Probe) -> Result<String, String> {
        let tld = &self.settings.domains.tld;
        if !crate::steps::valid_tld(tld) {
            return Err("invalid TLD".into());
        }
        match probe {
            Probe::Daemon => {
                let port = self.settings.daemon.port;
                let addr = SocketAddr::from(([127, 0, 0, 1], port));
                let mut s =
                    TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| e.to_string())?;
                s.set_read_timeout(Some(Duration::from_secs(2)))
                    .map_err(|e| e.to_string())?;
                http_request(&mut s, &format!("127.0.0.1:{port}"), "/api/system")
            }
            Probe::CaddyAdmin => {
                let path = self
                    .paths
                    .caddy_admin
                    .as_ref()
                    .ok_or("no user runtime directory")?;
                let mut s = UnixStream::connect(path).map_err(|e| e.to_string())?;
                s.set_read_timeout(Some(Duration::from_secs(2)))
                    .map_err(|e| e.to_string())?;
                http_request(&mut s, "localhost", "/config/")
            }
            Probe::DnsResponder => dns_query(
                tld,
                self.settings
                    .dns
                    .listen
                    .parse()
                    .map_err(|_| "invalid DNS listener")?,
            ),
            Probe::DnsSystem => {
                let mut addrs = (format!("lantern-probe.{tld}"), 0)
                    .to_socket_addrs()
                    .map_err(|e| e.to_string())?;
                if addrs.any(|a| a.ip() == std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)) {
                    Ok("system resolver returned loopback".into())
                } else {
                    Err("system resolver did not return IPv4 loopback".into())
                }
            }
            Probe::TlsServing => {
                let pem = crate::ca::read_bounded_nofollow(&self.paths.root_cert, 16 * 1024)
                    .map_err(|e| e.to_string())?;
                let der = nss_trust::certificate_der(&pem)?;
                crate::ca::inspect_der(&der, tld).map_err(|e| e.to_string())?;
                let mut roots = rustls::RootCertStore::empty();
                roots
                    .add(rustls::pki_types::CertificateDer::from(der))
                    .map_err(|e| e.to_string())?;
                let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
                let name = rustls::pki_types::ServerName::try_from(format!("lantern.{tld}"))
                    .map_err(|e| e.to_string())?;
                let mut tls = rustls::ClientConnection::new(std::sync::Arc::new(config), name)
                    .map_err(|e| e.to_string())?;
                let addr = SocketAddr::from(([127, 0, 0, 1], self.settings.proxy.https_port));
                let mut socket =
                    TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| e.to_string())?;
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .map_err(|e| e.to_string())?;
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .map_err(|e| e.to_string())?;
                while tls.is_handshaking() {
                    tls.complete_io(&mut socket).map_err(|e| e.to_string())?;
                }
                Ok("TLS handshake verified against Lantern's root".into())
            }
            Probe::EndToEnd => {
                let port = self.settings.proxy.https_port;
                let url = format!("https://lantern.{tld}:{port}/__lantern/ping");
                let out = self
                    .runner
                    .run(&CommandSpec::new(
                        "/usr/bin/curl",
                        ["-sS", "--max-time", "3", "--noproxy", "*", &url],
                        false,
                    ))
                    .map_err(|e| e.to_string())?;
                if !out.success {
                    return Err(String::from_utf8_lossy(&out.stderr).trim().into());
                }
                if String::from_utf8_lossy(&out.stdout).trim() != "lantern-ok" {
                    return Err("unexpected HTTPS ping response".into());
                }
                Ok("HTTPS system resolver and trust verified".into())
            }
        }
    }
}
/// Inputs and injectable boundaries for read-only doctor checks.
pub struct DoctorCtx<'a> {
    /// Target user paths.
    pub paths: &'a LanternPaths,
    /// Effective settings.
    pub settings: &'a Settings,
    /// Read-only command boundary.
    pub runner: &'a dyn CommandRunner,
    /// Read-only network boundary.
    pub network: &'a dyn NetworkProbe,
    /// Absolute Caddy executable selected by the caller.
    pub caddy: &'a Path,
    /// Absolute certutil path, if installed.
    pub certutil: Option<&'a Path>,
    /// PATH directories to inspect for shadowing.
    pub search_path: Vec<PathBuf>,
}
impl std::fmt::Debug for DoctorCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoctorCtx")
            .field("caddy", &self.caddy)
            .finish_non_exhaustive()
    }
}
fn item(
    id: &str,
    title: &str,
    status: CheckStatus,
    detail: impl Into<String>,
    fix: &str,
    needs_root: bool,
) -> Check {
    Check {
        id: id.into(),
        title: title.into(),
        status,
        detail: detail.into(),
        fix: Some(Fix {
            description: format!("Fix {title}"),
            command: fix.into(),
            needs_root,
        }),
    }
}
fn from_result(id: &str, title: &str, result: Result<String, String>, fix: &str, root: bool) -> Check {
    match result {
        Ok(detail) => item(id, title, CheckStatus::Ok, detail, fix, root),
        Err(detail) => item(id, title, CheckStatus::Fail, detail, fix, root),
    }
}
fn from_step(id: &str, title: &str, step: &dyn Step) -> Check {
    match step.check() {
        StepState::Done(s) => item(
            id,
            title,
            CheckStatus::Ok,
            s,
            step.fix_command(),
            step.needs_root(),
        ),
        StepState::Pending(s) | StepState::Failed(s) => item(
            id,
            title,
            CheckStatus::Fail,
            s,
            step.fix_command(),
            step.needs_root(),
        ),
        StepState::Skipped(s) => item(
            id,
            title,
            CheckStatus::Skipped,
            s,
            step.fix_command(),
            step.needs_root(),
        ),
    }
}
fn command(runner: &dyn CommandRunner, program: &Path, args: &[&str]) -> Result<String, String> {
    let out = runner
        .run(&CommandSpec::new(program, args.iter().copied(), false))
        .map_err(|e| e.to_string())?;
    if out.success {
        Ok(String::from_utf8_lossy(&out.stdout).trim().into())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().into())
    }
}
fn command_failure(output: &crate::command::CommandOutput, command: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    if detail.is_empty() {
        format!("{command} failed without output")
    } else {
        format!("{command}: {detail}")
    }
}
fn user_output(ctx: &DoctorCtx<'_>, args: &[&str]) -> Result<crate::command::CommandOutput, String> {
    let mut spec = CommandSpec::new("/usr/bin/systemctl", args.iter().copied(), false);
    let runtime = ctx
        .paths
        .runtime
        .as_ref()
        .and_then(|p| p.parent())
        .ok_or("no user systemd session")?;
    spec.env
        .insert("XDG_RUNTIME_DIR".into(), runtime.as_os_str().into());
    spec.env.insert(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        format!("unix:path={}/bus", runtime.display()).into(),
    );
    ctx.runner
        .run(&spec)
        .map_err(|e| format!("systemctl {}: {e}", args.join(" ")))
}
fn user_command(ctx: &DoctorCtx<'_>, args: &[&str]) -> Result<String, String> {
    let out = user_output(ctx, args)?;
    if out.success {
        Ok(String::from_utf8_lossy(&out.stdout).trim().into())
    } else {
        Err(command_failure(&out, &format!("systemctl {}", args.join(" "))))
    }
}
fn session_check(ctx: &DoctorCtx<'_>) -> Check {
    let state = user_command(ctx, &["--user", "show-environment"])
        .and_then(|_| user_output(ctx, &["--user", "is-system-running"]));
    let (status, detail, fix) = match state {
        Ok(out) => match String::from_utf8_lossy(&out.stdout).trim() {
            "running" if out.success => (CheckStatus::Ok, "user systemd manager is running".into(), "systemctl --user --failed"),
            "degraded" => (CheckStatus::Warn, "user systemd manager is running but some units failed; inspect them with systemctl --user --failed".into(), "systemctl --user --failed"),
            _ => (CheckStatus::Fail, command_failure(&out, "systemctl --user is-system-running"), "systemctl --user status"),
        },
        Err(detail) => (CheckStatus::Fail, detail, "systemctl --user status"),
    };
    item("session.systemd", "User systemd", status, detail, fix, false)
}
fn free_ports(ctx: &DoctorCtx<'_>) -> Result<String, String> {
    let mut saw = false;
    let mut held = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let path = ctx.paths.system(Path::new(file));
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        saw = true;
        for line in text.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                continue;
            }
            let port = fields[1]
                .rsplit_once(':')
                .and_then(|(_, p)| u16::from_str_radix(p, 16).ok());
            if port == Some(ctx.settings.proxy.http_port) || port == Some(ctx.settings.proxy.https_port) {
                held.push((port.unwrap(), fields[9].to_owned()));
            }
        }
    }
    if !saw {
        return Err("cannot read /proc/net/tcp or tcp6".into());
    }
    if held.is_empty() {
        return Ok("configured ports have no listeners".into());
    }
    let pid = user_command(
        ctx,
        &[
            "--user",
            "show",
            "-p",
            "MainPID",
            "--value",
            "lantern-caddy.service",
        ],
    )
    .ok()
    .and_then(|s| s.parse::<u32>().ok())
    .filter(|p| *p != 0);
    let owned: Vec<String> = pid
        .and_then(|p| fs::read_dir(ctx.paths.system(Path::new(&format!("/proc/{p}/fd")))).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::read_link(e.path()).ok())
        .filter_map(|p| {
            p.to_str()
                .and_then(|s| s.strip_prefix("socket:["))
                .and_then(|s| s.strip_suffix(']'))
                .map(str::to_owned)
        })
        .collect();
    if held.iter().all(|(_, inode)| owned.contains(inode)) {
        Ok("configured ports belong to Lantern Caddy".into())
    } else {
        Err(format!(
            "configured port(s) held by another or unknown process: {held:?}"
        ))
    }
}
fn api_port_conflict(ctx: &DoctorCtx<'_>) -> Result<Option<String>, String> {
    let mut available = false;
    let mut inodes = std::collections::BTreeSet::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(ctx.paths.system(Path::new(file))) else {
            continue;
        };
        available = true;
        for line in text.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                continue;
            }
            let port = fields[1]
                .rsplit_once(':')
                .and_then(|(_, p)| u16::from_str_radix(p, 16).ok());
            if port == Some(ctx.settings.daemon.port) {
                inodes.insert(fields[9].to_owned());
            }
        }
    }
    if !available {
        return Err("cannot inspect API port listeners".into());
    }
    if inodes.is_empty() {
        return Ok(None);
    }
    let proc_root = ctx.paths.system(Path::new("/proc"));
    let entries = fs::read_dir(proc_root).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().and_then(|s| s.parse::<u32>().ok()).is_none() {
            continue;
        }
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let matches = fds
            .flatten()
            .filter_map(|fd| fs::read_link(fd.path()).ok())
            .any(|target| {
                target
                    .to_str()
                    .and_then(|s| s.strip_prefix("socket:["))
                    .and_then(|s| s.strip_suffix(']'))
                    .is_some_and(|inode| inodes.contains(inode))
            });
        if !matches {
            continue;
        }
        let exe = fs::read_link(entry.path().join("exe")).ok();
        if exe
            .as_ref()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == "lantern")
        {
            continue;
        }
        return Ok(Some(format!(
            "API port {} is held by pid {} ({})",
            ctx.settings.daemon.port,
            name.to_string_lossy(),
            exe.map_or_else(|| "unknown executable".into(), |p| p.display().to_string())
        )));
    }
    // An inaccessible fd is not evidence that the listener is ours.
    let lantern_owns_all = {
        let mut owned = std::collections::BTreeSet::new();
        if let Ok(entries) = fs::read_dir(ctx.paths.system(Path::new("/proc"))) {
            for entry in entries.flatten() {
                let exe = fs::read_link(entry.path().join("exe")).ok();
                if !exe
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| n == "lantern")
                {
                    continue;
                }
                if let Ok(fds) = fs::read_dir(entry.path().join("fd")) {
                    for link in fds.flatten().filter_map(|fd| fs::read_link(fd.path()).ok()) {
                        if let Some(inode) = link
                            .to_str()
                            .and_then(|s| s.strip_prefix("socket:["))
                            .and_then(|s| s.strip_suffix(']'))
                        {
                            owned.insert(inode.to_owned());
                        }
                    }
                }
            }
        }
        inodes.is_subset(&owned)
    };
    if lantern_owns_all {
        Ok(None)
    } else {
        Ok(Some(format!(
            "API port {} has an unidentified listener",
            ctx.settings.daemon.port
        )))
    }
}
/// Run all checks without mutating the machine or depending on the daemon.
pub fn run(ctx: &DoctorCtx<'_>) -> Vec<Check> {
    let mut checks = Vec::new();
    let settings = ctx.settings;
    let tld = &settings.domains.tld;
    let dns_addr = settings.dns.listen.parse::<SocketAddr>();
    checks.push(from_result(
        "daemon.running",
        "Daemon",
        ctx.network.check(Probe::Daemon),
        "systemctl --user start lantern",
        false,
    ));
    checks.push(session_check(ctx));
    checks.push(from_result(
        "session.unit",
        "Lantern user unit",
        user_command(ctx, &["--user", "is-enabled", "lantern.service"])
            .map(|state| format!("Lantern user unit is {state}")),
        "systemctl --user enable lantern",
        false,
    ));
    let caddy = if !ctx.caddy.is_absolute() {
        Err("Caddy path is not absolute".into())
    } else {
        command(ctx.runner, ctx.caddy, &["version"]).and_then(|v| {
            let version = v.split_whitespace().find_map(|s| {
                let s = s.trim_start_matches('v');
                let mut n = s.split('.');
                Some((
                    n.next()?.parse::<u32>().ok()?,
                    n.next()?.parse::<u32>().ok()?,
                    n.next()?
                        .split(|c: char| !c.is_ascii_digit())
                        .next()?
                        .parse::<u32>()
                        .ok()?,
                ))
            });
            if version.is_some_and(|v| v >= (2, 8, 0)) {
                Ok(v)
            } else {
                Err(format!("Caddy version is missing or older than 2.8: {v}"))
            }
        })
    };
    checks.push(from_result(
        "caddy.binary",
        "Caddy binary",
        caddy,
        "sudo apt install --reinstall lantern",
        true,
    ));
    let caddy_running = user_command(ctx, &["--user", "is-active", "lantern-caddy.service"])
        .and_then(|_| ctx.network.check(Probe::CaddyAdmin));
    checks.push(from_result(
        "caddy.running",
        "Caddy service",
        caddy_running,
        "systemctl --user restart lantern-caddy",
        false,
    ));
    let ports = PortsStep {
        paths: ctx.paths,
        runner: ctx.runner,
        http_port: settings.proxy.http_port,
        https_port: settings.proxy.https_port,
        caddy: ctx.caddy,
        strategy: PortStrategy::Sysctl,
        from_package: false,
    };
    checks.push(from_step("ports.bind", "Port permissions", &ports));
    checks.push(from_result(
        "ports.free",
        "Port availability",
        free_ports(ctx),
        "lantern doctor",
        false,
    ));
    if tld == "localhost" {
        for id in ["dns.resolved", "dns.delegate", "dns.responder", "dns.system"] {
            checks.push(item(
                id,
                "DNS",
                CheckStatus::Skipped,
                "localhost needs no DNS delegate",
                "lantern init",
                false,
            ));
        }
    } else {
        let resolved = command(ctx.runner,Path::new("/usr/bin/systemctl"),&["is-active","systemd-resolved"]).and_then(|_| command(ctx.runner,Path::new("/usr/bin/systemctl"),&["--version"])).and_then(|v| {
            let version = v.split_whitespace().nth(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            let resolv = ctx.paths.system(Path::new("/etc/resolv.conf"));
            let stub = fs::read_link(resolv).is_ok_and(|p| p.ends_with("run/systemd/resolve/stub-resolv.conf"));
            if version >= 258 && stub { Ok(format!("systemd-resolved {version} uses the stub")) } else { Err("requires systemd-resolved >=258 and its stub resolver; use localhost or configure a DNS forwarder".into()) }
        });
        checks.push(from_result(
            "dns.resolved",
            "Resolved DNS",
            resolved,
            "lantern settings set tld localhost",
            false,
        ));
        match dns_addr {
            Ok(listen) => checks.push(from_step(
                "dns.delegate",
                "DNS delegate",
                &DnsDelegateStep {
                    paths: ctx.paths,
                    runner: ctx.runner,
                    tld,
                    listen,
                    from_package: false,
                },
            )),
            Err(_) => checks.push(item(
                "dns.delegate",
                "DNS delegate",
                CheckStatus::Fail,
                "invalid DNS listener",
                "lantern init",
                false,
            )),
        }
        checks.push(from_result(
            "dns.responder",
            "DNS responder",
            ctx.network.check(Probe::DnsResponder),
            "systemctl --user restart lantern",
            false,
        ));
        checks.push(from_result(
            "dns.system",
            "System DNS",
            ctx.network.check(Probe::DnsSystem),
            "sudo lantern init --system-steps",
            true,
        ));
    }
    let ca = crate::ca::inspect_root(ctx.paths, tld, 30)
        .map(|info| {
            format!(
                "CA {} expires at Unix time {}",
                info.fingerprint_sha256, info.expires_at
            )
        })
        .map_err(|e| e.to_string());
    checks.push(from_result("tls.ca", "Local CA", ca, "lantern init", false));
    let trust = crate::steps::system_trust::SystemTrustStep {
        paths: ctx.paths,
        runner: ctx.runner,
        tld,
    };
    checks.push(from_step("tls.system", "System trust", &trust));
    let certutil = ctx.certutil.filter(|p| p.is_absolute());
    checks.push(item(
        "tls.certutil",
        "NSS tools",
        if certutil.is_some() {
            CheckStatus::Ok
        } else {
            CheckStatus::Fail
        },
        if certutil.is_some() {
            "certutil available"
        } else {
            "certutil not found"
        },
        "sudo apt install libnss3-tools",
        true,
    ));
    let root = crate::ca::inspect_root(ctx.paths, tld, 0)
        .ok()
        .and_then(|_| crate::ca::read_bounded_nofollow(&ctx.paths.root_cert, 16 * 1024).ok())
        .and_then(|v| nss_trust::certificate_der(&v).ok());
    for db in nss_trust::discover(ctx.paths) {
        let id = if db.browser == Browser::Chrome {
            "tls.nss.chrome"
        } else {
            "tls.nss.firefox"
        };
        let (status, detail) = match (certutil, root.as_deref()) {
            (Some(program), Some(der)) if nss_trust::trusted(ctx.runner, program, &db, der) => {
                (CheckStatus::Ok, format!("{} trusts the CA", db.path.display()))
            }
            (Some(_), Some(_)) => (
                CheckStatus::Fail,
                format!("{} does not trust the CA", db.path.display()),
            ),
            _ => (
                CheckStatus::Skipped,
                format!("{}: CA or certutil unavailable", db.path.display()),
            ),
        };
        checks.push(item(id, "Browser trust", status, detail, "lantern init", false));
    }
    checks.push(from_result(
        "tls.serving",
        "TLS serving",
        ctx.network.check(Probe::TlsServing),
        "systemctl --user restart lantern-caddy",
        false,
    ));
    checks.push(from_result(
        "e2e.https",
        "HTTPS end to end",
        ctx.network.check(Probe::EndToEnd),
        "lantern doctor",
        false,
    ));
    let mut strict = Vec::new();
    for db in nss_trust::discover(ctx.paths)
        .into_iter()
        .filter(|d| d.browser == Browser::Firefox)
    {
        if fs::read_to_string(db.path.join("prefs.js")).is_ok_and(|s| {
            s.lines().any(|l| {
                l.contains("network.trr.mode")
                    && l.split(',')
                        .nth(1)
                        .is_some_and(|v| v.trim_start().starts_with("3)"))
            })
        }) {
            strict.push(db.path);
        }
    }
    fn strict_doh(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
                (key == "dns_over_https"
                    && value.get("mode").and_then(serde_json::Value::as_str) == Some("secure"))
                    || strict_doh(value)
            }),
            serde_json::Value::Array(values) => values.iter().any(strict_doh),
            _ => false,
        }
    }
    let mut observed_browser_settings = false;
    for path in [
        ".config/google-chrome/Local State",
        ".config/chromium/Local State",
        "snap/chromium/current/.config/chromium/Local State",
    ] {
        let path = ctx.paths.home.join(path);
        if let Ok(data) = fs::read(&path) {
            observed_browser_settings = true;
            if serde_json::from_slice::<serde_json::Value>(&data).is_ok_and(|v| strict_doh(&v)) {
                strict.push(path);
            }
        }
    }
    let (doh_status, doh_detail) = if !strict.is_empty() {
        (
            CheckStatus::Warn,
            format!("Strict browser DoH may bypass local DNS: {strict:?}"),
        )
    } else if observed_browser_settings {
        (
            CheckStatus::Ok,
            "No strict DoH found in inspected browser settings".into(),
        )
    } else {
        (
            CheckStatus::Skipped,
            "No Chrome Local State found; Firefox profiles were inspected when present".into(),
        )
    };
    checks.push(item(
        "browser.doh",
        "Browser DNS over HTTPS",
        doh_status,
        doh_detail,
        "lantern doctor",
        false,
    ));
    let artifacts = v1_migrate::detect(ctx.paths);
    let port_conflict = api_port_conflict(ctx);
    let (v1_status, v1_detail) = match port_conflict {
        Ok(conflict) if !artifacts.is_empty() || conflict.is_some() => (
            CheckStatus::Warn,
            format!(
                "{} v1 artifact(s); {}",
                artifacts.len(),
                conflict.unwrap_or_else(|| "no foreign API listener".into())
            ),
        ),
        Ok(_) => (CheckStatus::Ok, "no v1 artifacts or foreign API listener".into()),
        Err(e) if !artifacts.is_empty() => (
            CheckStatus::Warn,
            format!("{} v1 artifact(s); {e}", artifacts.len()),
        ),
        Err(e) => (CheckStatus::Skipped, e),
    };
    checks.push(item(
        "conflict.v1",
        "v1 artifacts",
        v1_status,
        v1_detail,
        "sudo lantern init --system-steps --migrate-v1",
        true,
    ));
    let system_caddy = ctx.runner.run(&CommandSpec::new(
        "/usr/bin/systemctl",
        ["is-active", "caddy.service"],
        false,
    ));
    let (caddy_status, caddy_detail) = match system_caddy {
        Ok(output) if output.success => (CheckStatus::Fail, "system Caddy is active".to_string()),
        Ok(output)
            if matches!(
                String::from_utf8_lossy(&output.stdout).trim(),
                "inactive" | "failed" | "unknown"
            ) =>
        {
            (CheckStatus::Ok, "system Caddy is not active".to_string())
        }
        Ok(output) => (
            CheckStatus::Skipped,
            format!(
                "could not inspect system Caddy: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
        Err(e) => (
            CheckStatus::Skipped,
            format!("could not inspect system Caddy: {e}"),
        ),
    };
    checks.push(item(
        "conflict.caddy",
        "System Caddy",
        caddy_status,
        caddy_detail,
        "sudo systemctl disable --now caddy",
        true,
    ));
    let shadow = ctx
        .search_path
        .iter()
        .map(|p| p.join("lantern"))
        .find(|p| p.is_file());
    checks.push(item(
        "path.shadow",
        "Executable PATH",
        if shadow
            .as_ref()
            .is_some_and(|p| p != Path::new("/usr/bin/lantern"))
        {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        },
        format!(
            "first Lantern on PATH: {}",
            shadow.map_or_else(|| "not found".into(), |p| p.display().to_string())
        ),
        "command -v lantern",
        false,
    ));
    for (name, enabled) in [
        ("mailpit", settings.services.mailpit.enabled),
        ("redis", settings.services.redis.enabled),
        ("postgres", settings.services.postgres.enabled),
    ] {
        let unit = format!("lantern-svc-{name}.service");
        let observed = user_command(ctx, &["--user", "is-active", &unit]);
        let detail = if !enabled {
            "disabled in settings".to_string()
        } else {
            match observed {
                Ok(s) => format!("user unit {unit}: {s}"),
                Err(_) => format!("user unit {unit} not active or unavailable"),
            }
        };
        checks.push(item(
            &format!("services.{name}"),
            name,
            CheckStatus::Skipped,
            detail,
            "lantern services",
            false,
        ));
    }
    checks.push(item(
        "php.fpm",
        "PHP-FPM",
        CheckStatus::Skipped,
        "PHP instance presence is not available to this context",
        "lantern services start php-fpm",
        false,
    ));
    let linger = command(
        ctx.runner,
        Path::new("/usr/bin/loginctl"),
        &["show-user", "-p", "Linger", "--value"],
    );
    checks.push(item(
        "linger",
        "User lingering",
        CheckStatus::Skipped,
        match linger {
            Ok(value) => format!("Linger={value}"),
            Err(_) => "linger state unavailable".into(),
        },
        "loginctl enable-linger $USER",
        false,
    ));

    checks
}

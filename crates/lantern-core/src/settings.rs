//! Typed global settings with explicit environment and CLI precedence.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

macro_rules! section {
    ($(#[$meta:meta])* $name:ident { $($field:ident: $ty:ty = $default:expr),* $(,)? }) => {
        $(#[$meta])* #[derive(Debug,Clone,PartialEq,Serialize,Deserialize)] #[serde(default)]
        pub struct $name { $(pub $field:$ty),* }
        impl Default for $name { fn default()->Self { Self { $($field:$default),* } } }
    }
}
section!(/// Daemon HTTP listener settings.
    DaemonSettings { port:u16=4777 });
section!(/// Local domain settings.
    DomainSettings { tld:String="glow".into() });
section!(/// Workspace scan behavior.
    WorkspaceSettings { roots:Vec<String>=vec!["~/sites".into(),"~/tools".into()], depth:u8=2, ignore:Vec<String>=vec![], include_hidden:Vec<String>=vec![], scan_interval_secs:u32=60, suggest:bool=true });
section!(/// Sticky port lease range.
    PortSettings { range_start:u16=29000, range_end:u16=29999 });
section!(/// Worktree setup behavior.
    WorktreeSettings { dir:String=String::new(), clone_deps:String="never".into(), max_concurrent_setups:u32=2, setup_timeout_secs:u64=900, gc_hint_days:u32=30, missing_retention_days:u32=30 });
section!(/// Template selection.
    TemplateSettings { default:String=String::new() });
section!(/// Initial profile selection.
    ProfileSettings { default:String="light".into() });
section!(/// Runtime settings, including legacy v1 keys retained for compatibility.
    RuntimeSettings { shell:String=String::new(), startup_timeout_secs:u64=60, stop_timeout_secs:u64=10, settle_secs:u64=2, env_capture_timeout_secs:u64=10, deploy_timeout_secs:u64=120, max_retries:u32=5, backoff_base_secs:u64=1, backoff_cap_secs:u64=30, stable_after_secs:u64=60, env:BTreeMap<String,String>=BTreeMap::new(), start_timeout_secs:u64=15, max_restarts:u32=5, restart_backoff_ms:u64=1000, log_buffer_lines:u32=1000 });
section!(/// Health-check policy.
    HealthSettings { interval_secs:u64=10, timeout_secs:u64=3, fail_threshold:u32=3, grace_secs:u64=15 });
section!(/// Log settings.
    LogSettings { level:String="info".into(), ring_lines:usize=2000, ring_bytes:usize=2097152 });
section!(/// Proxy listener settings.
    ProxySettings { http_port:u16=80, https_port:u16=443, bind:String="loopback".into(), ipv6:bool=false });
section!(/// Caddy binary selection.
    CaddySettings { binary:String=String::new() });
section!(/// DNS listener settings.
    DnsSettings { listen:String="127.0.0.1:5354".into() });
section!(/// PHP-FPM socket selection.
    PhpSettings { fpm_socket:String="auto".into() });
section!(/// Shared service settings.
    ServiceSettings { enabled:bool=true, mode:String="auto".into(), port:Option<u16>=None });
/// Shared services settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServicesSettings {
    pub mailpit: ServiceSettings,
    pub redis: ServiceSettings,
    pub postgres: ServiceSettings,
}
impl Default for ServicesSettings {
    fn default() -> Self {
        Self {
            mailpit: ServiceSettings {
                port: Some(8025),
                ..Default::default()
            },
            redis: ServiceSettings {
                port: Some(6379),
                ..Default::default()
            },
            postgres: ServiceSettings {
                port: Some(5432),
                ..Default::default()
            },
        }
    }
}
section!(/// MCP upstream call policy.
    McpSettings { allowed_upstream_hosts:Vec<String>=vec![], allow_critical_calls:bool=false });
section!(/// Intel refresh and cache policy.
    IntelSettings { refresh_interval_minutes:u32=10, cache_file:bool=true, max_openapi_bytes:usize=5242880 });
section!(/// Editor and terminal command templates used by clients.
    ClientSettings { editor:String="auto".into(), terminal:String="auto".into() });
/// Effective configuration, after file and environment overrides.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub daemon: DaemonSettings,
    pub domains: DomainSettings,
    pub workspaces: WorkspaceSettings,
    pub ports: PortSettings,
    pub worktrees: WorktreeSettings,
    pub templates: TemplateSettings,
    pub profiles: ProfileSettings,
    pub runtime: RuntimeSettings,
    pub health: HealthSettings,
    pub logs: LogSettings,
    pub proxy: ProxySettings,
    pub caddy: CaddySettings,
    pub dns: DnsSettings,
    pub php: PhpSettings,
    pub services: ServicesSettings,
    pub mcp: McpSettings,
    pub intel: IntelSettings,
    pub clients: ClientSettings,
}
/// Environment access supplied by the caller, avoiding global environment reads in tests.
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}
impl EnvSource for BTreeMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        self.get(key).cloned()
    }
}
/// Read the actual process environment.
#[derive(Debug, Clone, Copy)]
pub struct ProcessEnv;
impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}
/// A nonfatal settings warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub path: String,
    pub message: String,
}
/// Parsed effective settings and overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSettings {
    pub settings: Settings,
    pub overridden_by_env: Vec<String>,
    pub warnings: Vec<Warning>,
}
/// Fatal syntax, type or validation error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{file}:{line}:{column}: {path}: {message}")]
pub struct Error {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub path: String,
    pub message: String,
}
fn error(file: &Path, src: &str, offset: Option<usize>, path: &str, message: impl Into<String>) -> Error {
    let start = offset.unwrap_or(0).min(src.len());
    let prefix = &src[..src.floor_char_boundary(start)];
    Error {
        file: file.into(),
        line: prefix.bytes().filter(|b| *b == b'\n').count() + 1,
        column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
        path: path.into(),
        message: message.into(),
    }
}
fn semantic_error(file: &Path, src: &str, path: &str, message: impl Into<String>) -> Error {
    let key = path.rsplit('.').next().unwrap_or(path);
    error(file, src, src.find(key), path, message)
}
/// Parse, apply environment/CLI overrides, normalize and validate a TOML file.
pub fn load_str(
    src: &str,
    file: impl AsRef<Path>,
    env: &dyn EnvSource,
    home: &Path,
    cli_port: Option<u16>,
) -> Result<LoadedSettings, Error> {
    let file = file.as_ref();
    let mut ignored = Vec::new();
    let de = toml::Deserializer::parse(src)
        .map_err(|e| error(file, src, e.span().map(|s| s.start), "", e.message()))?;
    let mut settings: Settings = serde_ignored::deserialize(de, |path| ignored.push(path.to_string()))
        .map_err(|e: toml::de::Error| error(file, src, e.span().map(|s| s.start), "", e.message()))?;
    let mut warnings: Vec<_> = ignored
        .into_iter()
        .map(|path| Warning {
            path,
            message: "unknown setting is ignored".into(),
        })
        .collect();
    let mut overrides = Vec::new();
    macro_rules! str_env {
        ($key:literal,$path:literal,$target:expr) => {
            if let Some(value) = env.get($key) {
                $target = value;
                overrides.push($path.into());
            }
        };
    }
    macro_rules! number_env {
        ($key:literal,$path:literal,$target:expr) => {
            if let Some(value) = env.get($key) {
                $target = value
                    .parse()
                    .map_err(|_| semantic_error(file, src, $path, format!("invalid value in {}", $key)))?;
                overrides.push($path.into());
            }
        };
    }
    number_env!("LANTERN_DAEMON_PORT", "daemon.port", settings.daemon.port);
    str_env!("LANTERN_TLD", "domains.tld", settings.domains.tld);
    if let Some(value) = env.get("LANTERN_WORKSPACES") {
        settings.workspaces.roots = value.split(':').map(str::to_owned).collect();
        overrides.push("workspaces.roots".into());
    }
    number_env!(
        "LANTERN_PORT_RANGE_START",
        "ports.range_start",
        settings.ports.range_start
    );
    number_env!(
        "LANTERN_PORT_RANGE_END",
        "ports.range_end",
        settings.ports.range_end
    );
    str_env!(
        "LANTERN_PHP_FPM_SOCKET",
        "php.fpm_socket",
        settings.php.fpm_socket
    );
    str_env!("LANTERN_WORKTREES_DIR", "worktrees.dir", settings.worktrees.dir);
    if let Some(value) = env.get("LANTERN_LOG").or_else(|| env.get("RUST_LOG")) {
        settings.logs.level = value;
        overrides.push("logs.level".into());
    }
    if let Some(port) = cli_port {
        settings.daemon.port = port;
    }
    settings.domains.tld = settings.domains.tld.trim_start_matches('.').to_owned();
    let tld = &settings.domains.tld;
    if tld.is_empty()
        || tld.len() > 63
        || !tld.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        || !tld
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || ["com", "net", "org", "io", "dev", "app", "ai", "co"].contains(&tld.as_str())
    {
        return Err(semantic_error(file, src, "domains.tld", "invalid or public TLD"));
    }
    if tld == "local" {
        warnings.push(Warning {
            path: "domains.tld".into(),
            message: "local is used by mDNS".into(),
        });
    }
    let (start, end) = (settings.ports.range_start, settings.ports.range_end);
    if start < 1024
        || start >= end
        || u32::from(end) - u32::from(start) + 1 < 16
        || (start..=end).contains(&settings.daemon.port)
    {
        return Err(semantic_error(
            file,
            src,
            "ports.range_start",
            "invalid range, fewer than 16 ports, or range contains daemon port",
        ));
    }
    if !(1..=5).contains(&settings.workspaces.depth) {
        return Err(semantic_error(
            file,
            src,
            "workspaces.depth",
            "depth must be 1 through 5",
        ));
    }
    for root in &mut settings.workspaces.roots {
        if root == "~" {
            *root = home.to_string_lossy().into_owned();
        } else if let Some(suffix) = root.strip_prefix("~/") {
            *root = home.join(suffix).to_string_lossy().into_owned();
        }
        let path = Path::new(root);
        if !path.is_absolute() || path == Path::new("/") {
            return Err(semantic_error(
                file,
                src,
                "workspaces.roots",
                "roots must be absolute and cannot be /",
            ));
        }
        if path == home {
            warnings.push(Warning {
                path: "workspaces.roots".into(),
                message: "scanning the entire home directory may be slow".into(),
            });
        }
    }
    Ok(LoadedSettings {
        settings,
        overridden_by_env: overrides,
        warnings,
    })
}
/// Load a missing-or-present settings file without writing it.
pub fn load(
    path: &Path,
    env: &dyn EnvSource,
    home: &Path,
    cli_port: Option<u16>,
) -> Result<LoadedSettings, Error> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(error(path, "", None, "", e.to_string())),
    };
    load_str(&src, path, env, home, cli_port)
}
/// Update a dotted TOML setting while preserving other comments and ordering.
pub fn edit_setting(src: &str, path: &str, value: toml_edit::Value) -> Result<String, String> {
    let mut doc = src.parse::<toml_edit::DocumentMut>().map_err(|e| e.to_string())?;
    let mut parts = path.split('.').peekable();
    let mut table = doc.as_table_mut();
    while let Some(part) = parts.next() {
        if part.is_empty() {
            return Err("empty setting path component".into());
        }
        if parts.peek().is_none() {
            table.insert(part, toml_edit::Item::Value(value));
            return Ok(doc.to_string());
        }
        if !table.contains_key(part) {
            table.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        table = table
            .get_mut(part)
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or_else(|| format!("{part} is not a table"))?;
    }
    Err("empty setting path".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_environment_and_cli_precedence() {
        let mut env = BTreeMap::new();
        env.insert("LANTERN_PORT".into(), "1234".into());
        env.insert("LANTERN_DAEMON_PORT".into(), "4888".into());
        env.insert("LANTERN_TLD".into(), ".glow".into());
        let got = load_str(
            "[daemon]\nport=5000\n",
            "config.toml",
            &env,
            Path::new("/home/test"),
            Some(4999),
        )
        .unwrap();
        assert_eq!(got.settings.daemon.port, 4999);
        assert_eq!(got.settings.domains.tld, "glow");
        assert_eq!(got.settings.ports.range_start, 29000);
        assert_eq!(
            got.settings.workspaces.roots,
            ["/home/test/sites", "/home/test/tools"]
        );
        assert!(got.overridden_by_env.contains(&"daemon.port".into()));
    }
    #[test]
    fn validation_and_unknowns() {
        let env = BTreeMap::new();
        let home = Path::new("/home/test");
        assert!(
            load_str(
                "[ports]\nrange_start=9000\nrange_end=9001",
                "config.toml",
                &env,
                home,
                None
            )
            .is_err()
        );
        assert!(load_str("[workspaces]\ndepth=6", "config.toml", &env, home, None).is_err());
        let x = load_str(
            "[clients]\neditor=\"code {path}\"\nunknown=1",
            "config.toml",
            &env,
            home,
            None,
        )
        .unwrap();
        assert_eq!(x.warnings[0].path, "clients.unknown");
    }
    #[test]
    fn edit_keeps_comments() {
        let s = edit_setting(
            "# comment\n[daemon]\nport = 4777 # keep\n",
            "daemon.port",
            toml_edit::Value::from(5000),
        )
        .unwrap();
        assert!(s.contains("# comment"));
        assert!(s.contains("5000"));
    }
}

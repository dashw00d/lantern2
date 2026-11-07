//! Manifest parsing, diagnostics and layered resolution.
pub mod raw;

use crate::{
    ids::ProjectId,
    model::{ProjectKind, ServeMode},
    template::TemplateString,
    yaml::{self, Node, Position},
};
use raw::*;
use serde::{Deserialize, Serialize, de::IntoDeserializer};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}
/// Stable, located manifest diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub path: String,
    pub file: PathBuf,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub expected: Option<String>,
    pub help: Option<String>,
}
impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}: {:?}[{}] {}: {}",
            self.file.display(),
            self.line.unwrap_or(1),
            self.column.unwrap_or(1),
            self.severity,
            self.code,
            self.path,
            self.message
        )
    }
}
/// Parsed fields and accumulated diagnostics, including successful fields on errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedManifest {
    pub raw: RawManifest,
    pub diagnostics: Vec<Diagnostic>,
    pub usable: bool,
    pub file: PathBuf,
}

fn index(
    node: &yaml::Spanned<Node>,
    path: &str,
    positions: &mut BTreeMap<String, Position>,
    keys: &mut BTreeMap<String, Position>,
) {
    positions.insert(path.to_string(), node.position);
    match &node.value {
        Node::Map(items) => {
            for (key, value) in items {
                let p = if path.is_empty() {
                    key.value.clone()
                } else {
                    format!("{path}.{}", key.value)
                };
                keys.insert(p.clone(), key.position);
                index(value, &p, positions, keys);
            }
        }
        Node::Seq(items) => {
            for (i, value) in items.iter().enumerate() {
                index(value, &format!("{path}[{i}]"), positions, keys);
            }
        }
        _ => {}
    }
}
fn nearest<'a>(mut path: &'a str, map: &'a BTreeMap<String, Position>) -> Option<Position> {
    loop {
        if let Some(p) = map.get(path) {
            return Some(*p);
        }
        if let Some(i) = path.rfind(['.', '[']) {
            path = &path[..i];
        } else {
            return map.get("").copied();
        }
    }
}
fn diagnostic(
    severity: Severity,
    code: &str,
    path: &str,
    message: impl Into<String>,
    file: &Path,
    positions: &BTreeMap<String, Position>,
) -> Diagnostic {
    let pos = nearest(path, positions);
    Diagnostic {
        severity,
        code: code.into(),
        path: path.into(),
        file: file.to_path_buf(),
        line: pos.map(|p| p.line),
        column: pos.map(|p| p.column),
        message: message.into(),
        expected: None,
        help: None,
    }
}
const ROOT: &[&str] = &[
    "version",
    "id",
    "name",
    "description",
    "kind",
    "tags",
    "enabled",
    "repo_url",
    "template",
    "type",
    "domain",
    "root",
    "upstream_url",
    "base_url",
    "health_endpoint",
    "run",
    "setup",
    "worktree",
    "deploy",
    "depends_on",
    "features",
    "routing",
    "docs",
    "docs_auto",
    "endpoints",
    "api_auto",
];
fn fields(parent: &str) -> Option<&'static [&'static str]> {
    match parent {
        "" => Some(ROOT),
        "run" => Some(&["cmd", "cwd", "env", "pre_start", "post_stop"]),
        "worktree" => Some(&["copy", "link", "env"]),
        "deploy" => Some(&[
            "start", "stop", "restart", "logs", "status", "install", "env_file",
        ]),
        "features" => Some(&["mailpit", "auto_start", "auto_open_browser"]),
        "routing" => Some(&[
            "aliases",
            "paths",
            "websocket",
            "triggers",
            "risk",
            "requires_confirmation",
            "max_concurrent",
            "agents",
        ]),
        "docs_auto" => Some(&["enabled", "patterns", "ignore", "max_files", "max_bytes"]),
        "api_auto" => Some(&["enabled", "sources", "candidates", "timeout_ms", "max_endpoints"]),
        p if p.starts_with("endpoints[") && !p.contains('.') => Some(&[
            "method",
            "path",
            "description",
            "category",
            "risk",
            "body_hint",
            "params",
        ]),
        p if p.starts_with("docs[") && !p.contains('.') => Some(&["path", "kind"]),
        _ => None,
    }
}
fn edit_distance(a: &str, b: &str) -> usize {
    let mut d: Vec<_> = (0..=b.len()).collect();
    for (i, x) in a.bytes().enumerate() {
        let mut prev = d[0];
        d[0] = i + 1;
        for (j, y) in b.bytes().enumerate() {
            let old = d[j + 1];
            d[j + 1] = (old + 1).min(d[j] + 1).min(prev + usize::from(x != y));
            prev = old;
        }
    }
    d[b.len()]
}
fn inspect_keys(
    node: &yaml::Spanned<Node>,
    parent: &str,
    keys: &BTreeMap<String, Position>,
    file: &Path,
    ds: &mut Vec<Diagnostic>,
) {
    match &node.value {
        Node::Map(items) => {
            for (k, v) in items {
                let p = if parent.is_empty() {
                    k.value.clone()
                } else {
                    format!("{parent}.{}", k.value)
                };
                if let Some(known) = fields(parent) {
                    if !known.contains(&k.value.as_str()) && !k.value.starts_with("x-") {
                        let hint = known
                            .iter()
                            .min_by_key(|n| edit_distance(&k.value, n))
                            .filter(|n| edit_distance(&k.value, n) <= 2)
                            .map(|n| format!("; did you mean `{n}`?"))
                            .unwrap_or_default();
                        ds.push(diagnostic(
                            Severity::Warning,
                            "unknown_key",
                            &p,
                            format!("unknown key (ignored){hint}"),
                            file,
                            keys,
                        ));
                    }
                }
                inspect_keys(v, &p, keys, file, ds);
            }
        }
        Node::Seq(items) => {
            for (i, v) in items.iter().enumerate() {
                inspect_keys(v, &format!("{parent}[{i}]"), keys, file, ds);
            }
        }
        _ => {}
    }
}
fn bad_rel(s: &str, allow_root_slash: bool) -> bool {
    let s = if allow_root_slash {
        s.strip_prefix('/').unwrap_or(s)
    } else {
        s
    };
    Path::new(s).components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) || s.split('/').any(|p| p == ".git")
}
fn valid_env_key(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
fn sem_validate(raw: &RawManifest, file: &Path, pos: &BTreeMap<String, Position>, ds: &mut Vec<Diagnostic>) {
    let mut add =
        |sev, code: &str, path: &str, msg: String| ds.push(diagnostic(sev, code, path, msg, file, pos));
    if raw.version.is_some_and(|x| x != 1) {
        add(
            Severity::Error,
            "bad_value",
            "version",
            "only version 1 is supported".into(),
        );
    }
    if let Some(id) = &raw.id {
        if let Ok(normal) = ProjectId::from_name(id) {
            if normal.as_str() != id {
                add(
                    Severity::Warning,
                    "id_normalized",
                    "id",
                    format!("id normalized to {normal}"),
                );
            }
        }
    }
    for (path, value, valid) in [
        (
            "kind",
            raw.kind.as_deref(),
            &["project", "service", "capability", "website", "tool"][..],
        ),
        (
            "type",
            raw.serve.as_deref(),
            &["proxy", "php", "static", "unknown"][..],
        ),
    ] {
        if let Some(v) = value {
            if !valid.contains(&v) {
                add(
                    Severity::Warning,
                    "unknown_value",
                    path,
                    format!("unknown value {v:?}; using default"),
                );
            }
        }
    }
    for (path, value) in [
        ("root", raw.root.as_deref()),
        ("run.cwd", raw.run.as_ref().and_then(|x| x.cwd.as_deref())),
        (
            "deploy.env_file",
            raw.deploy.as_ref().and_then(|x| x.env_file.as_deref()),
        ),
    ] {
        if let Some(v) = value {
            if bad_rel(v, false) {
                add(
                    Severity::Error,
                    "bad_path",
                    path,
                    "path must stay inside the checkout".into(),
                );
            }
        }
    }
    if let Some(docs) = &raw.docs {
        for (i, d) in docs.iter().enumerate() {
            if bad_rel(d.path(), false) {
                add(
                    Severity::Error,
                    "bad_path",
                    &format!("docs[{i}]"),
                    "document path must stay inside the checkout".into(),
                );
            }
        }
    }
    for (path, value) in [
        ("upstream_url", raw.upstream_url.as_deref()),
        ("base_url", raw.base_url.as_deref()),
    ] {
        if let Some(v) = value.filter(|v| !v.trim().is_empty()) {
            if !url::Url::parse(v).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
            {
                add(
                    Severity::Error,
                    "bad_value",
                    path,
                    "expected an HTTP(S) URL".into(),
                );
            }
        }
    }
    if raw
        .health_endpoint
        .as_deref()
        .is_some_and(|s| !s.starts_with('/'))
    {
        add(
            Severity::Error,
            "bad_value",
            "health_endpoint",
            "expected a path beginning with /".into(),
        );
    }
    if let Some(domain) = &raw.domain {
        if domain.is_empty()
            || domain.len() > 253
            || domain.split('.').any(|s| {
                s.is_empty()
                    || s.len() > 63
                    || s.starts_with('-')
                    || s.ends_with('-')
                    || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            add(
                Severity::Error,
                "bad_value",
                "domain",
                "expected a hostname".into(),
            );
        }
    }
    if let Some(w) = &raw.worktree {
        for (key, list) in [("copy", &w.copy), ("link", &w.link)] {
            if let Some(list) = list {
                for (i, s) in list.iter().enumerate() {
                    let p = format!("worktree.{key}[{i}]");
                    if bad_rel(s, true) || s.starts_with("//") {
                        add(
                            Severity::Error,
                            "bad_path",
                            &p,
                            "glob must remain within the repository and exclude .git".into(),
                        );
                    } else if globset::GlobBuilder::new(s.trim_start_matches('/'))
                        .literal_separator(true)
                        .build()
                        .is_err()
                    {
                        add(Severity::Error, "bad_glob", &p, "invalid glob".into());
                    }
                }
            }
        }
    }
    if let Some(setup) = &raw.setup {
        for (i, s) in setup.iter().enumerate() {
            if s.trim().is_empty() {
                add(
                    Severity::Error,
                    "bad_value",
                    &format!("setup[{i}]"),
                    "setup command must not be blank".into(),
                );
            }
        }
    }
    for (section, env) in [
        ("run.env", raw.run.as_ref().and_then(|r| r.env.as_ref())),
        ("worktree.env", raw.worktree.as_ref().and_then(|w| w.env.as_ref())),
    ] {
        if let Some(env) = env {
            for (k, v) in env {
                if !valid_env_key(k) {
                    add(
                        Severity::Error,
                        "bad_value",
                        &format!("{section}.{k}"),
                        "invalid environment variable name".into(),
                    );
                }
                if (k == "PORT" || k == "HOST") && !TemplateString::parse(v).0.mentions(k) {
                    add(
                        Severity::Warning,
                        "reserved_env",
                        &format!("{section}.{k}"),
                        format!("{k} overrides Lantern's assigned value"),
                    );
                }
            }
        }
    }
    if let Some(run) = &raw.run {
        for (k, v) in [("pre_start", &run.pre_start), ("post_stop", &run.post_stop)] {
            if v.is_some() {
                add(
                    Severity::Warning,
                    "ignored_key",
                    &format!("run.{k}"),
                    "this key is ignored".into(),
                );
            }
        }
    }
    if raw.deploy.as_ref().is_some_and(|d| d.install.is_some()) {
        add(
            Severity::Warning,
            "deprecated_key",
            "deploy.install",
            "install is ignored; use setup".into(),
        );
    }
    if let Some(r) = &raw.routing {
        if r.max_concurrent == Some(0) {
            add(
                Severity::Error,
                "bad_value",
                "routing.max_concurrent",
                "must be at least 1".into(),
            );
        }
        if r.risk
            .as_deref()
            .is_some_and(|s| !["low", "medium", "high"].contains(&s))
        {
            add(
                Severity::Warning,
                "unknown_value",
                "routing.risk",
                "unknown risk is ignored".into(),
            );
        }
    }
    if let Some(endpoints) = &raw.endpoints {
        for (i, e) in endpoints.iter().enumerate() {
            if !e.path.starts_with('/') {
                add(
                    Severity::Error,
                    "bad_value",
                    &format!("endpoints[{i}].path"),
                    "endpoint path must start with /".into(),
                );
            }
            if e.method.as_deref().is_some_and(|m| {
                !["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
                    .contains(&m.to_ascii_uppercase().as_str())
            }) {
                add(
                    Severity::Error,
                    "bad_value",
                    &format!("endpoints[{i}].method"),
                    "unsupported HTTP method".into(),
                );
            }
            if e.risk
                .as_deref()
                .is_some_and(|s| !["low", "medium", "high"].contains(&s))
            {
                add(
                    Severity::Warning,
                    "unknown_value",
                    &format!("endpoints[{i}].risk"),
                    "unknown risk is ignored".into(),
                );
            }
        }
    }
    let declared: BTreeSet<String> = raw
        .run
        .as_ref()
        .and_then(|x| x.env.as_ref())
        .into_iter()
        .chain(raw.worktree.as_ref().and_then(|x| x.env.as_ref()))
        .flat_map(|m| m.keys().cloned())
        .collect();
    let mut templates: Vec<(String, &str, bool)> = Vec::new();
    if let Some(run) = &raw.run {
        for (k, s, shell) in [
            ("cmd", run.cmd.as_deref(), true),
            ("cwd", run.cwd.as_deref(), false),
            ("pre_start", run.pre_start.as_deref(), true),
            ("post_stop", run.post_stop.as_deref(), true),
        ] {
            if let Some(s) = s {
                templates.push((format!("run.{k}"), s, shell));
            }
        }
        if let Some(env) = &run.env {
            for (k, v) in env {
                templates.push((format!("run.env.{k}"), v, false));
            }
        }
    }
    if let Some(w) = &raw.worktree {
        if let Some(env) = &w.env {
            for (k, v) in env {
                templates.push((format!("worktree.env.{k}"), v, false));
            }
        }
    }
    if let Some(setup) = &raw.setup {
        for (i, s) in setup.iter().enumerate() {
            templates.push((format!("setup[{i}]"), s, true));
        }
    }
    if let Some(d) = &raw.deploy {
        for (k, v) in [
            ("start", &d.start),
            ("stop", &d.stop),
            ("restart", &d.restart),
            ("logs", &d.logs),
            ("status", &d.status),
            ("install", &d.install),
        ] {
            if let Some(s) = v {
                templates.push((format!("deploy.{k}"), s, true));
            }
        }
    }
    for (path, text, shell) in templates {
        for lint in TemplateString::parse_with_env(text, &declared).1 {
            if shell || lint.code != "var_in_single_quotes" {
                add(Severity::Warning, &lint.code, &path, lint.message);
            }
        }
    }
    if raw.serve.as_deref() == Some("static") && raw.run.as_ref().and_then(|r| r.cmd.as_ref()).is_some() {
        add(
            Severity::Warning,
            "key_not_applicable",
            "run.cmd",
            "static routes do not run a command".into(),
        );
    }
    if raw.serve.as_deref() == Some("proxy")
        && raw.upstream_url.is_none()
        && raw.deploy.as_ref().and_then(|d| d.start.as_ref()).is_none()
    {
        if let Some(run) = &raw.run {
            if let Some(cmd) = &run.cmd {
                if !TemplateString::parse(cmd).0.mentions("PORT")
                    && !run
                        .env
                        .as_ref()
                        .is_some_and(|env| env.values().any(|v| TemplateString::parse(v).0.mentions("PORT")))
                {
                    add(
                        Severity::Warning,
                        "port_not_referenced",
                        "run.cmd",
                        "command does not reference the assigned port".into(),
                    );
                }
            }
        }
    }
}

/// Parse a manifest and retain independent valid top-level keys after errors.
pub fn parse_manifest(src: &str, file: impl AsRef<Path>) -> ParsedManifest {
    let file = file.as_ref();
    let mut diagnostics = Vec::new();
    let tree = match yaml::parse_to_tree(src) {
        Ok(t) => t,
        Err(e) => {
            let p = e.position;
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: e.code.into(),
                path: String::new(),
                file: file.into(),
                line: p.map(|p| p.line),
                column: p.map(|p| p.column),
                message: e.message,
                expected: None,
                help: None,
            });
            return ParsedManifest {
                raw: RawManifest::default(),
                diagnostics,
                usable: false,
                file: file.into(),
            };
        }
    };
    let mut pos = BTreeMap::new();
    let mut keys = BTreeMap::new();
    index(&tree, "", &mut pos, &mut keys);
    if matches!(tree.value, Node::Null) {
        return ParsedManifest {
            raw: RawManifest::default(),
            diagnostics,
            usable: true,
            file: file.into(),
        };
    }
    let Node::Map(items) = &tree.value else {
        diagnostics.push(diagnostic(
            Severity::Error,
            "not_a_mapping",
            "",
            "lantern.yaml must contain a YAML mapping",
            file,
            &pos,
        ));
        return ParsedManifest {
            raw: RawManifest::default(),
            diagnostics,
            usable: false,
            file: file.into(),
        };
    };
    inspect_keys(&tree, "", &keys, file, &mut diagnostics);
    let mut good = serde_json::Map::new();
    for (key, value) in items {
        if !ROOT.contains(&key.value.as_str()) {
            continue;
        }
        let mut m = serde_json::Map::new();
        m.insert(key.value.clone(), value.value.to_json());
        let test = serde_json::Value::Object(m.clone());
        match serde_path_to_error::deserialize::<_, RawManifest>(test.into_deserializer()) {
            Ok(_) => {
                good.extend(m);
            }
            Err(e) => {
                let path = e.path().to_string();
                let path = if path == "." { key.value.clone() } else { path };
                let message = e.inner().to_string();
                let code = if message.contains("missing field") {
                    "missing_field"
                } else {
                    "wrong_type"
                };
                diagnostics.push(diagnostic(Severity::Error, code, &path, message, file, &pos));
            }
        }
    }
    let raw = serde_json::from_value::<RawManifest>(serde_json::Value::Object(good)).unwrap_or_default();
    sem_validate(&raw, file, &pos, &mut diagnostics);
    let usable = !diagnostics.iter().any(|d| d.severity == Severity::Error);
    ParsedManifest {
        raw,
        diagnostics,
        usable,
        file: file.into(),
    }
}

/// Find a manifest, preferring the .yaml spelling.
pub fn find_manifest(dir: &Path) -> Option<PathBuf> {
    ["lantern.yaml", "lantern.yml"]
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}
/// Read a manifest from a directory, if present.
pub fn load(dir: &Path) -> std::io::Result<Option<ParsedManifest>> {
    let Some(path) = find_manifest(dir) else {
        return Ok(None);
    };
    let bytes = std::fs::read(&path)?;
    let mut parsed = match std::str::from_utf8(&bytes) {
        Ok(src) => parse_manifest(src, &path),
        Err(_) => ParsedManifest {
            raw: RawManifest::default(),
            usable: false,
            file: path.clone(),
            diagnostics: vec![Diagnostic {
                severity: Severity::Error,
                code: "not_utf8".into(),
                path: String::new(),
                file: path.clone(),
                line: None,
                column: None,
                message: "manifest is not UTF-8".into(),
                expected: None,
                help: None,
            }],
        },
    };
    if dir.join("lantern.yaml").is_file() && dir.join("lantern.yml").is_file() {
        parsed.diagnostics.push(diagnostic(
            Severity::Warning,
            "both_manifest_files",
            "",
            "using lantern.yaml",
            &path,
            &BTreeMap::new(),
        ));
    }
    Ok(Some(parsed))
}

/// Where a resolved value originated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Override,
    CheckoutManifest,
    PrimaryManifest,
    Template(String),
    Detected(String),
    Default,
}
/// Selected source per top-level key and the manifest files read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub files: Vec<PathBuf>,
    pub keys: BTreeMap<String, Layer>,
}
/// Resolved run configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRun {
    pub cmd: Option<TemplateString>,
    pub cwd: TemplateString,
    pub env: BTreeMap<String, TemplateString>,
}
/// Worktree materialization instructions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorktreeSpec {
    pub copy: Vec<String>,
    pub link: Vec<String>,
    pub env: BTreeMap<String, TemplateString>,
}
/// Resolved shell-based operational overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeploySpec {
    pub start: Option<TemplateString>,
    pub stop: Option<TemplateString>,
    pub restart: Option<TemplateString>,
    pub logs: Option<TemplateString>,
    pub status: Option<TemplateString>,
    pub env_file: Option<String>,
}
/// An executable template together with whether it is built in.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateDefinition {
    pub manifest: RawManifest,
    pub builtin: bool,
}
/// Provider for built-in and user templates, with user templates taking precedence.
pub trait TemplateLookup {
    fn get(&self, name: &str) -> Option<TemplateDefinition>;
}
/// Detection values consumed by manifest resolution.
pub trait DetectionInfo {
    fn framework_name(&self) -> &str;
    fn template_name(&self) -> Option<&str>;
    fn rewrite_builtin(&self, cmd: &str) -> String;
    /// Detected docroot for PHP projects.
    fn php_root(&self) -> Option<&str> {
        None
    }
}
/// An effective manifest for one checkout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedManifest {
    pub id: ProjectId,
    pub name: String,
    pub description: Option<String>,
    pub kind: ProjectKind,
    pub tags: Vec<String>,
    pub enabled: bool,
    pub repo_url: Option<String>,
    pub serve: ServeMode,
    pub domain: Option<String>,
    pub root: Option<String>,
    pub upstream_url: Option<String>,
    pub base_url: Option<String>,
    pub health_endpoint: Option<String>,
    pub run: ResolvedRun,
    pub setup: Option<Vec<TemplateString>>,
    pub worktree: WorktreeSpec,
    pub deploy: DeploySpec,
    pub depends_on: Vec<String>,
    pub features: FeaturesSection,
    pub routing: RoutingSection,
    pub docs: Vec<DocEntry>,
    pub endpoints: Vec<EndpointEntry>,
    pub docs_auto: Option<DocsAuto>,
    pub api_auto: Option<ApiAuto>,
    pub provenance: Provenance,
    pub diagnostics: Vec<Diagnostic>,
    pub usable: bool,
}
/// Inputs for pure, per-checkout resolution.
pub struct ResolveInput<'a> {
    pub dir_name: &'a str,
    pub is_primary: bool,
    pub primary: Option<&'a ParsedManifest>,
    pub checkout: Option<&'a ParsedManifest>,
    pub overrides: &'a RawManifest,
    pub templates: &'a dyn TemplateLookup,
    pub detection: &'a dyn DetectionInfo,
    pub tld: &'a str,
    pub default_template: &'a str,
}
impl std::fmt::Debug for ResolveInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolveInput")
            .field("dir_name", &self.dir_name)
            .field("is_primary", &self.is_primary)
            .finish_non_exhaustive()
    }
}
fn prune_nulls(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(m) = value {
        m.retain(|_, v| !v.is_null());
        for v in m.values_mut() {
            prune_nulls(v);
        }
    }
}
fn sparse(raw: &RawManifest) -> serde_json::Value {
    let mut v = serde_json::to_value(raw).expect("raw manifest serializes");
    prune_nulls(&mut v);
    v
}
fn merge(base: &mut serde_json::Value, incoming: serde_json::Value) {
    match (base, incoming) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}
fn has_way(raw: &RawManifest) -> bool {
    raw.run
        .as_ref()
        .and_then(|r| r.cmd.as_ref())
        .is_some_and(|s| !s.trim().is_empty())
        || raw.upstream_url.as_ref().is_some_and(|s| !s.trim().is_empty())
        || raw.deploy.as_ref().and_then(|d| d.start.as_ref()).is_some()
        || matches!(raw.serve.as_deref(), Some("php" | "static"))
}
fn t(s: String) -> TemplateString {
    TemplateString::parse(&s).0
}
fn env_t(env: Option<BTreeMap<String, String>>) -> BTreeMap<String, TemplateString> {
    env.unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, t(v)))
        .collect()
}
fn nonblank(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}
/// Resolve override, checkout, primary fallback, template and detection layers.
pub fn resolve(input: ResolveInput<'_>) -> ResolvedManifest {
    let mut diagnostics = Vec::new();
    let mut provenance = Provenance::default();
    if let Some(primary) = input.primary {
        provenance.files.push(primary.file.clone());
        diagnostics.extend(primary.diagnostics.clone());
    }
    if !input.is_primary {
        if let Some(checkout) = input.checkout {
            provenance.files.push(checkout.file.clone());
            diagnostics.extend(checkout.diagnostics.clone());
        }
    }
    let selected = if input.is_primary {
        input.primary
    } else {
        input.checkout.or(input.primary)
    };
    let mut source = selected
        .map(|p| sparse(&p.raw))
        .unwrap_or_else(|| serde_json::json!({}));
    let identity = [
        "id",
        "name",
        "description",
        "kind",
        "tags",
        "enabled",
        "repo_url",
        "domain",
    ];
    if !input.is_primary && input.checkout.is_some() {
        if let Some(obj) = source.as_object_mut() {
            for key in identity {
                if obj.remove(key).is_some() {
                    let p = input.checkout.expect("checked");
                    diagnostics.push(Diagnostic {
                        severity: Severity::Info,
                        code: "identity_key_ignored".into(),
                        path: key.into(),
                        file: p.file.clone(),
                        line: None,
                        column: None,
                        message: "project identity comes from the primary manifest".into(),
                        expected: None,
                        help: None,
                    });
                }
            }
        }
        if let Some(primary) = input.primary {
            let p = sparse(&primary.raw);
            for key in identity {
                if let Some(v) = p.get(key) {
                    source
                        .as_object_mut()
                        .expect("object")
                        .insert(key.into(), v.clone());
                }
            }
        }
    }
    let base_source = source.clone();
    merge(&mut source, sparse(input.overrides));
    let high: RawManifest = serde_json::from_value(source.clone()).unwrap_or_default();
    let explicit = high.template.as_deref();
    let desired = explicit.or_else(|| {
        if !has_way(&high) {
            input
                .detection
                .template_name()
                .or_else(|| (!input.default_template.is_empty()).then_some(input.default_template))
        } else {
            None
        }
    });
    let mut template = None;
    if let Some(name) = desired {
        if let Some(mut def) = input.templates.get(name) {
            def.manifest.id = None;
            def.manifest.name = None;
            def.manifest.description = None;
            def.manifest.domain = None;
            def.manifest.repo_url = None;
            def.manifest.enabled = None;
            if def.builtin {
                if name == "php" && input.detection.framework_name() == "php" {
                    if let Some(root) = input.detection.php_root() {
                        def.manifest.root = Some(root.to_string());
                    }
                }
                if let Some(run) = &mut def.manifest.run {
                    if let Some(cmd) = &mut run.cmd {
                        *cmd = input.detection.rewrite_builtin(cmd);
                    }
                }
            }
            template = Some((name.to_string(), def));
        } else {
            diagnostics.push(Diagnostic {
                severity: Severity::Warning,
                code: "unknown_template".into(),
                path: "template".into(),
                file: selected.map(|p| p.file.clone()).unwrap_or_default(),
                line: None,
                column: None,
                message: format!("unknown template {name:?}"),
                expected: None,
                help: None,
            });
            if explicit.is_some() {
                if let Some(fallback) = input.detection.template_name() {
                    template = input.templates.get(fallback).map(|d| (fallback.to_string(), d));
                }
            }
        }
    }
    let mut merged = serde_json::json!({});
    if let Some((name, def)) = &template {
        let val = sparse(&def.manifest);
        if let Some(obj) = val.as_object() {
            for k in obj.keys() {
                provenance.keys.insert(k.clone(), Layer::Template(name.clone()));
            }
        }
        merge(&mut merged, val);
    }
    if let Some(obj) = base_source.as_object() {
        for k in obj.keys() {
            let layer = if !input.is_primary && input.checkout.is_some() && !identity.contains(&k.as_str()) {
                Layer::CheckoutManifest
            } else {
                Layer::PrimaryManifest
            };
            provenance.keys.insert(k.clone(), layer);
        }
    }
    merge(&mut merged, base_source);
    let overrides = sparse(input.overrides);
    if let Some(obj) = overrides.as_object() {
        for k in obj.keys() {
            provenance.keys.insert(k.clone(), Layer::Override);
        }
    }
    merge(&mut merged, overrides);
    let mut raw: RawManifest = serde_json::from_value(merged).unwrap_or_default();
    let requested_id = raw.id.as_deref().unwrap_or(input.dir_name);
    if let Err(err) = ProjectId::from_name(requested_id) {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: err.code().into(),
            path: "id".into(),
            file: selected.map(|p| p.file.clone()).unwrap_or_default(),
            line: None,
            column: None,
            message: err.to_string(),
            expected: None,
            help: None,
        });
    }
    let id = raw
        .id
        .as_deref()
        .and_then(|s| ProjectId::from_name(s).ok())
        .or_else(|| ProjectId::from_name(input.dir_name).ok())
        .unwrap_or_else(|| ProjectId::parse("project").expect("project is valid"));
    let kind = match raw.kind.as_deref() {
        Some("service") => ProjectKind::Service,
        Some("tool") => ProjectKind::Tool,
        Some("capability") => ProjectKind::Capability,
        Some("website") => ProjectKind::Website,
        _ => ProjectKind::Project,
    };
    let run = raw.run.take().unwrap_or_default();
    let work = raw.worktree.take().unwrap_or_default();
    let dep = raw.deploy.take().unwrap_or_default();
    let serve = match raw.serve.as_deref() {
        Some("proxy") => ServeMode::Proxy,
        Some("php") => ServeMode::Php,
        Some("static") => ServeMode::Static,
        _ => {
            if run.cmd.as_ref().is_some_and(|s| !s.trim().is_empty())
                || raw.upstream_url.as_ref().is_some_and(|s| !s.trim().is_empty())
            {
                ServeMode::Proxy
            } else {
                ServeMode::Unknown
            }
        }
    };
    let domain = raw.domain.take().map(|d| {
        if d.contains('.') {
            d
        } else {
            format!("{}.{}", d, input.tld.trim_start_matches('.'))
        }
    });
    if let Some(d) = &domain {
        if !d.ends_with(&format!(".{}", input.tld.trim_start_matches('.'))) {
            diagnostics.push(Diagnostic {
                severity: Severity::Warning,
                code: "domain_outside_tld".into(),
                path: "domain".into(),
                file: selected.map(|p| p.file.clone()).unwrap_or_default(),
                line: None,
                column: None,
                message: "domain is outside the configured TLD".into(),
                expected: None,
                help: None,
            });
        }
    }
    let root = raw.root.take().or_else(|| match serve {
        ServeMode::Php => Some("public".into()),
        ServeMode::Static => Some(".".into()),
        _ => None,
    });
    if let Some(r) = &mut raw.routing {
        if r.risk
            .as_deref()
            .is_some_and(|r| !["low", "medium", "high"].contains(&r))
        {
            r.risk = None;
        }
    }
    let mut endpoints = raw.endpoints.take().unwrap_or_default();
    for e in &mut endpoints {
        e.method = Some(e.method.as_deref().unwrap_or("GET").to_ascii_uppercase());
        if e.risk
            .as_deref()
            .is_some_and(|r| !["low", "medium", "high"].contains(&r))
        {
            e.risk = None;
        }
    }
    let usable = !diagnostics.iter().any(|d| d.severity == Severity::Error);
    ResolvedManifest {
        name: raw.name.take().unwrap_or_else(|| id.to_string()),
        id,
        description: raw.description,
        kind,
        tags: raw.tags.unwrap_or_default(),
        enabled: raw.enabled.unwrap_or(true),
        repo_url: raw.repo_url,
        serve,
        domain,
        root,
        upstream_url: nonblank(raw.upstream_url),
        base_url: nonblank(raw.base_url),
        health_endpoint: raw.health_endpoint,
        run: ResolvedRun {
            cmd: nonblank(run.cmd).map(t),
            cwd: t(run.cwd.unwrap_or_else(|| ".".into())),
            env: env_t(run.env),
        },
        setup: raw.setup.map(|v| v.into_iter().map(t).collect()),
        worktree: WorktreeSpec {
            copy: work.copy.unwrap_or_default(),
            link: work.link.unwrap_or_default(),
            env: env_t(work.env),
        },
        deploy: DeploySpec {
            start: dep.start.map(t),
            stop: dep.stop.map(t),
            restart: dep.restart.map(t),
            logs: dep.logs.map(t),
            status: dep.status.map(t),
            env_file: dep.env_file,
        },
        depends_on: raw.depends_on.unwrap_or_default(),
        features: raw.features.unwrap_or_default(),
        routing: raw.routing.unwrap_or_default(),
        docs: raw.docs.unwrap_or_default(),
        endpoints,
        docs_auto: raw.docs_auto,
        api_auto: raw.api_auto,
        provenance,
        diagnostics,
        usable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoTemplates;
    impl TemplateLookup for NoTemplates {
        fn get(&self, _: &str) -> Option<TemplateDefinition> {
            None
        }
    }
    struct NoDetection;
    impl DetectionInfo for NoDetection {
        fn framework_name(&self) -> &str {
            "unknown"
        }
        fn template_name(&self) -> Option<&str> {
            None
        }
        fn rewrite_builtin(&self, s: &str) -> String {
            s.to_owned()
        }
    }
    #[test]
    fn v1_fixtures_load_without_errors() {
        for text in [
            include_str!("../../tests/fixtures/v1/lantern.yaml"),
            include_str!("../../tests/fixtures/v1/lantern.yaml.example"),
        ] {
            let p = parse_manifest(text, "lantern.yaml");
            assert!(p.usable, "{:?}", p.diagnostics);
        }
    }
    #[test]
    fn nested_type_error_is_located_and_other_fields_survive() {
        let p = parse_manifest(
            "name: Example\nrun:\n  cmd: [oops, list]\ntags: [ok]\n",
            "lantern.yaml",
        );
        let e = p
            .diagnostics
            .iter()
            .find(|e| e.severity == Severity::Error)
            .unwrap();
        assert_eq!(
            (e.file.to_str().unwrap(), e.line, e.column, e.path.as_str()),
            ("lantern.yaml", Some(3), Some(8), "run.cmd")
        );
        assert_eq!(p.raw.name.as_deref(), Some("Example"));
        assert_eq!(p.raw.tags, Some(vec!["ok".into()]));
    }
    #[test]
    fn checkout_identity_and_setup_absence_are_preserved() {
        let primary = parse_manifest(
            "id: app\nname: Primary\nrun: {cmd: primary, env: {A: a, B: b}}\nsetup: [install]\n",
            "primary.yaml",
        );
        let checkout = parse_manifest(
            "id: other\nrun: {cmd: branch, env: {B: branch}}\nsetup: []\n",
            "branch.yaml",
        );
        let overrides = RawManifest::default();
        let r = resolve(ResolveInput {
            dir_name: "app",
            is_primary: false,
            primary: Some(&primary),
            checkout: Some(&checkout),
            overrides: &overrides,
            templates: &NoTemplates,
            detection: &NoDetection,
            tld: "glow",
            default_template: "",
        });
        assert_eq!(r.id.as_str(), "app");
        assert_eq!(r.name, "Primary");
        assert_eq!(r.run.cmd.unwrap().source(), "branch");
        assert!(r.run.env.get("A").is_none());
        assert_eq!(r.setup, Some(vec![]));
        assert!(r.diagnostics.iter().any(|d| d.code == "identity_key_ignored"));
    }
    #[test]
    fn worktree_paths_and_unknown_nested_keys() {
        let p = parse_manifest(
            "worktree:\n  copy: [/.env, ../secret, '[oops']\n  typo: true\n",
            "lantern.yaml",
        );
        assert!(p.diagnostics.iter().any(|d| d.code == "bad_path"));
        assert!(p.diagnostics.iter().any(|d| d.code == "bad_glob"));
        assert!(p.diagnostics.iter().any(|d| d.code == "unknown_key"));
    }
}

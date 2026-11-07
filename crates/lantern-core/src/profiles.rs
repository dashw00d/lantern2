//! Profile parsing and embedded built-ins; activation is owned by the registry.
use crate::{
    ids::InstanceId,
    manifest::{Diagnostic, Severity},
    template::TemplateString,
    yaml::{self, Node},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Profile services, start targets and environment overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
#[derive(Default)]
pub struct Profile {
    pub description: Option<String>,
    pub services: Vec<String>,
    #[serde(alias = "auto_start_projects")]
    pub auto_start: Vec<String>,
    #[schemars(with = "BTreeMap<String, ProfileScalar>")]
    pub env: BTreeMap<String, String>,
    pub port_range: Option<[u16; 2]>,
    pub stop_others: bool,
}
#[allow(dead_code)]
#[derive(JsonSchema)]
#[serde(untagged)]
enum ProfileScalar {
    Text(String),
    Number(f64),
    Boolean(bool),
}
/// A parsed profile and diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedProfile {
    pub profile: Profile,
    pub diagnostics: Vec<Diagnostic>,
    pub usable: bool,
}
fn diag(
    severity: Severity,
    code: &str,
    path: &str,
    file: &Path,
    position: Option<yaml::Position>,
    message: impl Into<String>,
) -> Diagnostic {
    Diagnostic {
        severity,
        code: code.into(),
        path: path.into(),
        file: file.into(),
        line: position.map(|p| p.line),
        column: position.map(|p| p.column),
        message: message.into(),
        expected: None,
        help: None,
    }
}
/// Validate a profile file stem.
pub fn valid_name(name: &str) -> bool {
    let mut c = name.chars();
    name.len() <= 48
        && c.next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}
/// Parse and validate a profile; unknown services are warnings.
pub fn parse_profile(src: &str, file: impl AsRef<Path>) -> ParsedProfile {
    let file = file.as_ref();
    let tree = match yaml::parse_to_tree(src) {
        Ok(t) => t,
        Err(e) => {
            return ParsedProfile {
                profile: Profile::default(),
                diagnostics: vec![diag(Severity::Error, e.code, "", file, e.position, e.message)],
                usable: false,
            };
        }
    };
    let mut diagnostics = Vec::new();
    let Node::Map(items) = &tree.value else {
        return ParsedProfile {
            profile: Profile::default(),
            diagnostics: vec![diag(
                Severity::Error,
                "not_a_mapping",
                "",
                file,
                Some(tree.position),
                "profile must be a mapping",
            )],
            usable: false,
        };
    };
    let position = |path: &str| {
        items
            .iter()
            .find(|(k, _)| k.value == path)
            .map(|(_, v)| v.position)
            .or(Some(tree.position))
    };
    for (k, _) in items {
        if ![
            "description",
            "services",
            "auto_start",
            "auto_start_projects",
            "env",
            "port_range",
            "stop_others",
        ]
        .contains(&k.value.as_str())
            && !k.value.starts_with("x-")
        {
            diagnostics.push(diag(
                Severity::Warning,
                "unknown_key",
                &k.value,
                file,
                Some(k.position),
                "unknown key ignored",
            ));
        }
    }
    let mut value = tree.value.to_json();
    if let Some(env) = value.get_mut("env").and_then(serde_json::Value::as_object_mut) {
        for value in env.values_mut() {
            if value.is_number() || value.is_boolean() {
                *value = serde_json::Value::String(value.to_string());
            }
        }
    }
    let profile = match serde_path_to_error::deserialize::<_, Profile>(
        serde::de::IntoDeserializer::into_deserializer(value),
    ) {
        Ok(p) => p,
        Err(e) => {
            let p = e.path().to_string();
            diagnostics.push(diag(
                Severity::Error,
                "wrong_type",
                &p,
                file,
                position(p.split('.').next().unwrap_or("")),
                e.inner().to_string(),
            ));
            Profile::default()
        }
    };
    for s in &profile.services {
        if !["mailpit", "redis", "postgres", "php-fpm"].contains(&s.as_str()) {
            diagnostics.push(diag(
                Severity::Warning,
                "unknown_value",
                "services",
                file,
                position("services"),
                format!("unknown service {s}"),
            ));
        }
    }
    for target in &profile.auto_start {
        if target != "*" && InstanceId::parse(target).is_err() {
            diagnostics.push(diag(
                Severity::Error,
                "bad_value",
                "auto_start",
                file,
                position("auto_start"),
                format!("invalid instance id {target}"),
            ));
        }
    }
    for (key, value) in &profile.env {
        if ["PORT", "HOST"].contains(&key.as_str()) {
            diagnostics.push(diag(
                Severity::Error,
                "reserved_env",
                &format!("env.{key}"),
                file,
                position("env"),
                "profile must not override PORT or HOST",
            ));
        }
        let declared: BTreeSet<_> = profile.env.keys().cloned().collect();
        for lint in TemplateString::parse_with_env(value, &declared).1 {
            if lint.code != "var_in_single_quotes" {
                diagnostics.push(diag(
                    Severity::Warning,
                    &lint.code,
                    &format!("env.{key}"),
                    file,
                    position("env"),
                    lint.message,
                ));
            }
        }
    }
    if let Some([start, end]) = profile.port_range
        && (start < 1024 || start >= end || u32::from(end) - u32::from(start) + 1 < 16)
    {
        diagnostics.push(diag(
            Severity::Error,
            "bad_value",
            "port_range",
            file,
            position("port_range"),
            "invalid port range",
        ));
    }
    let usable = !diagnostics.iter().any(|d| d.severity == Severity::Error);
    ParsedProfile {
        profile,
        diagnostics,
        usable,
    }
}
/// Embedded profile names.
pub const BUILTIN_NAMES: &[&str] = &["light", "full_stack", "demo"];
/// Retrieve an embedded profile's YAML.
pub fn builtin_source(name: &str) -> Option<&'static str> {
    match name {
        "light" => Some(include_str!("../builtin/profiles/light.yaml")),
        "full_stack" => Some(include_str!("../builtin/profiles/full_stack.yaml")),
        "demo" => Some(include_str!("../builtin/profiles/demo.yaml")),
        _ => None,
    }
}
/// Parse a built-in profile.
pub fn builtin(name: &str) -> Option<Profile> {
    let p = parse_profile(builtin_source(name)?, format!("builtin/{name}.yaml"));
    p.usable.then_some(p.profile)
}

/// User profiles shadow embedded profiles of the same name.
#[derive(Debug, Clone, Default)]
pub struct Profiles {
    pub user: BTreeMap<String, Profile>,
}
impl Profiles {
    /// Find a profile, preferring a user file.
    pub fn get(&self, name: &str) -> Option<Profile> {
        self.user.get(name).cloned().or_else(|| builtin(name))
    }
    /// Read profile files and return their diagnostics.
    pub fn load_dir(dir: &Path) -> std::io::Result<(Self, Vec<Diagnostic>)> {
        use std::io::Read;
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Self::default(), vec![])),
            Err(e) => return Err(e),
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "yaml" || e == "yml"))
            .collect::<Vec<_>>();
        paths.sort_by_key(|p| {
            (
                p.file_stem().map(|s| s.to_os_string()),
                p.extension().map(|e| if e == "yaml" { 0 } else { 1 }),
            )
        });
        let mut result = Self::default();
        let mut diagnostics = Vec::new();
        for path in paths {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            if !valid_name(name) {
                diagnostics.push(diag(
                    Severity::Error,
                    "bad_value",
                    "name",
                    &path,
                    None,
                    "invalid profile filename",
                ));
                continue;
            }
            if result.user.contains_key(name) {
                continue;
            }
            let mut bytes = Vec::new();
            std::fs::File::open(&path)?
                .take((yaml::MAX_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > yaml::MAX_BYTES {
                diagnostics.push(diag(
                    Severity::Error,
                    "file_too_large",
                    "",
                    &path,
                    None,
                    "profile exceeds 1 MiB",
                ));
                continue;
            }
            let text = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(_) => {
                    diagnostics.push(diag(
                        Severity::Error,
                        "not_utf8",
                        "",
                        &path,
                        None,
                        "profile is not UTF-8",
                    ));
                    continue;
                }
            };
            let parsed = parse_profile(text, &path);
            diagnostics.extend(parsed.diagnostics);
            if parsed.usable {
                result.user.insert(name.to_owned(), parsed.profile);
            }
        }
        Ok((result, diagnostics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builtins_and_alias() {
        for n in BUILTIN_NAMES {
            assert!(builtin(n).is_some());
        }
        let d = builtin("demo").unwrap();
        assert_eq!(d.port_range, Some([29900, 29999]));
        assert_eq!(d.auto_start, ["*"]);
        let p = parse_profile("auto_start_projects: [app]\n", "profile.yaml");
        assert_eq!(p.profile.auto_start, ["app"]);
    }
    #[test]
    fn user_profiles_shadow_builtins() {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/profiles"));
        let (profiles, diagnostics) = Profiles::load_dir(dir).unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(profiles.get("light").unwrap().services, vec!["redis"]);
        assert_eq!(profiles.get("demo").unwrap().port_range, Some([29900, 29999]));
    }
    #[test]
    fn reserved_and_typo() {
        let p = parse_profile("env:\n  PORT: 8\n  DB: ${PROT}\n", "profile.yaml");
        assert!(!p.usable);
        assert!(p.diagnostics.iter().any(|d| d.code == "reserved_env"));
        assert!(p.diagnostics.iter().any(|d| d.code == "possible_var_typo"));
    }
}

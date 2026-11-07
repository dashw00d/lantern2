//! Cheap, read-only framework and package-manager detection.
use crate::manifest::{self, DetectionInfo, TemplateDefinition, TemplateLookup};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Detected framework, in detection priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    Laravel,
    Symfony,
    Php,
    NextJs,
    Nuxt,
    ReactRouter,
    Remix,
    Vite,
    FastApi,
    Django,
    Flask,
    Static,
    Unknown,
}
impl Framework {
    fn name(self) -> &'static str {
        match self {
            Self::Laravel => "laravel",
            Self::Symfony => "symfony",
            Self::Php => "php",
            Self::NextJs => "nextjs",
            Self::Nuxt => "nuxt",
            Self::ReactRouter => "react-router",
            Self::Remix => "remix",
            Self::Vite => "vite",
            Self::FastApi => "fastapi",
            Self::Django => "django",
            Self::Flask => "flask",
            Self::Static => "static",
            Self::Unknown => "unknown",
        }
    }
}
/// Strength of framework evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}
/// Node package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Bun,
    Pnpm,
    Yarn,
    Npm,
}
/// Python command runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonRunner {
    Uv,
    Poetry,
    Pipenv,
    Venv,
    None,
}
/// Detection output, including evidence for the GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub framework: Framework,
    pub confidence: Confidence,
    pub template: Option<&'static str>,
    pub package_manager: Option<PackageManager>,
    pub python_runner: Option<PythonRunner>,
    pub evidence: Vec<String>,
    pub fastapi_entrypoint: Option<String>,
    pub remix_vite: bool,
    pub php_root: Option<String>,
}
impl Default for Detection {
    fn default() -> Self {
        Self {
            framework: Framework::Unknown,
            confidence: Confidence::Low,
            template: None,
            package_manager: None,
            python_runner: None,
            evidence: vec![],
            fastapi_entrypoint: None,
            remix_vite: false,
            php_root: None,
        }
    }
}
fn read_bounded(path: &Path, limit: u64) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    fs::read_to_string(path).ok()
}
fn ancestors(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut current = Some(dir);
    for _ in 0..=4 {
        let Some(p) = current else { break };
        paths.push(p.to_path_buf());
        if p.join(".git").exists() {
            break;
        }
        current = p.parent();
    }
    paths
}
fn manager_name(s: &str) -> Option<PackageManager> {
    match s.split('@').next().unwrap_or_default() {
        "bun" => Some(PackageManager::Bun),
        "pnpm" => Some(PackageManager::Pnpm),
        "yarn" => Some(PackageManager::Yarn),
        "npm" => Some(PackageManager::Npm),
        _ => None,
    }
}
fn detect_pm(
    paths: &[PathBuf],
    package: &Option<serde_json::Value>,
    evidence: &mut Vec<String>,
) -> Option<PackageManager> {
    if let Some(name) = package
        .as_ref()
        .and_then(|v| v.get("packageManager"))
        .and_then(|v| v.as_str())
    {
        if let Some(pm) = manager_name(name) {
            evidence.push(format!("package.json packageManager={name}"));
            return Some(pm);
        }
    }
    for p in paths {
        for (name, pm) in [
            ("bun.lockb", PackageManager::Bun),
            ("bun.lock", PackageManager::Bun),
            ("pnpm-lock.yaml", PackageManager::Pnpm),
            ("yarn.lock", PackageManager::Yarn),
            ("package-lock.json", PackageManager::Npm),
        ] {
            if p.join(name).is_file() {
                evidence.push(p.join(name).display().to_string());
                return Some(pm);
            }
        }
    }
    if package.is_some() {
        evidence.push("package.json".into());
        Some(PackageManager::Npm)
    } else {
        None
    }
}
fn has_text(path: &Path, needle: &str) -> bool {
    read_bounded(path, 65536).is_some_and(|s| s.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()))
}
fn python_files(dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![dir.join("pyproject.toml"), dir.join("Pipfile")];
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("requirements") && name.ends_with(".txt") {
                v.push(e.path());
            }
        }
    }
    v.sort();
    v
}
fn fastapi_entry(dir: &Path) -> Option<String> {
    for file in ["app/main.py", "main.py", "src/main.py", "app.py", "api/main.py"] {
        if let Some(src) = read_bounded(&dir.join(file), 65536) {
            for line in src.lines() {
                if let Some((name, rest)) = line.split_once('=') {
                    let name = name.trim();
                    if !name.is_empty()
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        && rest.trim_start().starts_with("FastAPI(")
                    {
                        return Some(format!(
                            "{}:{name}",
                            file.trim_end_matches(".py").replace('/', ".")
                        ));
                    }
                }
            }
        }
    }
    None
}
/// Probe a directory without executing commands or following the network.
pub fn detect_dir(dir: &Path) -> Detection {
    let paths = ancestors(dir);
    let package = read_bounded(&dir.join("package.json"), 1024 * 1024)
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let mut d = Detection::default();
    d.package_manager = detect_pm(&paths, &package, &mut d.evidence);
    let dep = |key: &str| {
        package.as_ref().is_some_and(|p| {
            ["dependencies", "devDependencies"]
                .iter()
                .any(|part| p.get(part).and_then(|v| v.get(key)).is_some())
        })
    };
    let py = python_files(dir);
    let has_py = |name: &str| py.iter().any(|p| has_text(p, name));
    let python = if dir.join("uv.lock").is_file() {
        PythonRunner::Uv
    } else if dir.join("poetry.lock").is_file() {
        PythonRunner::Poetry
    } else if dir.join("Pipfile.lock").is_file() {
        PythonRunner::Pipenv
    } else if dir.join(".venv/bin").is_dir() {
        PythonRunner::Venv
    } else {
        PythonRunner::None
    };
    let public_php = dir.join("public/index.php").is_file();
    let vite_config = [
        "vite.config.js",
        "vite.config.ts",
        "vite.config.mjs",
        "vite.config.mts",
        "vite.config.cjs",
        "vite.config.cts",
    ]
    .iter()
    .any(|p| dir.join(p).is_file());
    let (framework, confidence, evidence) = if dir.join("artisan").is_file() && public_php {
        (Framework::Laravel, Confidence::High, "artisan + public/index.php")
    } else if dir.join("bin/console").is_file() && public_php {
        (
            Framework::Symfony,
            Confidence::High,
            "bin/console + public/index.php",
        )
    } else if public_php || dir.join("index.php").is_file() {
        d.php_root = Some(if public_php { "public" } else { "." }.into());
        (Framework::Php, Confidence::Medium, "index.php")
    } else if dep("next") {
        (
            Framework::NextJs,
            Confidence::High,
            "package.json dependency next",
        )
    } else if dep("nuxt") || dep("nuxt3") {
        (Framework::Nuxt, Confidence::High, "package.json dependency nuxt")
    } else if dep("@react-router/dev") {
        (
            Framework::ReactRouter,
            Confidence::High,
            "package.json dependency @react-router/dev",
        )
    } else if dep("@remix-run/dev") {
        d.remix_vite = vite_config;
        (
            Framework::Remix,
            if vite_config {
                Confidence::High
            } else {
                Confidence::Medium
            },
            "package.json dependency @remix-run/dev",
        )
    } else if dep("vite") {
        (
            Framework::Vite,
            Confidence::Medium,
            "package.json dependency vite",
        )
    } else if has_py("fastapi") {
        d.fastapi_entrypoint = fastapi_entry(dir);
        (
            Framework::FastApi,
            if d.fastapi_entrypoint.is_some() {
                Confidence::Medium
            } else {
                Confidence::Low
            },
            "Python dependency fastapi",
        )
    } else if dir.join("manage.py").is_file() {
        (
            Framework::Django,
            if has_text(&dir.join("manage.py"), "django") {
                Confidence::Medium
            } else {
                Confidence::Low
            },
            "manage.py",
        )
    } else if has_py("flask")
        || has_text(&dir.join("app.py"), "flask")
        || has_text(&dir.join("wsgi.py"), "flask")
    {
        (Framework::Flask, Confidence::Medium, "Flask source or dependency")
    } else if dir.join("index.html").is_file() {
        (Framework::Static, Confidence::Low, "index.html")
    } else {
        (Framework::Unknown, Confidence::Low, "")
    };
    d.framework = framework;
    d.confidence = confidence;
    if framework != Framework::Unknown {
        d.template = Some(framework.name());
        d.evidence.push(evidence.into());
    }
    if matches!(
        framework,
        Framework::FastApi | Framework::Django | Framework::Flask
    ) {
        d.python_runner = Some(python);
    }
    d
}
impl DetectionInfo for Detection {
    fn framework_name(&self) -> &str {
        self.framework.name()
    }
    fn template_name(&self) -> Option<&str> {
        self.template
    }
    fn php_root(&self) -> Option<&str> {
        self.php_root.as_deref()
    }
    fn rewrite_builtin(&self, cmd: &str) -> String {
        let mut cmd = cmd.to_owned();
        if self.framework == Framework::Remix && !self.remix_vite {
            cmd = cmd.replace(
                "remix vite:dev --host ${HOST} --port ${PORT}",
                "remix dev --port ${PORT}",
            );
        }
        if let Some(entry) = &self.fastapi_entrypoint {
            cmd = cmd.replace("app.main:app", entry);
        }
        if let Some(rest) = cmd.strip_prefix("npx ") {
            let prefix = match self.package_manager {
                Some(PackageManager::Pnpm) => "pnpm exec ",
                Some(PackageManager::Yarn) => "yarn ",
                Some(PackageManager::Bun) => "bunx ",
                _ => "npx ",
            };
            return format!("{prefix}{rest}");
        }
        if matches!(
            self.framework,
            Framework::FastApi | Framework::Django | Framework::Flask
        ) {
            let prefix = match self.python_runner {
                Some(PythonRunner::Uv) => "uv run ",
                Some(PythonRunner::Poetry) => "poetry run ",
                Some(PythonRunner::Pipenv) => "pipenv run ",
                Some(PythonRunner::Venv) => ".venv/bin/",
                _ => "",
            };
            return format!("{prefix}{cmd}");
        }
        cmd
    }
}
/// Names of all built-in template files.
pub const BUILTIN_NAMES: &[&str] = &[
    "nextjs",
    "nuxt",
    "remix",
    "react-router",
    "vite",
    "fastapi",
    "django",
    "flask",
    "laravel",
    "laravel-serve",
    "symfony",
    "php",
    "static",
];
/// Fetch an embedded template's YAML text.
pub fn builtin_source(name: &str) -> Option<&'static str> {
    match name {
        "nextjs" => Some(include_str!("../builtin/templates/nextjs.yaml")),
        "nuxt" => Some(include_str!("../builtin/templates/nuxt.yaml")),
        "remix" => Some(include_str!("../builtin/templates/remix.yaml")),
        "react-router" => Some(include_str!("../builtin/templates/react-router.yaml")),
        "vite" => Some(include_str!("../builtin/templates/vite.yaml")),
        "fastapi" => Some(include_str!("../builtin/templates/fastapi.yaml")),
        "django" => Some(include_str!("../builtin/templates/django.yaml")),
        "flask" => Some(include_str!("../builtin/templates/flask.yaml")),
        "laravel" => Some(include_str!("../builtin/templates/laravel.yaml")),
        "laravel-serve" => Some(include_str!("../builtin/templates/laravel-serve.yaml")),
        "symfony" => Some(include_str!("../builtin/templates/symfony.yaml")),
        "php" => Some(include_str!("../builtin/templates/php.yaml")),
        "static" => Some(include_str!("../builtin/templates/static.yaml")),
        _ => None,
    }
}
/// Lookup containing user templates which shadow builtins.
#[derive(Debug, Clone, Default)]
pub struct Templates {
    pub user: BTreeMap<String, manifest::raw::RawManifest>,
}
impl TemplateLookup for Templates {
    fn get(&self, name: &str) -> Option<TemplateDefinition> {
        if let Some(raw) = self.user.get(name) {
            return Some(TemplateDefinition {
                manifest: raw.clone(),
                builtin: false,
            });
        }
        let p = manifest::parse_manifest(builtin_source(name)?, format!("builtin/{name}.yaml"));
        if p.usable {
            Some(TemplateDefinition {
                manifest: p.raw,
                builtin: true,
            })
        } else {
            None
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn monorepo_lockfile_and_first_match() {
        let d = detect_dir(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/detect/monorepo/app"
        )));
        assert_eq!(d.framework, Framework::NextJs);
        assert_eq!(d.package_manager, Some(PackageManager::Pnpm));
        assert_eq!(
            d.rewrite_builtin("npx next dev -p ${PORT}"),
            "pnpm exec next dev -p ${PORT}"
        );
    }
    #[test]
    fn fastapi_entry_and_runner() {
        let d = detect_dir(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/detect/fastapi"
        )));
        assert_eq!(d.framework, Framework::FastApi);
        assert_eq!(d.fastapi_entrypoint.as_deref(), Some("app:web"));
        assert_eq!(d.python_runner, Some(PythonRunner::Uv));
        assert!(
            d.rewrite_builtin("uvicorn app.main:app --port ${PORT}")
                .starts_with("uv run uvicorn app:web")
        );
    }
    #[test]
    fn builtin_templates_parse() {
        for name in BUILTIN_NAMES {
            assert!(Templates::default().get(name).is_some(), "{name}");
        }
    }
}

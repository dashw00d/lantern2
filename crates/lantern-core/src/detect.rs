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
    Astro,
    SvelteKit,
    ReactRouter,
    Remix,
    Angular,
    Expo,
    VitePress,
    Docusaurus,
    Storybook,
    Vite,
    Wrangler,
    Node,
    FastApi,
    Django,
    Flask,
    Streamlit,
    Gradio,
    MkDocs,
    Rails,
    Hugo,
    Go,
    Rust,
    DockerCompose,
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
            Self::Astro => "astro",
            Self::SvelteKit => "sveltekit",
            Self::ReactRouter => "react-router",
            Self::Remix => "remix",
            Self::Angular => "angular",
            Self::Expo => "expo",
            Self::VitePress => "vitepress",
            Self::Docusaurus => "docusaurus",
            Self::Storybook => "storybook",
            Self::Vite => "vite",
            Self::Wrangler => "wrangler",
            Self::Node => "node",
            Self::FastApi => "fastapi",
            Self::Django => "django",
            Self::Flask => "flask",
            Self::Streamlit => "streamlit",
            Self::Gradio => "gradio",
            Self::MkDocs => "mkdocs",
            Self::Rails => "rails",
            Self::Hugo => "hugo",
            Self::Go => "go",
            Self::Rust => "rust",
            Self::DockerCompose => "docker-compose",
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
    pub node_script: Option<String>,
    pub deno_task: Option<String>,
    pub python_entrypoint: Option<String>,
    pub go_air: bool,
    pub rust_trunk: bool,
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
            node_script: None,
            deno_task: None,
            python_entrypoint: None,
            go_air: false,
            rust_trunk: false,
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
        && let Some(pm) = manager_name(name)
    {
        evidence.push(format!("package.json packageManager={name}"));
        return Some(pm);
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
fn first_existing_file<'a>(dir: &Path, candidates: &[&'a str]) -> Option<&'a str> {
    candidates.iter().copied().find(|f| dir.join(f).is_file())
}
fn deno_dev_task(dir: &Path) -> Option<String> {
    for name in ["deno.json", "deno.jsonc"] {
        if let Some(src) = read_bounded(&dir.join(name), 256 * 1024)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&src)
            && let Some(tasks) = val.get("tasks").and_then(|v| v.as_object())
        {
            for key in ["dev", "start", "serve"] {
                if tasks.contains_key(key) {
                    return Some(key.into());
                }
            }
        }
    }
    None
}
fn cargo_has_web_crate(dir: &Path) -> bool {
    let Some(src) = read_bounded(&dir.join("Cargo.toml"), 256 * 1024) else {
        return false;
    };
    let lower = src.to_ascii_lowercase();
    [
        "axum",
        "actix-web",
        "rocket",
        "warp",
        "poem",
        "salvo",
        "leptos",
        "dioxus",
        "tide",
        "tiny_http",
        "rouille",
    ]
    .iter()
    .any(|crate_name| lower.contains(crate_name))
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
    let has_script = |key: &str| {
        package
            .as_ref()
            .and_then(|p| p.get("scripts"))
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
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
    let wrangler_config = ["wrangler.toml", "wrangler.json", "wrangler.jsonc"]
        .iter()
        .any(|p| dir.join(p).is_file());
    let compose_file = [
        "compose.yaml",
        "compose.yml",
        "docker-compose.yml",
        "docker-compose.yaml",
    ]
    .iter()
    .any(|p| dir.join(p).is_file());
    let hugo_config = ["hugo.toml", "hugo.yaml", "hugo.json", "config.toml"]
        .iter()
        .any(|p| dir.join(p).is_file())
        && dir.join("content").is_dir();
    let deno_task = deno_dev_task(dir);
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
    } else if dep("astro") {
        (
            Framework::Astro,
            Confidence::High,
            "package.json dependency astro",
        )
    } else if dep("@sveltejs/kit") {
        (
            Framework::SvelteKit,
            Confidence::High,
            "package.json dependency @sveltejs/kit",
        )
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
    } else if dep("@angular/cli") || dir.join("angular.json").is_file() {
        (
            Framework::Angular,
            Confidence::High,
            "Angular configuration or dependency",
        )
    } else if dep("expo") {
        (Framework::Expo, Confidence::High, "package.json dependency expo")
    } else if dep("vitepress") {
        (
            Framework::VitePress,
            Confidence::High,
            "package.json dependency vitepress",
        )
    } else if dep("@docusaurus/core") {
        (
            Framework::Docusaurus,
            Confidence::High,
            "package.json dependency @docusaurus/core",
        )
    } else if dep("storybook")
        || dep("@storybook/react")
        || dep("@storybook/vue3")
        || dep("@storybook/svelte")
    {
        (
            Framework::Storybook,
            Confidence::Medium,
            "package.json dependency storybook",
        )
    } else if dep("vite") || vite_config {
        (
            Framework::Vite,
            Confidence::Medium,
            if dep("vite") {
                "package.json dependency vite"
            } else {
                "vite.config"
            },
        )
    } else if dep("wrangler") || wrangler_config {
        (
            Framework::Wrangler,
            Confidence::Medium,
            "Wrangler configuration or dependency",
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
    } else if has_py("streamlit") {
        d.python_entrypoint = first_existing_file(dir, &["app.py", "streamlit_app.py", "Home.py", "main.py"])
            .map(str::to_owned);
        (
            Framework::Streamlit,
            Confidence::Medium,
            "Python dependency streamlit",
        )
    } else if has_py("gradio") {
        d.python_entrypoint = first_existing_file(dir, &["app.py", "demo.py", "main.py"]).map(str::to_owned);
        (Framework::Gradio, Confidence::Medium, "Python dependency gradio")
    } else if dir.join("mkdocs.yml").is_file() || dir.join("mkdocs.yaml").is_file() {
        (Framework::MkDocs, Confidence::Medium, "mkdocs.yml")
    } else if dir.join("Gemfile").is_file()
        && (dir.join("bin/rails").is_file() || dir.join("config/application.rb").is_file())
    {
        (Framework::Rails, Confidence::High, "Gemfile + Rails app")
    } else if hugo_config {
        (Framework::Hugo, Confidence::Medium, "Hugo config + content/")
    } else if let Some(task) = deno_task {
        d.deno_task = Some(task);
        (Framework::Node, Confidence::Medium, "deno.json task")
    } else if has_script("dev") || has_script("start") {
        d.node_script = Some(if has_script("dev") { "dev" } else { "start" }.into());
        (
            Framework::Node,
            Confidence::Medium,
            if has_script("dev") {
                "package.json scripts.dev"
            } else {
                "package.json scripts.start"
            },
        )
    } else if dir.join("go.mod").is_file()
        && (dir.join(".air.toml").is_file()
            || dir.join(".air.conf").is_file()
            || dir.join("main.go").is_file()
            || dir.join("cmd").is_dir())
    {
        d.go_air = dir.join(".air.toml").is_file() || dir.join(".air.conf").is_file();
        (
            Framework::Go,
            Confidence::Medium,
            if d.go_air {
                "go.mod + .air.toml"
            } else {
                "go.mod + entrypoint"
            },
        )
    } else if dir.join("Trunk.toml").is_file() || cargo_has_web_crate(dir) {
        d.rust_trunk = dir.join("Trunk.toml").is_file();
        (
            Framework::Rust,
            Confidence::Medium,
            if d.rust_trunk {
                "Trunk.toml"
            } else {
                "Cargo.toml web server crate"
            },
        )
    } else if compose_file {
        (
            Framework::DockerCompose,
            Confidence::Medium,
            "Docker Compose file",
        )
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
        Framework::FastApi
            | Framework::Django
            | Framework::Flask
            | Framework::Streamlit
            | Framework::Gradio
            | Framework::MkDocs
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
        if let Some(entry) = &self.python_entrypoint {
            cmd = cmd.replace("app.py", entry);
        }
        if self.framework == Framework::Node {
            if let Some(task) = &self.deno_task {
                return format!("deno task {task}");
            }
            if let Some(script) = &self.node_script {
                return match self.package_manager {
                    Some(PackageManager::Pnpm) => format!("pnpm run {script}"),
                    Some(PackageManager::Yarn) => format!("yarn {script}"),
                    Some(PackageManager::Bun) => format!("bun run {script}"),
                    _ => format!("npm run {script}"),
                };
            }
        }
        if self.framework == Framework::Go && self.go_air {
            return "air".into();
        }
        if self.framework == Framework::Rust && self.rust_trunk {
            return "trunk serve --address ${HOST} --port ${PORT}".into();
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
            Framework::FastApi
                | Framework::Django
                | Framework::Flask
                | Framework::Streamlit
                | Framework::Gradio
                | Framework::MkDocs
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
    "astro",
    "sveltekit",
    "remix",
    "react-router",
    "angular",
    "expo",
    "vitepress",
    "docusaurus",
    "storybook",
    "vite",
    "wrangler",
    "node",
    "fastapi",
    "django",
    "flask",
    "streamlit",
    "gradio",
    "mkdocs",
    "laravel",
    "laravel-serve",
    "symfony",
    "php",
    "rails",
    "hugo",
    "go",
    "rust",
    "docker-compose",
    "static",
];
/// Fetch an embedded template's YAML text.
pub fn builtin_source(name: &str) -> Option<&'static str> {
    match name {
        "nextjs" => Some(include_str!("../builtin/templates/nextjs.yaml")),
        "nuxt" => Some(include_str!("../builtin/templates/nuxt.yaml")),
        "astro" => Some(include_str!("../builtin/templates/astro.yaml")),
        "sveltekit" => Some(include_str!("../builtin/templates/sveltekit.yaml")),
        "remix" => Some(include_str!("../builtin/templates/remix.yaml")),
        "react-router" => Some(include_str!("../builtin/templates/react-router.yaml")),
        "angular" => Some(include_str!("../builtin/templates/angular.yaml")),
        "expo" => Some(include_str!("../builtin/templates/expo.yaml")),
        "vitepress" => Some(include_str!("../builtin/templates/vitepress.yaml")),
        "docusaurus" => Some(include_str!("../builtin/templates/docusaurus.yaml")),
        "storybook" => Some(include_str!("../builtin/templates/storybook.yaml")),
        "vite" => Some(include_str!("../builtin/templates/vite.yaml")),
        "wrangler" => Some(include_str!("../builtin/templates/wrangler.yaml")),
        "node" => Some(include_str!("../builtin/templates/node.yaml")),
        "fastapi" => Some(include_str!("../builtin/templates/fastapi.yaml")),
        "django" => Some(include_str!("../builtin/templates/django.yaml")),
        "flask" => Some(include_str!("../builtin/templates/flask.yaml")),
        "streamlit" => Some(include_str!("../builtin/templates/streamlit.yaml")),
        "gradio" => Some(include_str!("../builtin/templates/gradio.yaml")),
        "mkdocs" => Some(include_str!("../builtin/templates/mkdocs.yaml")),
        "laravel" => Some(include_str!("../builtin/templates/laravel.yaml")),
        "laravel-serve" => Some(include_str!("../builtin/templates/laravel-serve.yaml")),
        "symfony" => Some(include_str!("../builtin/templates/symfony.yaml")),
        "php" => Some(include_str!("../builtin/templates/php.yaml")),
        "rails" => Some(include_str!("../builtin/templates/rails.yaml")),
        "hugo" => Some(include_str!("../builtin/templates/hugo.yaml")),
        "go" => Some(include_str!("../builtin/templates/go.yaml")),
        "rust" => Some(include_str!("../builtin/templates/rust.yaml")),
        "docker-compose" => Some(include_str!("../builtin/templates/docker-compose.yaml")),
        "static" => Some(include_str!("../builtin/templates/static.yaml")),
        _ => None,
    }
}
/// Lookup containing user templates which shadow builtins.
#[derive(Debug, Clone, Default)]
pub struct Templates {
    pub user: BTreeMap<String, manifest::raw::RawManifest>,
}
/// Validate a user template filename stem.
pub fn valid_template_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 48
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}
impl Templates {
    /// Load user templates from a directory; valid user files shadow embedded templates.
    pub fn load_dir(dir: &Path) -> std::io::Result<(Self, Vec<manifest::Diagnostic>)> {
        use std::io::Read;
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
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
            if !valid_template_name(name) {
                diagnostics.push(template_diag(
                    &path,
                    "bad_value",
                    "name",
                    "invalid template filename",
                    manifest::Severity::Error,
                ));
                continue;
            }
            if result.user.contains_key(name) {
                continue;
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take((crate::yaml::MAX_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > crate::yaml::MAX_BYTES {
                diagnostics.push(template_diag(
                    &path,
                    "file_too_large",
                    "",
                    "template exceeds 1 MiB",
                    manifest::Severity::Error,
                ));
                continue;
            }
            let src = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(_) => {
                    diagnostics.push(template_diag(
                        &path,
                        "not_utf8",
                        "",
                        "template is not UTF-8",
                        manifest::Severity::Error,
                    ));
                    continue;
                }
            };
            let mut parsed = manifest::parse_manifest(src, &path);
            if parsed.raw.name.as_deref().is_some_and(|n| n != name) {
                parsed.diagnostics.push(template_diag(
                    &path,
                    "bad_value",
                    "name",
                    "template name must match the filename",
                    manifest::Severity::Error,
                ));
                parsed.usable = false;
            }
            for (key, present) in [
                ("id", parsed.raw.id.is_some()),
                ("domain", parsed.raw.domain.is_some()),
                ("repo_url", parsed.raw.repo_url.is_some()),
                ("enabled", parsed.raw.enabled.is_some()),
            ] {
                if present {
                    parsed.diagnostics.push(template_diag(
                        &path,
                        "template_key_ignored",
                        key,
                        "identity key is ignored in templates",
                        manifest::Severity::Warning,
                    ));
                }
            }
            diagnostics.extend(parsed.diagnostics);
            if parsed.usable {
                result.user.insert(name.to_owned(), parsed.raw);
            }
        }
        Ok((result, diagnostics))
    }
}
fn template_diag(
    path: &Path,
    code: &str,
    key: &str,
    message: &str,
    severity: manifest::Severity,
) -> manifest::Diagnostic {
    manifest::Diagnostic {
        severity,
        code: code.into(),
        path: key.into(),
        file: path.into(),
        line: None,
        column: None,
        message: message.into(),
        expected: None,
        help: None,
    }
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
    fn user_templates_shadow_builtins() {
        let dir = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/v1/templates"
        ));
        let (templates, diagnostics) = Templates::load_dir(dir).unwrap();
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == manifest::Severity::Error),
            "{diagnostics:?}"
        );
        let next = templates.get("nextjs").unwrap();
        assert!(!next.builtin);
        assert!(next.manifest.run.unwrap().cmd.unwrap().contains("npx next"));
    }
    #[test]
    fn builtin_templates_parse() {
        for name in BUILTIN_NAMES {
            assert!(Templates::default().get(name).is_some(), "{name}");
        }
    }
    #[test]
    fn extended_framework_detection() {
        let root = std::env::temp_dir().join(format!("lantern-detect-ext-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let astro = root.join("astro");
        fs::create_dir_all(&astro).unwrap();
        fs::write(astro.join("package.json"), r#"{"dependencies":{"astro":"^5.0"}}"#).unwrap();
        fs::write(astro.join("pnpm-lock.yaml"), "").unwrap();
        let d = detect_dir(&astro);
        assert_eq!(d.framework, Framework::Astro);
        assert_eq!(
            d.rewrite_builtin("npx astro dev --host ${HOST} --port ${PORT}"),
            "pnpm exec astro dev --host ${HOST} --port ${PORT}"
        );

        let svelte = root.join("svelte");
        fs::create_dir_all(&svelte).unwrap();
        fs::write(
            svelte.join("package.json"),
            r#"{"devDependencies":{"@sveltejs/kit":"^2.0","vite":"^6.0"}}"#,
        )
        .unwrap();
        assert_eq!(detect_dir(&svelte).framework, Framework::SvelteKit);

        let node = root.join("node");
        fs::create_dir_all(&node).unwrap();
        fs::write(
            node.join("package.json"),
            r#"{"scripts":{"dev":"tsx watch src/index.ts"}}"#,
        )
        .unwrap();
        fs::write(node.join("bun.lock"), "").unwrap();
        let d = detect_dir(&node);
        assert_eq!(d.framework, Framework::Node);
        assert_eq!(d.rewrite_builtin("npm run dev"), "bun run dev");

        let go = root.join("goapp");
        fs::create_dir_all(&go).unwrap();
        fs::write(go.join("go.mod"), "module example.com/app\n").unwrap();
        fs::write(go.join(".air.toml"), "").unwrap();
        let d = detect_dir(&go);
        assert_eq!(d.framework, Framework::Go);
        assert_eq!(d.rewrite_builtin("go run ."), "air");

        let rust = root.join("rustapp");
        fs::create_dir_all(&rust).unwrap();
        fs::write(rust.join("Cargo.toml"), "[dependencies]\naxum = \"0.8\"\n").unwrap();
        assert_eq!(detect_dir(&rust).framework, Framework::Rust);

        let st = root.join("streamlitapp");
        fs::create_dir_all(&st).unwrap();
        fs::write(st.join("requirements.txt"), "streamlit>=1.30\n").unwrap();
        fs::write(st.join("Home.py"), "import streamlit as st\n").unwrap();
        fs::write(st.join("uv.lock"), "").unwrap();
        let d = detect_dir(&st);
        assert_eq!(d.framework, Framework::Streamlit);
        assert!(
            d.rewrite_builtin("streamlit run app.py")
                .starts_with("uv run streamlit run Home.py")
        );

        let _ = fs::remove_dir_all(&root);
    }
}

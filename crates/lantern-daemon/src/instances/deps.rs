//! Default installation choices and content-based setup stamps.
use super::InstanceError;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

const LOCKS: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "requirements.txt",
    "composer.lock",
    "Gemfile.lock",
    "go.sum",
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".tool-versions",
    ".php-version",
];

/// A default installation command and the workspace directory that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultSetup {
    /// Directory to run the installation in.
    pub cwd: PathBuf,
    /// Shell command.
    pub command: String,
    /// Directory whose absence makes the stamp stale.
    pub artifact: PathBuf,
}

fn ancestors(project: &Path, root: &Path) -> Result<Vec<PathBuf>, InstanceError> {
    if !project.starts_with(root) {
        return Err(InstanceError::UnsafePath(project.display().to_string()));
    }
    let mut out = Vec::new();
    let mut d = project;
    loop {
        out.push(d.to_path_buf());
        if d == root {
            break;
        }
        d = d
            .parent()
            .ok_or_else(|| InstanceError::UnsafePath(project.display().to_string()))?;
    }
    Ok(out)
}
/// SHA-256 hash of all supported lockfiles and version pins along the checkout's ancestor chain.
pub fn lock_hash(project: &Path, root: &Path) -> Result<String, InstanceError> {
    let mut paths = Vec::new();
    for dir in ancestors(project, root)? {
        for name in LOCKS {
            let p = dir.join(name);
            if p.is_file() {
                paths.push(p);
            }
        }
    }
    paths.sort();
    let mut h = Sha256::new();
    for p in paths {
        let rel = p.strip_prefix(root).expect("ancestor under root");
        let name = rel.to_string_lossy();
        let bytes = fs::read(&p)?;
        h.update((name.len() as u64).to_be_bytes());
        h.update(name.as_bytes());
        h.update((bytes.len() as u64).to_be_bytes());
        h.update(&bytes);
    }
    Ok(hex(&h.finalize()))
}
/// SHA-256 hash of setup commands and their execution directories.
pub fn commands_hash(commands: &[(PathBuf, String)]) -> String {
    let mut h = Sha256::new();
    for (dir, cmd) in commands {
        let d = dir.to_string_lossy();
        h.update((d.len() as u64).to_be_bytes());
        h.update(d.as_bytes());
        h.update((cmd.len() as u64).to_be_bytes());
        h.update(cmd.as_bytes());
    }
    hex(&h.finalize())
}
/// Detect PHP, Python and Node setups in that order, using nearest project/ancestor evidence.
pub fn default_setup(project: &Path, root: &Path) -> Result<Vec<DefaultSetup>, InstanceError> {
    let dirs = ancestors(project, root)?;
    let mut result = Vec::new();
    if let Some(d) = dirs.iter().find(|d| d.join("composer.lock").is_file()) {
        result.push(DefaultSetup {
            cwd: d.clone(),
            command: "composer install --no-interaction --prefer-dist".into(),
            artifact: d.join("vendor"),
        });
    }
    for d in &dirs {
        let py = if d.join("uv.lock").is_file() {
            Some("uv sync --frozen")
        } else if fs::read_to_string(d.join("pyproject.toml"))
            .unwrap_or_default()
            .contains("[tool.uv]")
        {
            Some("uv sync")
        } else if d.join("poetry.lock").is_file() {
            Some("poetry install --no-interaction")
        } else if d.join("requirements.txt").is_file() {
            Some(
                "if command -v uv >/dev/null 2>&1; then uv venv --allow-existing && uv pip install -r requirements.txt; else python3 -m venv .venv && .venv/bin/pip install -r requirements.txt; fi",
            )
        } else {
            None
        };
        if let Some(cmd) = py {
            result.push(DefaultSetup {
                cwd: d.clone(),
                command: cmd.into(),
                artifact: d.join(".venv"),
            });
            break;
        }
    }
    for d in &dirs {
        let pm = fs::read(d.join("package.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| {
                v.get("packageManager")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            });
        let manager = pm
            .as_deref()
            .and_then(|s| s.split('@').next())
            .filter(|s| matches!(*s, "bun" | "pnpm" | "yarn" | "npm"))
            .or_else(|| {
                if d.join("bun.lock").is_file() || d.join("bun.lockb").is_file() {
                    Some("bun")
                } else if d.join("pnpm-lock.yaml").is_file() {
                    Some("pnpm")
                } else if d.join("yarn.lock").is_file() {
                    Some("yarn")
                } else if d.join("package-lock.json").is_file() || d.join("npm-shrinkwrap.json").is_file() {
                    Some("npm")
                } else {
                    None
                }
            });
        let command = match manager {
            Some("bun") => Some("bun install --frozen-lockfile"),
            Some("pnpm") => Some("pnpm install --frozen-lockfile --prefer-offline"),
            Some("npm") => Some("npm ci --prefer-offline --no-audit --no-fund"),
            Some("yarn")
                if d.join(".yarnrc.yml").is_file()
                    || fs::read_to_string(d.join("yarn.lock"))
                        .unwrap_or_default()
                        .contains("__metadata:") =>
            {
                Some("yarn install --immutable")
            }
            Some("yarn") => Some("yarn install --frozen-lockfile --prefer-offline"),
            _ => None,
        };
        if let Some(cmd) = command {
            result.push(DefaultSetup {
                cwd: d.clone(),
                command: cmd.into(),
                artifact: d.join("node_modules"),
            });
            break;
        }
    }
    Ok(result)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

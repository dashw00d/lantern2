//! Generate the per-instance Bash launcher without interpolating data into commands.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use lantern_core::ids::InstanceId;
use thiserror::Error;

/// A shell-mutating version manager selected for the checkout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Toolchain {
    /// No shell-specific version manager is needed.
    #[default]
    None,
    /// Run the command under `mise exec`.
    Mise,
    /// Initialize and select a Node version with fnm.
    Fnm,
    /// Source nvm from this directory and select the requested Node version.
    Nvm(PathBuf),
}

/// Detected shell toolchain and independent Python virtual environment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DetectedToolchain {
    /// Version manager, selected by the documented precedence.
    pub manager: Toolchain,
    /// Virtual environment in the working directory, if present.
    pub virtual_env: Option<PathBuf>,
}

/// All data needed to render a launch and optional post-stop script.
///
/// `command`, `pre_start`, and `post_stop` must already be shell-rendered by the
/// caller; map values and paths are data. Use `try_render` with untrusted input;
/// `render` panics when its NUL-free, UTF-8 and identifier preconditions fail.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// Owning instance, also used for the two instance exports.
    pub instance: InstanceId,
    /// Working directory for both scripts.
    pub cwd: PathBuf,
    /// Optional captured shell environment file.
    pub shell_env: Option<PathBuf>,
    /// Safety-net directories to append to PATH when they exist.
    pub safety_path: Vec<PathBuf>,
    /// Detected toolchain, including an optional virtual environment.
    pub toolchain: DetectedToolchain,
    /// Settings-level environment, below Lantern variables in precedence.
    pub runtime_env: BTreeMap<String, String>,
    /// Template variable names without `LANTERN_` and their data values.
    pub variables: BTreeMap<String, String>,
    /// Parsed deploy environment file, below manifest environment in precedence.
    pub env_file: BTreeMap<String, String>,
    /// Already data-rendered manifest environment.
    pub run_env: BTreeMap<String, String>,
    /// Already shell-rendered setup to run before the command.
    pub pre_start: Option<String>,
    /// Already shell-rendered post-stop command.
    pub post_stop: Option<String>,
    /// Already shell-rendered command.
    pub command: String,
}

/// An invalid value that cannot safely be represented in a launcher script.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LaunchError {
    /// A value or command contained a NUL byte.
    #[error("launcher value contains NUL")]
    Nul,
    /// An environment or template variable name is not a valid shell identifier.
    #[error("invalid launcher environment key: {0}")]
    InvalidKey(String),
    /// A path cannot be represented as NUL-free UTF-8 shell text.
    #[error("launcher path is not NUL-free UTF-8")]
    InvalidPath,
}

impl LaunchSpec {
    /// Validate untrusted values before script generation.
    pub fn validate(&self) -> Result<(), LaunchError> {
        let path_ok = |path: &Path| path.to_str().is_some_and(|s| !s.contains('\0'));
        if !path_ok(&self.cwd)
            || self.shell_env.as_deref().is_some_and(|p| !path_ok(p))
            || self.safety_path.iter().any(|p| !path_ok(p))
            || matches!(&self.toolchain.manager, Toolchain::Nvm(p) if !path_ok(p))
            || self.toolchain.virtual_env.as_deref().is_some_and(|p| !path_ok(p))
        {
            return Err(LaunchError::InvalidPath);
        }
        for vars in [&self.runtime_env, &self.env_file, &self.run_env, &self.variables] {
            for (key, value) in vars {
                if !identifier(key) {
                    return Err(LaunchError::InvalidKey(key.clone()));
                }
                if value.contains('\0') {
                    return Err(LaunchError::Nul);
                }
            }
        }
        for key in self.variables.keys() {
            if !key
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(LaunchError::InvalidKey(key.clone()));
            }
        }
        if self.command.contains('\0')
            || self.pre_start.as_deref().is_some_and(|s| s.contains('\0'))
            || self.post_stop.as_deref().is_some_and(|s| s.contains('\0'))
        {
            return Err(LaunchError::Nul);
        }
        Ok(())
    }
}

/// Quote one NUL-free literal for Bash, including embedded single quotes.
pub fn sh_quote(value: &str) -> String {
    assert!(!value.contains('\0'), "shell values cannot contain NUL");
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn path_quote(path: &Path) -> String {
    sh_quote(path.to_str().expect("launcher paths must be UTF-8"))
}

fn identifier(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn exports(out: &mut String, vars: &BTreeMap<String, String>) {
    for (key, value) in vars {
        assert!(identifier(key), "invalid shell environment key: {key:?}");
        out.push_str(&format!("export {key}={}\n", sh_quote(value)));
    }
}

fn preamble(spec: &LaunchSpec) -> String {
    let mut out =
        String::from("#!/usr/bin/env bash\nlantern_fail() { echo \"[lantern] $*\" >&2; exit 78; }\n");
    out.push_str(&format!(
        "cd {} || lantern_fail 'working directory missing'\n",
        path_quote(&spec.cwd)
    ));
    if let Some(file) = &spec.shell_env {
        out.push_str(&format!(
            ". {} || lantern_fail 'cannot source shell environment'\n",
            path_quote(file)
        ));
    }
    // Clear values inherited from both the manager and the captured shell.
    out.push_str("while IFS= read -r lantern_name; do unset \"$lantern_name\"; done < <(compgen -A variable LANTERN_)\nunset lantern_name\n");
    for dir in &spec.safety_path {
        out.push_str(&format!(
            "if [ -d {} ]; then case :\"$PATH\": in *:{}:*) ;; *) export PATH=\"$PATH\":{} ;; esac; fi\n",
            path_quote(dir),
            path_quote(dir),
            path_quote(dir)
        ));
    }
    match &spec.toolchain.manager {
        Toolchain::None | Toolchain::Mise => {}
        Toolchain::Fnm => out.push_str("lantern_fnm_env=$(fnm env --shell bash) || lantern_fail 'fnm: environment setup failed'\neval \"$lantern_fnm_env\" || lantern_fail 'fnm: environment setup failed'\nunset lantern_fnm_env\nfnm use --version-file-strategy=recursive --silent-if-unchanged || lantern_fail 'fnm: version not installed (run: fnm install)'\n"),
        Toolchain::Nvm(dir) => out.push_str(&format!("export NVM_DIR={}\n. \"$NVM_DIR/nvm.sh\" --no-use && nvm use --silent || lantern_fail 'nvm: .nvmrc version not installed (run: nvm install)'\n", path_quote(dir))),
    }
    if let Some(dir) = &spec.toolchain.virtual_env {
        out.push_str(&format!(
            "export VIRTUAL_ENV={}\nexport PATH=\"$VIRTUAL_ENV/bin:$PATH\"\n",
            path_quote(dir)
        ));
    }
    exports(&mut out, &spec.runtime_env);
    for (key, value) in &spec.variables {
        assert!(
            identifier(key)
                && key
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
            "invalid template variable: {key:?}"
        );
        // These two are derived from the validated ID, not caller-provided data.
        if key != "INSTANCE" && key != "INSTANCE_ID" {
            out.push_str(&format!("export LANTERN_{key}={}\n", sh_quote(value)));
        }
    }
    out.push_str(&format!(
        "export LANTERN_INSTANCE={}\nexport LANTERN_INSTANCE_ID={}\n",
        sh_quote(&spec.instance.safe_name()),
        sh_quote(spec.instance.as_str())
    ));
    out.push_str("export PORT=\"${LANTERN_PORT-}\" DOMAIN=\"${LANTERN_DOMAIN-}\" PROJECT_NAME=\"${LANTERN_PROJECT-}\"\n");
    exports(&mut out, &spec.env_file);
    exports(&mut out, &spec.run_env);
    out
}

/// Render the main script, with literal values quoted and stdin kept open.
pub fn render(spec: &LaunchSpec) -> String {
    spec.validate().expect("invalid launcher spec");
    let mut out = preamble(spec);
    if let Some(command) = &spec.pre_start {
        assert!(!command.contains('\0'), "command cannot contain NUL");
        out.push_str(&format!("{{ {command}\n}} || exit $?\n"));
    }
    let prefix = if spec.toolchain.manager == Toolchain::Mise {
        "exec mise exec -- bash -c "
    } else {
        "exec bash -c "
    };
    out.push_str(&format!(
        "{prefix}{} < <(exec sleep infinity)\n",
        sh_quote(&spec.command)
    ));
    out
}

/// Validate untrusted data and render the main script without panicking on invalid input.
pub fn try_render(spec: &LaunchSpec) -> Result<String, LaunchError> {
    spec.validate()?;
    Ok(render(spec))
}

/// Render the optional ExecStopPost script with the same environment as the launch.
pub fn render_post_stop(spec: &LaunchSpec) -> Option<String> {
    spec.validate().expect("invalid launcher spec");
    spec.post_stop.as_ref().map(|command| {
        let mut out = preamble(spec);
        let prefix = if spec.toolchain.manager == Toolchain::Mise {
            "exec mise exec -- bash -c "
        } else {
            "exec bash -c "
        };
        out.push_str(&format!("{prefix}{}\n", sh_quote(command)));
        out
    })
}

/// Validate untrusted data and render the optional post-stop script.
pub fn try_render_post_stop(spec: &LaunchSpec) -> Result<Option<String>, LaunchError> {
    spec.validate()?;
    Ok(render_post_stop(spec))
}

fn executable_on_path(name: &str, path: &str) -> bool {
    std::env::split_paths(path).any(|dir| {
        let candidate = dir.join(name);
        candidate
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Detect managers from `cwd` up to `checkout_root`, using the captured PATH and NVM_DIR.
///
/// The working directory must be within the checkout; no paths above it are inspected.
pub fn detect_toolchain(
    cwd: &Path,
    checkout_root: &Path,
    path: &str,
    nvm_dir: Option<&Path>,
    home: &Path,
) -> DetectedToolchain {
    let mut dirs = Vec::new();
    let (Ok(cwd_canonical), Ok(root_canonical)) = (cwd.canonicalize(), checkout_root.canonicalize()) else {
        return DetectedToolchain::default();
    };
    if cwd_canonical.starts_with(&root_canonical) {
        let mut dir = cwd_canonical;
        loop {
            dirs.push(dir.clone());
            if dir == root_canonical {
                break;
            }
            if !dir.pop() {
                break;
            }
        }
    } else {
        return DetectedToolchain::default();
    }
    let has = |names: &[&str]| {
        dirs.iter()
            .any(|dir| names.iter().any(|name| dir.join(name).is_file()))
    };
    let fallback_nvm = home.join(".nvm");
    let nvm = nvm_dir.filter(|p| p.join("nvm.sh").is_file()).or_else(|| {
        fallback_nvm
            .join("nvm.sh")
            .is_file()
            .then_some(fallback_nvm.as_path())
    });
    let manager = if executable_on_path("mise", path) && has(&["mise.toml", ".mise.toml", ".tool-versions"]) {
        Toolchain::Mise
    } else if executable_on_path("fnm", path) && has(&[".node-version", ".nvmrc"]) {
        Toolchain::Fnm
    } else if has(&[".nvmrc"]) {
        nvm.map_or(Toolchain::None, |dir| Toolchain::Nvm(dir.to_path_buf()))
    } else {
        Toolchain::None
    };
    let virtual_env = [".venv", "venv"]
        .into_iter()
        .map(|dir| cwd.join(dir))
        .find(|dir| dir.join("pyvenv.cfg").is_file());
    DetectedToolchain { manager, virtual_env }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> LaunchSpec {
        LaunchSpec {
            instance: InstanceId::parse("app@feature").unwrap(),
            cwd: "/work/feature x".into(),
            shell_env: Some("/cache/shell-env.sh".into()),
            safety_path: vec!["/home/u/.cargo/bin".into()],
            toolchain: DetectedToolchain {
                manager: Toolchain::Nvm("/home/u/.nvm".into()),
                virtual_env: Some("/work/feature x/.venv".into()),
            },
            runtime_env: BTreeMap::from([("NODE_ENV".into(), "development".into())]),
            variables: BTreeMap::from([
                ("BRANCH".into(), "x$(id)".into()),
                ("DOMAIN".into(), "feature.app.glow".into()),
                ("PORT".into(), "29100".into()),
                ("PROJECT".into(), "app".into()),
            ]),
            env_file: BTreeMap::new(),
            run_env: BTreeMap::from([("API_KEY".into(), "a'b".into())]),
            pre_start: Some("echo setup \"${LANTERN_BRANCH}\"".into()),
            post_stop: Some("echo stop \"${LANTERN_BRANCH}\"".into()),
            command: "pnpm dev --port ${LANTERN_PORT}".into(),
        }
    }

    #[test]
    fn golden_launcher() {
        assert_eq!(render(&spec()), include_str!("fixtures/launcher.sh"));
        let post = render_post_stop(&spec()).unwrap();
        assert_eq!(post, include_str!("fixtures/post_stop.sh"));
        assert!(!post.contains("echo setup"));
    }

    #[test]
    fn branch_metacharacters_only_appear_in_single_quoted_data() {
        let script = render(&spec());
        assert_eq!(script.matches("$(id)").count(), 1);
        assert!(script.contains("export LANTERN_BRANCH='x$(id)'\n"));
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn mise_and_fnm_render_expected_commands() {
        let mut s = spec();
        s.toolchain.manager = Toolchain::Mise;
        assert!(render(&s).contains("exec mise exec -- bash -c 'pnpm"));
        assert!(
            render_post_stop(&s)
                .unwrap()
                .contains("exec mise exec -- bash -c 'echo stop")
        );
        s.toolchain.manager = Toolchain::Fnm;
        assert!(render(&s).contains("fnm use --version-file-strategy=recursive"));
    }

    #[test]
    fn exported_values_are_data_and_inherited_lantern_variables_are_removed() {
        use std::fs;
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("lantern-runtime-envtest-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let env_file = root.join("env.sh");
        fs::write(
            &env_file,
            "export LANTERN_DAEMON_URL=unsafe LANTERN_STALE=unsafe\n",
        )
        .unwrap();
        let mut s = spec();
        s.cwd = root.clone();
        s.shell_env = Some(env_file);
        s.safety_path.clear();
        s.toolchain = DetectedToolchain::default();
        let script = format!(
            "{}printf '%s\\n' \"$LANTERN_BRANCH\" \"${{LANTERN_DAEMON_URL-unset}}\" \"${{LANTERN_DAEMON_PORT-unset}}\" \"${{LANTERN_STALE-unset}}\" \"$LANTERN_INSTANCE_ID\" \"$PORT\"\n",
            preamble(&s)
        );
        let output = Command::new("/bin/bash")
            .arg("-c")
            .arg(script)
            .env("LANTERN_DAEMON_PORT", "unsafe")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "x$(id)\nunset\nunset\nunset\napp@feature\n29100\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[should_panic(expected = "NUL")]
    fn nul_is_rejected() {
        sh_quote("bad\0value");
    }

    #[test]
    fn untrusted_launcher_inputs_return_validation_errors() {
        let mut s = spec();
        s.run_env.insert("BAD-KEY".into(), "value".into());
        assert_eq!(try_render(&s), Err(LaunchError::InvalidKey("BAD-KEY".into())));
        s.run_env.clear();
        s.variables.insert("BRANCH".into(), "bad\0value".into());
        assert_eq!(try_render(&s), Err(LaunchError::Nul));
        assert_eq!(try_render_post_stop(&s), Err(LaunchError::Nul));
    }

    #[test]
    fn detection_obeys_precedence_checkout_boundary_and_venv() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("lantern-runtime-launcher-{}", std::process::id()));
        let checkout = root.join("checkout");
        let cwd = checkout.join("sub");
        let bin = root.join("bin");
        fs::create_dir_all(cwd.join(".venv")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(root.join(".nvm")).unwrap();
        fs::write(root.join(".nvm/nvm.sh"), "").unwrap();
        fs::write(cwd.join(".venv/pyvenv.cfg"), "").unwrap();
        fs::write(checkout.join(".nvmrc"), "20").unwrap();
        fs::write(root.join(".tool-versions"), "node 20").unwrap();
        fs::write(bin.join("fnm"), "").unwrap();
        fs::set_permissions(bin.join("fnm"), fs::Permissions::from_mode(0o700)).unwrap();
        let path = bin.to_str().unwrap();
        let detected = detect_toolchain(&cwd, &checkout, path, None, &root);
        assert_eq!(detected.manager, Toolchain::Fnm);
        assert_eq!(detected.virtual_env, Some(cwd.join(".venv")));
        fs::write(bin.join("mise"), "").unwrap();
        fs::set_permissions(bin.join("mise"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            detect_toolchain(&cwd, &checkout, path, None, &root).manager,
            Toolchain::Fnm
        );
        fs::write(checkout.join(".tool-versions"), "node 20").unwrap();
        assert_eq!(
            detect_toolchain(&cwd, &checkout, path, None, &root).manager,
            Toolchain::Mise
        );
        fs::remove_file(bin.join("fnm")).unwrap();
        fs::remove_file(bin.join("mise")).unwrap();
        assert_eq!(
            detect_toolchain(&cwd, &checkout, path, None, &root).manager,
            Toolchain::Nvm(root.join(".nvm"))
        );
        assert_eq!(
            detect_toolchain(&root, &checkout, path, None, &root),
            DetectedToolchain::default()
        );
        fs::remove_dir_all(root).unwrap();
    }
}

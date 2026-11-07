//! Explicit command descriptions and an injectable process boundary.
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Command,
};

/// A command that never uses a shell or inherits the ambient environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Absolute executable path.
    pub program: PathBuf,
    /// Individual arguments, never shell-interpreted.
    pub args: Vec<OsString>,
    /// Explicit environment added after clearing inherited variables.
    pub env: BTreeMap<OsString, OsString>,
    /// Whether the command changes system or user state.
    pub mutating: bool,
    /// Prevent real mutations when this command belongs to a fake-root operation.
    pub fake_root: bool,
}
impl CommandSpec {
    /// Describe a command by its absolute executable and arguments.
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        mutating: bool,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
            mutating,
            fake_root: false,
        }
    }
}
/// Captured process result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit success.
    pub success: bool,
    /// Captured stdout.
    pub stdout: Vec<u8>,
    /// Captured stderr.
    pub stderr: Vec<u8>,
}
/// Process execution boundary; tests can record and simulate all commands.
pub trait CommandRunner {
    /// Execute or simulate a single command.
    fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput>;
}
/// Real process runner, optionally protecting an isolated fake root.
#[derive(Debug, Clone, Copy)]
pub struct ProcessRunner {
    /// Refuse all mutating commands when the caller uses a fake root.
    pub fake_root: bool,
}
impl CommandRunner for ProcessRunner {
    fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
        if !spec.program.is_absolute()
            || spec
                .program
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "command must use an absolute path",
            ));
        }
        if spec.mutating
            && (self.fake_root
                || spec.fake_root
                || std::env::var_os("LANTERN_FAKE_ROOT").is_some_and(|v| !v.is_empty()))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "mutating commands are disabled with a fake root",
            ));
        }
        let output = Command::new(&spec.program)
            .args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}
/// Return the first existing absolute executable candidate.
pub fn find_program(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .map(Path::to_owned)
}

//! Identifiers and slugs (docs/01-architecture.md §6, docs/11-instances.md §3.8, docs/02-decisions.md D2/D11).
//!
//! Controller-owned. Lanes: request changes with `ask, controller:` on the board.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Max length of a [`ProjectId`].
pub const PROJECT_ID_MAX: usize = 48;
/// Max length of a branch slug (keeps `<slug>.<project>.<tld>` far below DNS label limits).
pub const BRANCH_SLUG_MAX: usize = 40;
/// Project ids that collide with daemon routes or Lantern's own hostnames (02 D11).
pub const RESERVED_PROJECT_IDS: &[&str] = &["api", "mcp", "lantern", "mailpit"];

/// Why an id string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("id is empty")]
    Empty,
    #[error("id {0:?} is longer than {PROJECT_ID_MAX} characters")]
    TooLong(String),
    #[error("id {0:?} must match ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$")]
    BadChars(String),
    #[error("id {0:?} is reserved")]
    Reserved(String),
    #[error("instance id {0:?} must be <project> or <project>@<slug>")]
    BadInstance(String),
}

impl IdError {
    /// Stable machine-readable code for the API error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            IdError::Reserved(_) => "reserved_id",
            _ => "invalid_id",
        }
    }
}

/// `true` if `s` matches `^[a-z0-9]([a-z0-9-]*[a-z0-9])?$`.
fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// Lowercase ASCII; every run of characters outside `[a-z0-9]` becomes one `-`; trims `-` at both ends.
/// May return `""`.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_dash = false;
    for c in input.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    out
}

/// Stable 6-hex-char hash (FNV-1a 64). Used to keep truncated or empty slugs unique.
pub fn hash6(input: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in input.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:06x}", h & 0xff_ffff)
}

/// Slug for a branch name: `feature/login-form` → `feature-login-form`. Never empty, at most
/// [`BRANCH_SLUG_MAX`] chars. Prefixes like `feature/` are not stripped.
pub fn branch_slug(branch: &str) -> String {
    let s = slugify(branch);
    if s.is_empty() {
        return format!("b-{}", hash6(branch));
    }
    if s.len() <= BRANCH_SLUG_MAX {
        return s;
    }
    format!("{}-{}", s[..33].trim_end_matches('-'), hash6(branch))
}

/// Slug for a detached HEAD: the 7-character short sha.
pub fn detached_slug(sha: &str) -> String {
    sha.chars().take(7).collect::<String>().to_ascii_lowercase()
}

/// A registered project's id, e.g. `myapp`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectId(String);

impl ProjectId {
    /// Validate an exact id (no slugification).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(IdError::Empty);
        }
        if s.len() > PROJECT_ID_MAX {
            return Err(IdError::TooLong(s.to_string()));
        }
        if !is_slug(s) {
            return Err(IdError::BadChars(s.to_string()));
        }
        if RESERVED_PROJECT_IDS.contains(&s) {
            return Err(IdError::Reserved(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }

    /// Slugify an arbitrary name (e.g. a directory name) into an id, truncating to
    /// [`PROJECT_ID_MAX`]. Fails only for empty results and reserved ids.
    pub fn from_name(name: &str) -> Result<Self, IdError> {
        let mut s = slugify(name);
        if s.len() > PROJECT_ID_MAX {
            s = format!(
                "{}-{}",
                s[..PROJECT_ID_MAX - 7].trim_end_matches('-'),
                hash6(name)
            );
        }
        Self::parse(&s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProjectId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<ProjectId> for String {
    fn from(id: ProjectId) -> Self {
        id.0
    }
}

/// A runnable instance: `myapp` (primary) or `myapp@feature-x` (another checkout).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstanceId(String);

impl InstanceId {
    pub fn primary(project: &ProjectId) -> Self {
        Self(project.0.clone())
    }

    /// `slug` must already be a valid slug (use [`branch_slug`] / [`detached_slug`]).
    pub fn derived(project: &ProjectId, slug: &str) -> Result<Self, IdError> {
        if !is_slug(slug) {
            return Err(IdError::BadChars(slug.to_string()));
        }
        Ok(Self(format!("{}@{}", project.0, slug)))
    }

    pub fn parse(s: &str) -> Result<Self, IdError> {
        match s.split_once('@') {
            None => ProjectId::parse(s).map(|p| Self::primary(&p)),
            Some((p, slug)) => {
                if slug.contains('@') {
                    return Err(IdError::BadInstance(s.to_string()));
                }
                let p = ProjectId::parse(p)?;
                Self::derived(&p, slug).map_err(|_| IdError::BadInstance(s.to_string()))
            }
        }
    }

    pub fn project(&self) -> ProjectId {
        ProjectId(self.0.split('@').next().unwrap_or_default().to_string())
    }

    /// `None` for the primary instance.
    pub fn slug(&self) -> Option<&str> {
        self.0.split_once('@').map(|(_, s)| s)
    }

    pub fn is_primary(&self) -> bool {
        !self.0.contains('@')
    }

    /// Filesystem/systemd-safe form: `myapp` or `myapp_feature-x` (02 D2). `_` never occurs in a
    /// project id or slug, so the mapping is injective. Used for unit names and `${INSTANCE}`.
    pub fn safe_name(&self) -> String {
        self.0.replace('@', "_")
    }

    /// Inverse of [`InstanceId::safe_name`].
    pub fn from_safe_name(s: &str) -> Result<Self, IdError> {
        Self::parse(&s.replacen('_', "@", 1))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for InstanceId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<InstanceId> for String {
    fn from(id: InstanceId) -> Self {
        id.0
    }
}

/// Domain for an instance: primary → `primary_domain`; others → `<slug>.<primary_domain>`.
/// `primary_domain` is the manifest `domain:` override or `<project>.<tld>`.
pub fn instance_domain(primary_domain: &str, slug: Option<&str>) -> String {
    match slug {
        None => primary_domain.to_string(),
        Some(s) => format!("{s}.{primary_domain}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_examples() {
        assert_eq!(slugify("feature/login-form"), "feature-login-form");
        assert_eq!(slugify("release/1.2"), "release-1-2");
        assert_eq!(slugify("Fix/Bug#12"), "fix-bug-12");
        assert_eq!(slugify("--a__b--"), "a-b");
        assert_eq!(slugify("🎉"), "");
    }

    #[test]
    fn branch_slug_never_empty_and_bounded() {
        assert!(branch_slug("🎉").starts_with("b-"));
        let long = "feature/".to_string() + &"x".repeat(80);
        let s = branch_slug(&long);
        assert!(s.len() <= BRANCH_SLUG_MAX, "{s}");
        assert_ne!(branch_slug(&(long.clone() + "a")), s);
        // Shell metacharacters never survive into a slug.
        assert_eq!(branch_slug("x$(id);rm"), "x-id-rm");
    }

    #[test]
    fn project_id_rules() {
        assert!(ProjectId::parse("myapp").is_ok());
        assert!(ProjectId::parse("my-app2").is_ok());
        assert_eq!(ProjectId::parse("").unwrap_err(), IdError::Empty);
        assert!(matches!(ProjectId::parse("My_App"), Err(IdError::BadChars(_))));
        assert!(matches!(ProjectId::parse("-x"), Err(IdError::BadChars(_))));
        assert_eq!(ProjectId::parse("api").unwrap_err().code(), "reserved_id");
        assert_eq!(ProjectId::from_name("My App").unwrap().as_str(), "my-app");
        assert!(ProjectId::from_name(&"a".repeat(100)).unwrap().as_str().len() <= PROJECT_ID_MAX);
    }

    #[test]
    fn instance_id_roundtrip() {
        let p = ProjectId::parse("myapp").unwrap();
        let prim = InstanceId::primary(&p);
        assert!(prim.is_primary());
        assert_eq!(prim.safe_name(), "myapp");
        let d = InstanceId::derived(&p, "feature-x").unwrap();
        assert_eq!(d.as_str(), "myapp@feature-x");
        assert_eq!(d.project(), p);
        assert_eq!(d.slug(), Some("feature-x"));
        assert_eq!(d.safe_name(), "myapp_feature-x");
        assert_eq!(InstanceId::from_safe_name("myapp_feature-x").unwrap(), d);
        assert_eq!(InstanceId::parse("myapp@feature-x").unwrap(), d);
        assert!(InstanceId::parse("myapp@a@b").is_err());
        assert!(InstanceId::parse("myapp@").is_err());
        assert!(InstanceId::derived(&p, "Bad Slug").is_err());
    }

    #[test]
    fn serde_validates() {
        let ok: InstanceId = serde_json::from_str("\"myapp@x\"").unwrap();
        assert_eq!(ok.as_str(), "myapp@x");
        assert!(serde_json::from_str::<InstanceId>("\"MyApp\"").is_err());
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"myapp@x\"");
    }

    #[test]
    fn domains() {
        assert_eq!(instance_domain("myapp.glow", None), "myapp.glow");
        assert_eq!(
            instance_domain("myapp.glow", Some("feature-x")),
            "feature-x.myapp.glow"
        );
        assert_eq!(detached_slug("ABCDEF1234"), "abcdef1");
    }
}

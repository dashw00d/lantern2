//! Safe template parsing for shell commands and data values (02 D1).

use crate::ids::{InstanceId, ProjectId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, net::IpAddr, path::PathBuf};

/// A supported template variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplateVar {
    Port,
    Host,
    Domain,
    Url,
    Project,
    Instance,
    Branch,
    BranchSlug,
    BranchSnake,
    Checkout,
    Primary,
}
impl TemplateVar {
    fn name(self) -> &'static str {
        match self {
            Self::Port => "PORT",
            Self::Host => "HOST",
            Self::Domain => "DOMAIN",
            Self::Url => "URL",
            Self::Project => "PROJECT",
            Self::Instance => "INSTANCE",
            Self::Branch => "BRANCH",
            Self::BranchSlug => "BRANCH_SLUG",
            Self::BranchSnake => "BRANCH_SNAKE",
            Self::Checkout => "CHECKOUT",
            Self::Primary => "PRIMARY",
        }
    }
    fn parse(name: &str) -> Option<Self> {
        match name {
            "PORT" => Some(Self::Port),
            "HOST" => Some(Self::Host),
            "DOMAIN" => Some(Self::Domain),
            "URL" => Some(Self::Url),
            "PROJECT" | "NAME" => Some(Self::Project),
            "INSTANCE" => Some(Self::Instance),
            "BRANCH" => Some(Self::Branch),
            "BRANCH_SLUG" => Some(Self::BranchSlug),
            "BRANCH_SNAKE" => Some(Self::BranchSnake),
            "CHECKOUT" | "PATH" => Some(Self::Checkout),
            "PRIMARY" => Some(Self::Primary),
            _ => None,
        }
    }
}
const NAMES: &[&str] = &[
    "PORT",
    "HOST",
    "DOMAIN",
    "URL",
    "PROJECT",
    "INSTANCE",
    "BRANCH",
    "BRANCH_SLUG",
    "BRANCH_SNAKE",
    "CHECKOUT",
    "PRIMARY",
];

/// A non-fatal template diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateLint {
    pub code: String,
    pub name: String,
    pub message: String,
    pub offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Segment {
    Lit(String),
    Var(TemplateVar),
}

/// Parsed template; shell rendering never incorporates values from a checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateString {
    source: String,
    segments: Vec<Segment>,
}

/// Values used for data-only textual expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarContext {
    pub port: u16,
    pub host: IpAddr,
    pub domain: String,
    pub url: String,
    pub project: ProjectId,
    pub instance: InstanceId,
    pub branch: Option<String>,
    pub branch_slug: Option<String>,
    pub checkout: PathBuf,
    pub primary: PathBuf,
}
impl VarContext {
    /// Set detached HEAD values to the same seven-character commit id.
    pub fn set_detached(&mut self, sha: &str) {
        let short = crate::ids::detached_slug(sha);
        self.branch = Some(short.clone());
        self.branch_slug = Some(short);
    }
    fn value(&self, v: TemplateVar) -> String {
        match v {
            TemplateVar::Port => self.port.to_string(),
            TemplateVar::Host => self.host.to_string(),
            TemplateVar::Domain => self.domain.clone(),
            TemplateVar::Url => self.url.clone(),
            TemplateVar::Project => self.project.to_string(),
            TemplateVar::Instance => self.instance.safe_name(),
            TemplateVar::Branch => self.branch.clone().unwrap_or_default(),
            TemplateVar::BranchSlug => self.branch_slug.clone().unwrap_or_default(),
            TemplateVar::BranchSnake => self.branch_slug.as_deref().unwrap_or_default().replace('-', "_"),
            TemplateVar::Checkout => self.checkout.to_string_lossy().into_owned(),
            TemplateVar::Primary => self.primary.to_string_lossy().into_owned(),
        }
    }
}

fn distance(a: &str, b: &str) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.bytes().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, y) in b.bytes().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (row[j + 1] + 1).min(row[j] + 1).min(prev + usize::from(x != y));
            prev = old;
        }
    }
    row[b.len()]
}

impl TemplateString {
    /// Parse without rejecting unknown shell variables.
    pub fn parse(src: &str) -> (Self, Vec<TemplateLint>) {
        Self::parse_with_env(src, &BTreeSet::new())
    }
    /// Parse while suppressing typo warnings for declared environment variables.
    pub fn parse_with_env(src: &str, declared: &BTreeSet<String>) -> (Self, Vec<TemplateLint>) {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut lints = Vec::new();
        let mut i = 0;
        let mut single = false;
        let mut double = false;
        let mut escaped = false;
        while i < src.len() {
            let tail = &src[i..];
            if tail.starts_with("$${") {
                literal.push_str("${");
                i += 3;
                continue;
            }
            if tail.starts_with("${")
                && let Some(end) = tail.find('}')
            {
                let name = &tail[2..end];
                if let Some(var) = TemplateVar::parse(name) {
                    if !literal.is_empty() {
                        segments.push(Segment::Lit(std::mem::take(&mut literal)));
                    }
                    segments.push(Segment::Var(var));
                    if name == "NAME" || name == "PATH" {
                        lints.push(TemplateLint { code: "deprecated_var".into(), name: name.into(), offset: i, message: if name == "PATH" { "${PATH} means the checkout directory, not the shell PATH; use ${CHECKOUT} (or $PATH for the shell path)".into() } else { "${NAME} is deprecated; use ${PROJECT}".into() } });
                    }
                    if single {
                        lints.push(TemplateLint {
                            code: "var_in_single_quotes".into(),
                            name: name.into(),
                            offset: i,
                            message: format!("${{{name}}} will not expand inside shell single quotes"),
                        });
                    }
                } else {
                    literal.push_str(&tail[..=end]);
                    if !declared.contains(name)
                        && !name.is_empty()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                        && let Some(near) = NAMES
                            .iter()
                            .min_by_key(|candidate| distance(name, candidate))
                            .filter(|candidate| distance(name, candidate) <= 2)
                    {
                        lints.push(TemplateLint {
                            code: "possible_var_typo".into(),
                            name: name.into(),
                            offset: i,
                            message: format!(
                                "${{{name}}} passes through unchanged; did you mean ${{{near}}}?"
                            ),
                        });
                    }
                }
                i += end + 1;
                continue;
            }
            let ch = tail.chars().next().expect("nonempty");
            literal.push(ch);
            i += ch.len_utf8();
            if escaped {
                escaped = false;
            } else if ch == '\\' && !single {
                escaped = true;
            } else if ch == '\'' && !double {
                single = !single;
            } else if ch == '"' && !single {
                double = !double;
            }
        }
        if !literal.is_empty() {
            segments.push(Segment::Lit(literal));
        }
        (
            Self {
                source: src.to_owned(),
                segments,
            },
            lints,
        )
    }
    /// Rewrite recognized variables to exported shell environment names.
    pub fn render_shell(&self) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Lit(s) => out.push_str(s),
                Segment::Var(v) => {
                    out.push_str("${LANTERN_");
                    out.push_str(v.name());
                    out.push('}');
                }
            }
        }
        out
    }
    /// Substitute values textually for paths and environment values.
    pub fn render_data(&self, ctx: &VarContext) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Lit(s) => out.push_str(s),
                Segment::Var(v) => out.push_str(&ctx.value(*v)),
            }
        }
        out
    }
    /// Return the original, unexpanded text.
    pub fn source(&self) -> &str {
        &self.source
    }
    /// Check a variable reference, including ordinary shell default expressions.
    pub fn mentions(&self, name: &str) -> bool {
        let mut i = 0;
        while i < self.source.len() {
            let tail = &self.source[i..];
            if tail.starts_with("$${") {
                i += 3;
                continue;
            }
            if let Some(after) = tail.strip_prefix('$') {
                let after = after.strip_prefix('{').unwrap_or(after);
                if after.starts_with(name)
                    && after[name.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                {
                    return true;
                }
            }
            i += tail.chars().next().expect("nonempty").len_utf8();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> VarContext {
        let p = ProjectId::parse("myapp").unwrap();
        VarContext {
            instance: InstanceId::derived(&p, "feature-x").unwrap(),
            project: p,
            port: 29003,
            host: "127.0.0.1".parse().unwrap(),
            domain: "a.glow".into(),
            url: "https://a.glow".into(),
            branch: Some("x$(id);rm".into()),
            branch_slug: Some("feature-x".into()),
            checkout: "/tmp/a".into(),
            primary: "/tmp/b".into(),
        }
    }
    #[test]
    fn shell_never_inserts_branch_data() {
        let (s, _) = TemplateString::parse("echo ${BRANCH} ${HOME} ${BRANCH_SLUG} ${PORT:-80}");
        let shell = s.render_shell();
        assert_eq!(
            shell,
            "echo ${LANTERN_BRANCH} ${HOME} ${LANTERN_BRANCH_SLUG} ${PORT:-80}"
        );
        assert!(!shell.contains("x$(id);rm"));
        assert!(s.mentions("PORT"));
        assert!(!s.mentions("POR"));
    }
    #[test]
    fn substitution_and_escape() {
        let (s, _) = TemplateString::parse("$${PORT} ${INSTANCE} ${BRANCH_SNAKE} ${NAME} ${PATH}");
        assert_eq!(
            s.render_data(&context()),
            "${PORT} myapp_feature-x feature_x myapp /tmp/a"
        );
        assert_eq!(
            s.render_shell(),
            "${PORT} ${LANTERN_INSTANCE} ${LANTERN_BRANCH_SNAKE} ${LANTERN_PROJECT} ${LANTERN_CHECKOUT}"
        );
    }
    #[test]
    fn lint_and_detached() {
        let (s, l) = TemplateString::parse("'${PATH}' ${PROT}");
        assert_eq!(
            l.iter().map(|x| x.code.as_str()).collect::<Vec<_>>(),
            ["deprecated_var", "var_in_single_quotes", "possible_var_typo"]
        );
        assert!(s.mentions("PATH"));
        let mut c = context();
        c.set_detached("ABCDEF123456");
        let (t, _) = TemplateString::parse("${BRANCH}/${BRANCH_SLUG}");
        assert_eq!(t.render_data(&c), "abcdef1/abcdef1");
    }
}

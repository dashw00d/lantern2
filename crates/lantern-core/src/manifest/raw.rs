//! Sparse, serializable manifest layers and lenient v1-compatible fields.
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use std::collections::BTreeMap;

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(vec![s])),
        Some(serde_json::Value::Array(a)) => a
            .into_iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| D::Error::custom("expected a string"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => Err(D::Error::custom("expected a string or list of strings")),
    }
}
fn string_or_list<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    let v = one_or_many(d)?;
    Ok(v.map(|v| {
        v.into_iter()
            .flat_map(|s| {
                s.split([',', '\n'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }))
}
fn lenient_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(v)) => Ok(Some(v)),
        Some(serde_json::Value::Number(n)) if n == 1.into() => Ok(Some(true)),
        Some(serde_json::Value::Number(n)) if n == 0.into() => Ok(Some(false)),
        Some(serde_json::Value::String(s)) => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(Some(true)),
            "false" | "no" | "off" | "0" => Ok(Some(false)),
            _ => Err(D::Error::custom("expected a boolean")),
        },
        _ => Err(D::Error::custom("expected a boolean")),
    }
}
fn scalar_map<'de, D: Deserializer<'de>>(d: D) -> Result<Option<BTreeMap<String, String>>, D::Error> {
    let v = Option::<BTreeMap<String, serde_json::Value>>::deserialize(d)?;
    v.map(|v| {
        v.into_iter()
            .map(|(k, x)| {
                let s = match x {
                    serde_json::Value::String(s) => s,
                    serde_json::Value::Bool(b) => b.to_string(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => return Err(D::Error::custom("expected a string, number or boolean")),
                };
                Ok((k, s))
            })
            .collect()
    })
    .transpose()
}

/// A sparse layer of manifest values; absent and explicitly empty lists differ.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawManifest {
    pub version: Option<u32>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub kind: Option<String>,
    #[serde(deserialize_with = "string_or_list")]
    pub tags: Option<Vec<String>>,
    #[serde(deserialize_with = "lenient_bool")]
    pub enabled: Option<bool>,
    pub repo_url: Option<String>,
    pub template: Option<String>,
    #[serde(rename = "type")]
    pub serve: Option<String>,
    pub domain: Option<String>,
    pub root: Option<String>,
    pub upstream_url: Option<String>,
    pub base_url: Option<String>,
    pub health_endpoint: Option<String>,
    pub run: Option<RunSection>,
    #[serde(deserialize_with = "one_or_many")]
    pub setup: Option<Vec<String>>,
    pub worktree: Option<WorktreeSection>,
    pub deploy: Option<DeploySection>,
    #[serde(deserialize_with = "string_or_list")]
    pub depends_on: Option<Vec<String>>,
    pub features: Option<FeaturesSection>,
    pub routing: Option<RoutingSection>,
    #[serde(deserialize_with = "docs_list")]
    pub docs: Option<Vec<DocEntry>>,
    pub docs_auto: Option<DocsAuto>,
    pub endpoints: Option<Vec<EndpointEntry>>,
    pub api_auto: Option<ApiAuto>,
}
impl RawManifest {
    /// Whether this sparse layer contains no values.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}
/// Project run command and environment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunSection {
    pub cmd: Option<String>,
    pub cwd: Option<String>,
    #[serde(deserialize_with = "scalar_map")]
    pub env: Option<BTreeMap<String, String>>,
    pub pre_start: Option<String>,
    pub post_stop: Option<String>,
}
/// Worktree copying, linking and environment overlay.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeSection {
    #[serde(deserialize_with = "one_or_many")]
    pub copy: Option<Vec<String>>,
    #[serde(deserialize_with = "one_or_many")]
    pub link: Option<Vec<String>>,
    #[serde(deserialize_with = "scalar_map")]
    pub env: Option<BTreeMap<String, String>>,
}
/// Shell overrides for project operations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeploySection {
    pub start: Option<String>,
    pub stop: Option<String>,
    pub restart: Option<String>,
    pub logs: Option<String>,
    pub status: Option<String>,
    pub install: Option<String>,
    pub env_file: Option<String>,
}
/// Feature switches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeaturesSection {
    #[serde(deserialize_with = "lenient_bool")]
    pub mailpit: Option<bool>,
    #[serde(deserialize_with = "lenient_bool")]
    pub auto_start: Option<bool>,
    #[serde(deserialize_with = "lenient_bool")]
    pub auto_open_browser: Option<bool>,
}
/// Routing and agent metadata.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingSection {
    #[serde(deserialize_with = "string_or_list")]
    pub aliases: Option<Vec<String>>,
    pub paths: Option<BTreeMap<String, String>>,
    #[serde(deserialize_with = "lenient_bool")]
    pub websocket: Option<bool>,
    #[serde(deserialize_with = "string_or_list")]
    pub triggers: Option<Vec<String>>,
    pub risk: Option<String>,
    #[serde(deserialize_with = "lenient_bool")]
    pub requires_confirmation: Option<bool>,
    pub max_concurrent: Option<u32>,
    #[serde(deserialize_with = "string_or_list")]
    pub agents: Option<Vec<String>>,
}
/// An explicitly listed document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, expecting = "a document path or mapping with a path")]
pub enum DocEntry {
    Path(String),
    Full { path: String, kind: Option<String> },
}
impl DocEntry {
    /// Path within the checkout.
    pub fn path(&self) -> &str {
        match self {
            Self::Path(p) | Self::Full { path: p, .. } => p,
        }
    }
}
fn docs_list<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<DocEntry>>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Array(a)) => serde_json::from_value(serde_json::Value::Array(a))
            .map(Some)
            .map_err(D::Error::custom),
        Some(x) => serde_json::from_value(x)
            .map(|x| Some(vec![x]))
            .map_err(D::Error::custom),
    }
}
/// Documentation discovery settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DocsAuto {
    #[serde(deserialize_with = "lenient_bool")]
    pub enabled: Option<bool>,
    #[serde(deserialize_with = "string_or_list")]
    pub patterns: Option<Vec<String>>,
    #[serde(deserialize_with = "string_or_list")]
    pub ignore: Option<Vec<String>>,
    pub max_files: Option<u32>,
    pub max_bytes: Option<u64>,
}
/// An explicit endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointEntry {
    pub method: Option<String>,
    pub path: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub risk: Option<String>,
    pub body_hint: Option<serde_json::Value>,
    pub params: Option<serde_json::Value>,
}
/// OpenAPI discovery settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiAuto {
    #[serde(deserialize_with = "lenient_bool")]
    pub enabled: Option<bool>,
    #[serde(deserialize_with = "string_or_list")]
    pub sources: Option<Vec<String>>,
    #[serde(deserialize_with = "string_or_list")]
    pub candidates: Option<Vec<String>>,
    pub timeout_ms: Option<u64>,
    pub max_endpoints: Option<u32>,
}

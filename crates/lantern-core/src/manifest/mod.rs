//! `lantern.yaml` schema, parsing, validation, resolution (docs/14-config.md §3.3-3.6). Owner: lane `config`.

use serde::{Deserialize, Serialize};

/// PLACEHOLDER from the base commit so `model::Instance` compiles. Lane `config` replaces it with the
/// real resolved manifest (docs/14-config.md §3.6), keeping the name `ResolvedManifest` and the
/// derives `Debug, Clone, PartialEq, Serialize, Deserialize`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ResolvedManifest {}

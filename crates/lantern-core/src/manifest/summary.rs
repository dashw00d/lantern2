//! `ResolvedManifest` → `api::ManifestSummary` (02 D7). Added by the controller at the wave-1 merge.

use super::{ResolvedManifest, Severity};
use crate::api::{DiagnosticCounts, ManifestSummary};

impl ResolvedManifest {
    /// Compact, secret-free summary for the event bus. `manifest_rev` is supplied by the registry
    /// (it changes whenever the resolved manifest is re-derived with a different result).
    pub fn summary(&self, manifest_rev: impl Into<String>) -> ManifestSummary {
        let mut counts = DiagnosticCounts {
            errors: 0,
            warnings: 0,
            info: 0,
        };
        for d in &self.diagnostics {
            match d.severity {
                Severity::Error => counts.errors += 1,
                Severity::Warning => counts.warnings += 1,
                Severity::Info => counts.info += 1,
            }
        }
        ManifestSummary {
            id: self.id.clone(),
            serve: self.serve,
            // The source text, never a rendered value: rendering could embed env data.
            run_cmd: self.run.cmd.as_ref().map(|c| c.source().to_string()),
            open_path: self.open_path.clone(),
            description: self.description.clone(),
            diagnostics: counts,
            manifest_rev: manifest_rev.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ResolveInput, TemplateDefinition, TemplateLookup, parse_manifest, resolve};
    use crate::manifest::raw::RawManifest;
    use crate::model::ServeMode;

    struct NoTemplates;
    impl TemplateLookup for NoTemplates {
        fn get(&self, _: &str) -> Option<TemplateDefinition> {
            None
        }
    }

    #[test]
    fn summary_counts_diagnostics_and_keeps_command_source() {
        let parsed = parse_manifest(
            "id: app\ntype: proxy\nrun: {cmd: 'pnpm dev --port ${PORT}', env: {SECRET: hunter2}}\nbogus_key: 1\n",
            "lantern.yaml",
        );
        let r = resolve(ResolveInput {
            dir_name: "app",
            is_primary: true,
            primary: Some(&parsed),
            checkout: None,
            overrides: &RawManifest::default(),
            templates: &NoTemplates,
            detection: &crate::detect::Detection::default(),
            settings: &crate::settings::Settings::default(),
        });
        let s = r.summary("rev-1");
        assert_eq!(s.id.as_str(), "app");
        assert_eq!(s.serve, ServeMode::Proxy);
        assert_eq!(s.run_cmd.as_deref(), Some("pnpm dev --port ${PORT}"));
        assert_eq!(s.manifest_rev, "rev-1");
        assert!(
            s.diagnostics.warnings >= 1,
            "unknown key should warn: {:?}",
            r.diagnostics
        );
        assert_eq!(s.diagnostics.errors, 0, "{:?}", r.diagnostics);
        // Env values never appear in the summary.
        assert!(!serde_json::to_string(&s).unwrap().contains("hunter2"));
    }

    #[test]
    fn critical_risk_and_unset_concurrency_survive_resolution() {
        let parsed = parse_manifest(
            "id: app\nrouting: {risk: critical}\nendpoints:\n  - {method: POST, path: /wipe, risk: critical}\n",
            "lantern.yaml",
        );
        let r = resolve(ResolveInput {
            dir_name: "app",
            is_primary: true,
            primary: Some(&parsed),
            checkout: None,
            overrides: &RawManifest::default(),
            templates: &NoTemplates,
            detection: &crate::detect::Detection::default(),
            settings: &crate::settings::Settings::default(),
        });
        assert_eq!(r.routing.risk.as_deref(), Some("critical"));
        assert_eq!(r.endpoints[0].risk.as_deref(), Some("critical"));
        assert_eq!(r.routing.max_concurrent, None, "unset means unlimited");
        assert!(
            r.diagnostics.iter().all(|d| d.code != "bad_value"),
            "{:?}",
            r.diagnostics
        );
    }
}

//! Offline JSON Schema generation for manifests, profiles and settings.
use crate::{manifest::raw::RawManifest, profiles::Profile, settings::Settings};
use schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::{Value, json};
/// Repository placeholder for the published manifest schema URL.
pub const SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/dashw00d/lantern/main/schema/lantern.schema.json";
fn generate<T: JsonSchema>() -> Value {
    let schema = SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<T>();
    serde_json::to_value(schema).expect("schema serializes")
}
/// Generate the manifest schema in draft-07 form.
pub fn manifest_json_schema() -> Value {
    let mut schema = generate::<RawManifest>();
    if let Some(o) = schema.as_object_mut() {
        o.insert("additionalProperties".into(), json!(true));
        o.insert("patternProperties".into(), json!({"^x-":{}}));
    }
    fn choices(schema: &mut Value, pointer: &str, allowed: &[&str]) {
        if let Some(prop) = schema.pointer_mut(pointer).and_then(Value::as_object_mut) {
            let mut values: Vec<Value> = allowed.iter().map(|s| json!(s)).collect();
            values.push(Value::Null);
            prop.insert("enum".into(), Value::Array(values));
        }
    }
    choices(
        &mut schema,
        "/properties/type",
        &["proxy", "php", "static", "unknown"],
    );
    choices(
        &mut schema,
        "/properties/kind",
        &["project", "service", "capability", "website", "tool"],
    );
    choices(
        &mut schema,
        "/definitions/RoutingSection/properties/risk",
        &["low", "medium", "high"],
    );
    choices(
        &mut schema,
        "/definitions/EndpointEntry/properties/risk",
        &["low", "medium", "high"],
    );
    schema
}
/// Generate the profile schema in draft-07 form.
pub fn profile_json_schema() -> Value {
    let mut schema = generate::<Profile>();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut)
        && let Some(mut alias) = properties.get("auto_start").cloned()
    {
        if let Some(object) = alias.as_object_mut() {
            object.insert("deprecated".into(), json!(true));
        }
        properties.insert("auto_start_projects".into(), alias);
    }
    schema
}
/// Generate the settings schema in draft-07 form.
pub fn config_json_schema() -> Value {
    generate::<Settings>()
}
/// Pretty-print a generated schema in its committed format.
pub fn render(schema: &Value) -> String {
    serde_json::to_string_pretty(schema).expect("schema serializes") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_files_are_current() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema");
        for (name, actual) in [
            ("lantern.schema.json", manifest_json_schema()),
            ("lantern-profile.schema.json", profile_json_schema()),
            ("lantern-config.schema.json", config_json_schema()),
        ] {
            let path = root.join(name);
            let expected = render(&actual);
            if std::env::var_os("LANTERN_UPDATE_SCHEMAS").is_some() {
                std::fs::write(&path, &expected).unwrap();
            }
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                expected,
                "{name} is stale"
            );
        }
    }
}

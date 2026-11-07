//! YAML boundary: bounded parsing with source positions and serialization.

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use std::fmt;

/// Maximum accepted YAML document size in bytes.
pub const MAX_BYTES: usize = 1024 * 1024;

/// One-based source position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

/// A value with the location where it was referenced.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub value: T,
    pub position: Position,
}

/// A YAML tree with positions for mapping keys and every value.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Seq(Vec<Spanned<Node>>),
    Map(Vec<(Spanned<String>, Spanned<Node>)>),
}

impl Node {
    /// Convert a tree to JSON for typed deserialization.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(v) => (*v).into(),
            Self::Number(v) => v.clone().into(),
            Self::String(v) => v.clone().into(),
            Self::Seq(v) => v.iter().map(|x| x.value.to_json()).collect(),
            Self::Map(v) => v
                .iter()
                .map(|(k, x)| (k.value.clone(), x.value.to_json()))
                .collect(),
        }
    }
}

fn convert<T>(value: serde_saphyr::Spanned<T>) -> Spanned<T> {
    Spanned {
        value: value.value,
        position: Position {
            line: value.referenced.line() as u32,
            column: value.referenced.column() as u32,
        },
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct NodeVisitor;
        impl<'de> Visitor<'de> for NodeVisitor {
            type Value = Node;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a YAML value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Node, E> {
                Ok(Node::Null)
            }
            fn visit_none<E: de::Error>(self) -> Result<Node, E> {
                Ok(Node::Null)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Node, E> {
                Ok(Node::Bool(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Node, E> {
                Ok(Node::Number(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Node, E> {
                Ok(Node::Number(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Node, E> {
                serde_json::Number::from_f64(v)
                    .map(Node::Number)
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Node, E> {
                Ok(Node::String(v.to_owned()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Node, E> {
                Ok(Node::String(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Node, A::Error> {
                let mut items = Vec::new();
                while let Some(v) = a.next_element::<serde_saphyr::Spanned<Node>>()? {
                    items.push(convert(v));
                }
                Ok(Node::Seq(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Node, A::Error> {
                let mut items = Vec::new();
                while let Some(k) = a.next_key::<serde_saphyr::Spanned<String>>()? {
                    let v = a.next_value::<serde_saphyr::Spanned<Node>>()?;
                    items.push((convert(k), convert(v)));
                }
                Ok(Node::Map(items))
            }
        }
        d.deserialize_any(NodeVisitor)
    }
}

/// YAML parser error with a stable category and optional source position.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub position: Option<Position>,
}

/// Parse a single bounded YAML document, preserving positions at every level.
pub fn parse_to_tree(src: &str) -> Result<Spanned<Node>, Error> {
    if src.len() > MAX_BYTES {
        return Err(Error {
            code: "file_too_large",
            message: "YAML exceeds 1 MiB".into(),
            position: None,
        });
    }
    let mut budget = serde_saphyr::Budget::default();
    budget.max_depth = 32;
    budget.max_nodes = 20_000;
    budget.max_aliases = 1_000;
    budget.max_events = 100_000;
    let mut options = serde_saphyr::Options::default();
    options.budget = Some(budget);
    options.duplicate_keys = serde_saphyr::DuplicateKeyPolicy::Error;
    options.strict_booleans = false;
    serde_saphyr::from_str_with_options::<serde_saphyr::Spanned<Node>>(
        src.trim_start_matches('\u{feff}'),
        options,
    )
    .map(convert)
    .map_err(|e| {
        let message = e.to_string();
        let lower = message.to_lowercase();
        let code = if lower.contains("duplicate") {
            "duplicate_key"
        } else if lower.contains("multiple document")
            || lower.contains("multiple yaml document")
            || lower.contains("more than one document")
        {
            "multiple_documents"
        } else if lower.contains("budget") || lower.contains("limit") {
            "yaml_limits"
        } else {
            "yaml_syntax"
        };
        Error {
            code,
            position: e.location().map(|p| Position {
                line: p.line() as u32,
                column: p.column() as u32,
            }),
            message,
        }
    })
}

/// Serialize a value as YAML.
pub fn to_string<T: Serialize>(value: &T) -> Result<String, String> {
    serde_saphyr::to_string(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_map_and_list_positions() {
        let tree = parse_to_tree("run:\n  env:\n    values:\n      - one\n      - key: value\n").unwrap();
        let Node::Map(root) = tree.value else { panic!() };
        assert_eq!(root[0].0.position, Position { line: 1, column: 1 });
        let Node::Map(run) = &root[0].1.value else {
            panic!()
        };
        let Node::Map(env) = &run[0].1.value else { panic!() };
        let Node::Seq(values) = &env[0].1.value else {
            panic!()
        };
        assert_eq!(values[0].position, Position { line: 4, column: 9 });
        let Node::Map(nested) = &values[1].value else {
            panic!()
        };
        assert_eq!(nested[0].0.position, Position { line: 5, column: 9 });
        assert_eq!(nested[0].1.position, Position { line: 5, column: 14 });
    }
    #[test]
    fn syntax_errors_have_positions() {
        let err = parse_to_tree("run:\n  cmd: [one,\n").unwrap_err();
        assert!(err.position.is_some(), "{err:?}");
    }
    #[test]
    fn nested_typed_error_has_location() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Run {
            cmd: String,
        }
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Manifest {
            run: Run,
        }
        let error = serde_saphyr::from_str::<Manifest>("run:\n  cmd: [one, two]\n").unwrap_err();
        let pos = error.location().unwrap();
        assert_eq!((pos.line(), pos.column()), (2, 8));
    }
    #[test]
    fn empty_and_multiple_documents() {
        assert!(matches!(
            parse_to_tree("# only a comment\n").unwrap().value,
            Node::Null
        ));
        let err = parse_to_tree("one: 1\n---\ntwo: 2\n").unwrap_err();
        assert_eq!(err.code, "multiple_documents");
    }
    #[test]
    fn duplicate_keys_fail() {
        assert_eq!(
            parse_to_tree("run: 1\nrun: 2\n").unwrap_err().code,
            "duplicate_key"
        );
    }
}

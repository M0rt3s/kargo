//! Reading a command's JSON Schema: which fields it has, what kind each one
//! is, how to turn command-line text into a typed value, and validation.
//!
//! Only the shapes schema generators commonly emit for flat command inputs are
//! understood for flag generation (scalars, enums, optional values, local
//! `$ref`s). Anything richer is accepted as a JSON string. Validation always
//! uses the full schema.

use jsonschema::{Retrieve, Uri, Validator};
use serde_json::{Map, Value};

use crate::errors::{Error, Result};
use crate::names;

/// How many `$ref` hops are followed before giving up (guards against cycles).
const MAX_REF_DEPTH: usize = 8;

/// The kind of value a field holds.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    /// A whole number.
    Integer,
    /// Any number.
    Number,
    /// `true` or `false`.
    Boolean,
    /// Free text.
    String,
    /// One of a fixed set of values.
    Enum(Vec<Value>),
    /// Anything structured; given on the command line as a JSON document.
    Json,
}

/// One field of a command's input object.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// Property name.
    pub name: String,
    /// Value kind.
    pub kind: FieldKind,
    /// Description from the schema, if any.
    pub description: Option<String>,
    /// Whether the schema lists the field as required.
    pub required: bool,
}

impl FieldKind {
    /// Name shown in `--help` value placeholders.
    pub fn placeholder(&self) -> &'static str {
        match self {
            FieldKind::Integer => "INT",
            FieldKind::Number => "NUM",
            FieldKind::Boolean => "BOOL",
            FieldKind::String => "TEXT",
            FieldKind::Enum(_) => "VALUE",
            FieldKind::Json => "JSON",
        }
    }

    /// The string choices of an enum, when all of them are strings.
    pub fn string_choices(&self) -> Option<Vec<String>> {
        match self {
            FieldKind::Enum(values) => values.iter().map(|v| v.as_str().map(str::to_owned)).collect(),
            _ => None,
        }
    }

    /// Convert command-line text into a JSON value of this kind.
    pub fn coerce(&self, raw: &str) -> std::result::Result<Value, String> {
        match self {
            FieldKind::String => Ok(Value::String(raw.to_owned())),
            FieldKind::Integer => raw
                .trim()
                .parse::<i64>()
                .map(Value::from)
                .or_else(|_| raw.trim().parse::<u64>().map(Value::from))
                .map_err(|_| format!("`{raw}` is not a whole number")),
            FieldKind::Number => raw
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| format!("`{raw}` is not a number")),
            FieldKind::Boolean => match raw.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" | "on" => Ok(Value::Bool(true)),
                "false" | "no" | "0" | "off" => Ok(Value::Bool(false)),
                _ => Err(format!("`{raw}` is not true or false")),
            },
            FieldKind::Enum(values) => values
                .iter()
                .find(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned) == raw)
                .cloned()
                .ok_or_else(|| format!("`{raw}` is not one of: {}", describe_choices(values))),
            FieldKind::Json => {
                serde_json::from_str(raw).map_err(|err| format!("`{raw}` is not valid JSON: {err}"))
            }
        }
    }
}

fn describe_choices(values: &[Value]) -> String {
    values
        .iter()
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Names of the input fields that `kargo` will not turn into flags.
pub fn rejected_fields(schema: &Value) -> Vec<&str> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    properties.keys().map(String::as_str).filter(|name| !names::is_valid(name)).collect()
}

/// List the fields of an object schema that have usable names. A schema with no properties
/// yields none.
pub fn fields(schema: &Value) -> Vec<Field> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    properties
        .iter()
        .filter(|(name, _)| names::is_valid(name))
        .map(|(name, node)| Field {
            name: name.clone(),
            kind: classify(schema, node, 0),
            description: description_of(schema, node),
            required: required.contains(&name.as_str()),
        })
        .collect()
}

fn description_of(root: &Value, node: &Value) -> Option<String> {
    let text = |n: &Value| n.get("description").and_then(Value::as_str).map(str::to_owned);
    text(node).or_else(|| text(resolve(root, node, 0)))
}

/// Follow local `$ref`s and single-element `allOf` wrappers.
fn resolve<'a>(root: &'a Value, node: &'a Value, depth: usize) -> &'a Value {
    if depth >= MAX_REF_DEPTH {
        return node;
    }
    if let Some(target) = node
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
    {
        return resolve(root, target, depth + 1);
    }
    if let Some([only]) = node.get("allOf").and_then(Value::as_array).map(Vec::as_slice) {
        return resolve(root, only, depth + 1);
    }
    node
}

fn classify(root: &Value, node: &Value, depth: usize) -> FieldKind {
    let node = resolve(root, node, 0);
    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        return FieldKind::Enum(values.clone());
    }
    if let Some(value) = node.get("const") {
        return FieldKind::Enum(vec![value.clone()]);
    }
    for key in ["oneOf", "anyOf"] {
        let Some(options) = node.get(key).and_then(Value::as_array) else { continue };
        let options: Vec<&Value> = options
            .iter()
            .map(|o| resolve(root, o, 0))
            .filter(|o| o.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if depth < MAX_REF_DEPTH {
            if let [only] = options.as_slice() {
                return classify(root, only, depth + 1);
            }
            let consts: Option<Vec<Value>> = options.iter().map(|o| o.get("const").cloned()).collect();
            if let Some(consts) = consts.filter(|c| !c.is_empty()) {
                return FieldKind::Enum(consts);
            }
        }
    }
    let kinds: Vec<&str> = match node.get("type") {
        Some(Value::String(single)) => vec![single.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    match kinds.iter().find(|k| **k != "null") {
        Some(&"integer") => FieldKind::Integer,
        Some(&"number") => FieldKind::Number,
        Some(&"boolean") => FieldKind::Boolean,
        Some(&"string") => FieldKind::String,
        _ => FieldKind::Json,
    }
}

/// Resolves nothing: command schemas must be self-contained, so any `$ref` that
/// leaves the document (`http://`, `file://`, ...) is refused rather than fetched.
struct LocalRefsOnly;

impl Retrieve for LocalRefsOnly {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("external reference `{uri}` is not allowed").into())
    }
}

/// Compile a command schema. Only references inside the document are honoured.
pub fn compile(schema: &Value) -> Result<Validator> {
    jsonschema::options()
        .with_retriever(LocalRefsOnly)
        .build(schema)
        .map_err(|err| Error::Schema(format!("the command's schema cannot be used: {err}")))
}

/// Compile `schema` and check `instance` against it in one go.
#[cfg(test)]
pub fn validate(schema: &Value, instance: &Value) -> Result<()> {
    check(&compile(schema)?, instance)
}

/// Check `instance` against an already compiled schema.
pub fn check(validator: &Validator, instance: &Value) -> Result<()> {
    let problems: Vec<String> = validator
        .iter_errors(instance)
        .map(|err| {
            let at = err.instance_path().to_string();
            if at.is_empty() { err.to_string() } else { format!("{at}: {err}") }
        })
        .collect();
    if problems.is_empty() { Ok(()) } else { Err(Error::Schema(problems.join("; "))) }
}

/// Build a command's JSON input from a base object and field overrides.
pub fn merge(base: Option<Value>, overrides: Map<String, Value>) -> Result<Map<String, Value>> {
    let mut merged = match base {
        None => Map::new(),
        Some(Value::Object(map)) => map,
        Some(_) => return Err(Error::Usage("--input must be a JSON object".into())),
    };
    merged.extend(overrides);
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {"type": "string", "description": "who"},
                "count": {"type": ["integer", "null"]},
                "loud": {"type": "boolean"},
                "style": {"$ref": "#/$defs/Style"},
                "mode": {"oneOf": [{"const": "fast"}, {"const": "slow"}]},
                "tags": {"type": "array", "items": {"type": "string"}}
            },
            "$defs": {"Style": {"type": "string", "enum": ["plain", "fancy"]}}
        })
    }

    fn kind_of(name: &str) -> FieldKind {
        fields(&schema()).into_iter().find(|f| f.name == name).unwrap().kind
    }

    #[test]
    fn classifies_common_shapes() {
        assert_eq!(kind_of("target"), FieldKind::String);
        assert_eq!(kind_of("count"), FieldKind::Integer);
        assert_eq!(kind_of("loud"), FieldKind::Boolean);
        assert_eq!(kind_of("style"), FieldKind::Enum(vec![json!("plain"), json!("fancy")]));
        assert_eq!(kind_of("mode"), FieldKind::Enum(vec![json!("fast"), json!("slow")]));
        assert_eq!(kind_of("tags"), FieldKind::Json);
    }

    #[test]
    fn fields_with_unusable_names_have_no_flags_but_still_validate() {
        let schema = json!({
            "type": "object",
            "required": ["-x"],
            "properties": {"-x": {"type": "string"}, "a b": {"type": "string"}, "k=v": {"type": "string"}, "ok": {"type": "string"}}
        });
        let names: Vec<String> = fields(&schema).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["ok"]);
        assert_eq!(rejected_fields(&schema), ["-x", "a b", "k=v"]);
        assert!(validate(&schema, &json!({"-x": "v"})).is_ok());
        assert!(validate(&schema, &json!({})).is_err());
    }

    #[test]
    fn required_and_description_are_reported() {
        let target = fields(&schema()).into_iter().find(|f| f.name == "target").unwrap();
        assert!(target.required);
        assert_eq!(target.description.as_deref(), Some("who"));
    }

    #[test]
    fn coercion_is_typed_and_strict() {
        assert_eq!(FieldKind::Integer.coerce("42"), Ok(json!(42)));
        assert!(FieldKind::Integer.coerce("4.2").is_err());
        assert_eq!(FieldKind::Boolean.coerce("Yes"), Ok(json!(true)));
        assert_eq!(FieldKind::Number.coerce("1.5"), Ok(json!(1.5)));
        assert!(FieldKind::Enum(vec![json!("a")]).coerce("b").is_err());
        assert_eq!(FieldKind::Json.coerce("[1,2]"), Ok(json!([1, 2])));
    }

    #[test]
    fn validation_reports_every_problem() {
        assert!(validate(&schema(), &json!({"target": "x"})).is_ok());
        let err = validate(&schema(), &json!({"count": "many"})).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("target"), "{text}");
        assert!(text.contains("count"), "{text}");
    }

    #[test]
    fn external_references_are_refused_without_panicking() {
        for reference in
            ["http://127.0.0.1:1/other.json", "file:///etc/passwd", "https://example.com/s.json#/x"]
        {
            let schema = json!({"type": "object", "properties": {"a": {"$ref": reference}}});
            let err = validate(&schema, &json!({"a": 1})).unwrap_err();
            assert!(matches!(err, Error::Schema(_)), "{reference}: {err:?}");
            assert_eq!(err.status().code(), 2);
        }
    }

    #[test]
    fn local_references_still_work() {
        let ok = validate(&schema(), &json!({"target": "x", "style": "plain"}));
        assert!(ok.is_ok(), "{ok:?}");
        assert!(validate(&schema(), &json!({"target": "x", "style": "nope"})).is_err());
    }

    #[test]
    fn merge_prefers_overrides_and_rejects_non_objects() {
        let merged = merge(Some(json!({"a": 1, "b": 2})), Map::from_iter([("b".into(), json!(3))])).unwrap();
        assert_eq!(Value::Object(merged), json!({"a": 1, "b": 3}));
        assert!(merge(Some(json!([1])), Map::new()).is_err());
    }
}

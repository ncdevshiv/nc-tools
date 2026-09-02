// JSON-Schema generation for tool args. Each tool's input schema is DERIVED
// from its typed args struct (schemars) — the schema can never drift from
// what the handler accepts, because they are the same type. A normalizer
// keeps the emitted shape identical to the v1 hand-written descriptors
// (optionality via required only, integer bounds, no generator noise), which
// the golden parity test locks against conformance/golden/tools.json.
use schemars::gen::SchemaGenerator;
use schemars::schema::Schema;
use serde_json::{json, Value};

/// Schema for a free-form object (batch.execute sub-call args): `{"type":"object"}`
/// with no additionalProperties restriction, exactly like the JS descriptor.
pub fn plain_object_schema(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(json!({ "type": "object" })).unwrap()
}

pub fn schema_for<T: schemars::JsonSchema>() -> Value {
    // JS descriptors inline nested object schemas — no $ref/definitions
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| s.inline_subschemas = true);
    let mut gen = settings.into_generator();
    let schema = T::json_schema(&mut gen);
    let mut v = serde_json::to_value(&schema).unwrap_or(Value::Null);
    normalize(&mut v);
    v
}

fn normalize(v: &mut Value) {
    shape_like_js(v);
    strip_format(v);
    if let Value::Object(map) = v {
        // top-level object schema: required is always present ([] when empty),
        // metadata noise from the generator is removed
        if map.get("type") == Some(&json!("object")) {
            map.entry("properties".to_string()).or_insert_with(|| json!({}));
            map.entry("required".to_string()).or_insert_with(|| Value::Array(vec![]));
        }
        map.remove("$schema");
        map.remove("$id");
        map.remove("title");
        map.remove("definitions");
    }
}

/// JS descriptor shape: optional fields are plain `{"type": T}` (optionality
/// lives in `required`), no `default` key, numeric bounds as integer literals
/// (and an implicit 0 minimum is never written), string enums carry "type".
fn shape_like_js(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get_mut("type") {
                let non_null: Vec<Value> = types
                    .iter()
                    .filter(|t| t.as_str() != Some("null") && **t != Value::Null)
                    .cloned()
                    .collect();
                if non_null.len() == 1 {
                    map.insert("type".into(), non_null[0].clone());
                }
            }
            map.remove("default");
            for key in ["minimum", "maximum", "minItems", "maxItems", "minLength", "maxLength"] {
                if let Some(Value::Number(n)) = map.get(key) {
                    if let Some(f) = n.as_f64() {
                        if f.fract() == 0.0 {
                            map.insert(key.into(), json!(f as i64));
                        }
                    }
                }
            }
            for key in ["minimum", "minItems", "minLength"] {
                if map.get(key) == Some(&json!(0)) {
                    map.remove(key);
                }
            }
            if let Some(Value::Array(e)) = map.get("enum") {
                if !e.is_empty() && e.iter().all(|x| x.is_string()) && !map.contains_key("type") {
                    map.insert("type".into(), json!("string"));
                }
            }
            for child in map.values_mut() {
                shape_like_js(child);
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                shape_like_js(child);
            }
        }
        _ => {}
    }
}

fn strip_format(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("format");
            for child in map.values_mut() {
                strip_format(child);
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                strip_format(child);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Probe {
        path: String,
        #[serde(default)]
        offset: Option<u64>,
    }

    #[test]
    fn optional_fields_lose_null_union() {
        let v = super::schema_for::<Probe>();
        let offset = &v["properties"]["offset"];
        assert_eq!(offset["type"], json!("integer"), "union not stripped: {v}");
        assert!(offset.get("default").is_none());
        assert_eq!(v["required"], json!(["path"]));
    }
}

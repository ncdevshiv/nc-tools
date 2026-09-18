// JSON-Schema generation for tool args. Each tool's input schema is DERIVED
// from its typed args struct (schemars) — the schema can never drift from
// what the handler accepts, because they are the same type. A normalizer
// keeps the emitted shape identical to the v1 hand-written descriptors
// (optionality via required only, integer bounds, no generator noise), which
// the golden parity test locks against conformance/golden/tools.json.
use schemars::gen::SchemaGenerator;
use schemars::schema::Schema;
use serde_json::{json, Value};

use crate::errors::{codes, ToolError};

/// Enforce the size/range constraints that `schema_for` advertises. The kernel
/// parses args with serde (which enforces types and required keys) but the
/// `#[schemars(range/length)]` bounds were documentation only — a call like
/// `retry:{times:1e6,delayMs:60000}` or `fs.read{limit:1e9}` was accepted and
/// could hang or allocate the server. This is the one boundary check, so every
/// advertised bound is now real without touching any handler.
///
/// Only the constraint kinds the generator emits are checked
/// (minimum/maximum/minItems/maxItems/minLength/maxLength/enum); any other key
/// is ignored so a hand-written schema cannot reject a valid call. A missing
/// key is never checked — a bound on an optional field must not make the field
/// mandatory, and serde already handles `required`.
pub fn check_bounds(schema: &Value, args: &Value) -> Result<(), ToolError> {
    check_bounds_at(schema, args, "args")
}

fn check_bounds_at(schema: &Value, value: &Value, path: &str) -> Result<(), ToolError> {
    if let Some(map) = schema.as_object() {
        // Numeric bounds — as_f64 covers u64/i64/f64 uniformly.
        if let Some(num) = value.as_f64() {
            if let Some(min) = num_bound(map, "minimum") {
                if num < min {
                    return Err(out_of_range(path, value, Some(min), None));
                }
            }
            if let Some(max) = num_bound(map, "maximum") {
                if num > max {
                    return Err(out_of_range(path, value, None, Some(max)));
                }
            }
        }
        // String length, in chars — `chars().count()`, not bytes.
        if let Some(s) = value.as_str() {
            let len = s.chars().count() as f64;
            if let Some(min) = num_bound(map, "minLength") {
                if len < min {
                    return Err(out_of_range(path, value, Some(min), None));
                }
            }
            if let Some(max) = num_bound(map, "maxLength") {
                if len > max {
                    return Err(out_of_range(path, value, None, Some(max)));
                }
            }
        }
        // Enum membership (string or number).
        if let Some(Value::Array(enums)) = map.get("enum") {
            if !enums.is_empty() && !enums.iter().any(|e| e == value) {
                return Err(ToolError::with_hint(
                    codes::BAD_INPUT,
                    format!("{path} must be one of {enums:?}, got {value}"),
                    json!({ "field": path, "value": value, "enum": enums }),
                ));
            }
        }
        // Arrays: count bounds, then recurse into each item.
        if let Some(arr) = value.as_array() {
            if let Some(min) = num_bound(map, "minItems") {
                if (arr.len() as f64) < min {
                    return Err(out_of_range(path, value, Some(min), None));
                }
            }
            if let Some(max) = num_bound(map, "maxItems") {
                if (arr.len() as f64) > max {
                    return Err(out_of_range(path, value, None, Some(max)));
                }
            }
            if let Some(items) = map.get("items") {
                for (i, item) in arr.iter().enumerate() {
                    check_bounds_at(items, item, &format!("{path}[{i}]"))?;
                }
            }
        }
        // Objects: recurse only into keys the caller actually passed.
        if let Some(Value::Object(props)) = map.get("properties") {
            if let Some(obj) = value.as_object() {
                for (key, v) in obj {
                    if let Some(prop_schema) = props.get(key) {
                        check_bounds_at(prop_schema, v, &format!("{path}.{key}"))?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn num_bound(map: &serde_json::Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key).and_then(|v| v.as_f64())
}

fn out_of_range(path: &str, value: &Value, min: Option<f64>, max: Option<f64>) -> ToolError {
    let msg = match (min, max) {
        (Some(lo), Some(hi)) => format!("{path}: {value} is out of range ({lo}..{hi})"),
        (Some(lo), None) => format!("{path}: {value} is below the minimum {lo}"),
        (None, Some(hi)) => format!("{path}: {value} exceeds the maximum {hi}"),
        (None, None) => unreachable!("out_of_range called with no bound"),
    };
    ToolError::with_hint(
        codes::BAD_INPUT,
        msg,
        json!({
            "field": path,
            "value": value,
            "min": min,
            "max": max,
        }),
    )
}

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
            map.entry("properties".to_string())
                .or_insert_with(|| json!({}));
            map.entry("required".to_string())
                .or_insert_with(|| Value::Array(vec![]));
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
            for key in [
                "minimum",
                "maximum",
                "minItems",
                "maxItems",
                "minLength",
                "maxLength",
            ] {
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
    use serde_json::{json, Value};
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
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

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct BoundsProbe {
        path: String,
        #[serde(default)]
        #[schemars(range(min = 1, max = 10))]
        limit: Option<u64>,
        #[serde(default)]
        #[schemars(length(min = 1, max = 4))]
        items: Vec<String>,
        #[serde(default)]
        #[schemars(range(min = 1, max = 30000))]
        delayMs: Option<u64>,
        #[serde(default)]
        label: Option<String>,
    }

    fn bounds_schema() -> Value {
        super::schema_for::<BoundsProbe>()
    }

    #[test]
    fn in_range_args_pass() {
        let v = json!({ "path": "a", "items": ["x"], "limit": 5, "delayMs": 1000, "label": "ok" });
        assert!(
            super::check_bounds(&bounds_schema(), &v).is_ok(),
            "in-range rejected"
        );
    }

    #[test]
    fn missing_optional_bound_is_not_mandatory() {
        // A bound on an optional field must not make the field required —
        // serde owns `required`, and callers omitting the key must still work.
        let v = json!({ "path": "a", "items": ["x"] });
        assert!(
            super::check_bounds(&bounds_schema(), &v).is_ok(),
            "omitted bounded field rejected"
        );
    }

    #[test]
    fn below_minimum_is_refused_with_the_field_path() {
        let err = super::check_bounds(
            &bounds_schema(),
            &json!({ "path": "a", "items": ["x"], "limit": 0 }),
        )
        .unwrap_err();
        assert_eq!(err.code, "ERR_BAD_INPUT");
        assert!(
            err.hint.as_ref().unwrap()["field"] == json!("args.limit"),
            "wrong field path: {:?}",
            err.hint
        );
        assert!(err.message.contains("below the minimum"), "{}", err.message);
    }

    #[test]
    fn above_maximum_is_refused_with_the_field_path() {
        let err = super::check_bounds(
            &bounds_schema(),
            &json!({ "path": "a", "items": ["x"], "limit": 11 }),
        )
        .unwrap_err();
        assert_eq!(err.code, "ERR_BAD_INPUT");
        assert!(err.hint.as_ref().unwrap()["field"] == json!("args.limit"));
        assert!(
            err.message.contains("exceeds the maximum"),
            "{}",
            err.message
        );
    }

    #[test]
    fn array_item_count_bounds_are_enforced() {
        assert!(
            super::check_bounds(&bounds_schema(), &json!({ "path": "a", "items": [] })).is_err(),
            "empty items accepted"
        );
        let five = json!({ "path": "a", "items": ["a", "b", "c", "d", "e"] });
        assert!(
            super::check_bounds(&bounds_schema(), &five).is_err(),
            "over-max items accepted"
        );
    }

    #[test]
    fn bounds_recurse_into_nested_calls() {
        // batch.execute-style free-form sub-calls: {type:"object"} has no
        // constraints, so any payload must pass unharmed.
        let v = json!({ "path": "a", "items": ["x"], "delayMs": 60000 });
        let err = super::check_bounds(&bounds_schema(), &v).unwrap_err();
        assert_eq!(err.hint.as_ref().unwrap()["field"], json!("args.delayMs"));
        let free = json!({ "type": "object" });
        assert!(super::check_bounds(&free, &json!({ "anything": 1_000_000 })).is_ok());
    }

    #[test]
    fn string_length_bounds_are_enforced_in_chars() {
        let v: Value = serde_json::from_str(
            r#"{"type":"object","required":["label"],"properties":{"label":{"type":"string","minLength":1,"maxLength":3}}}"#,
        )
        .unwrap();
        assert!(super::check_bounds(&v, &json!({ "label": "ab" })).is_ok());
        assert!(
            super::check_bounds(&v, &json!({ "label": "abcd" })).is_err(),
            "maxLength not enforced"
        );
        assert!(
            super::check_bounds(&v, &json!({ "label": "" })).is_err(),
            "minLength not enforced"
        );
    }
}

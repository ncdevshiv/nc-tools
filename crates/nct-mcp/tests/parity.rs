// Golden parity test: the Rust kernel's tool surface must structurally match
// conformance/golden/tools.json. That golden is regenerated FROM the Rust
// binary (`node tools/golden.mjs`), so the kernel is the source of truth —
// this test freezes the 60-tool surface and fails on any un-committed surface
// change until the golden is deliberately re-exported.
use serde_json::Value;

fn golden_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/golden/tools.json")
}

/// `required` is a SET (JSON array of names); schemars emits it sorted while
/// the JS descriptors use declaration order. Normalize both to sorted form —
/// membership, types and bounds still compare exactly.
fn sort_required(mut v: Value) -> Value {
    match &mut v {
        Value::Object(map) => {
            if let Some(Value::Array(req)) = map.get_mut("required") {
                req.sort_by(|a, b| {
                    a.as_str()
                        .unwrap_or_default()
                        .cmp(b.as_str().unwrap_or_default())
                });
            }
            for child in map.values_mut() {
                *child = sort_required(child.clone());
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                *child = sort_required(child.clone());
            }
        }
        _ => {}
    }
    v
}

#[test]
fn rust_surface_matches_frozen_golden() {
    let raw = std::fs::read_to_string(golden_path())
        .expect("golden spec missing — run node tools/golden.mjs in the repo root");
    let golden: Value = sort_required(serde_json::from_str(&raw).unwrap());
    let expected_tools: Vec<Value> = golden["tools"]
        .as_array()
        .expect("golden tools array")
        .clone();

    let kernel =
        nct_mcp::build_kernel(std::env::temp_dir().join("nc-parity-test")).expect("kernel");
    let actual: Vec<Value> = kernel
        .descriptors()
        .into_iter()
        .map(sort_required)
        .collect();

    // 1. exact tool count and name set
    assert_eq!(
        actual.len(),
        expected_tools.len(),
        "tool count mismatch: golden {} vs rust {}",
        expected_tools.len(),
        actual.len()
    );
    let expected_names: Vec<&str> = expected_tools
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let actual_names: Vec<&str> = actual.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for name in &expected_names {
        assert!(actual_names.contains(name), "missing tool: {name}");
    }
    for name in &actual_names {
        assert!(expected_names.contains(name), "extra tool: {name}");
    }

    // 2. per-tool structural parity: description + input schema
    let by_name = |v: &[Value], n: &str| v.iter().find(|t| t["name"] == *n).unwrap().clone();
    let mut diffs: Vec<String> = Vec::new();
    for et in &expected_tools {
        let name = et["name"].as_str().unwrap();
        let at = by_name(&actual, name);
        if et["description"] != at["description"] {
            diffs.push(format!(
                "{name}: description differs\n  golden: {}\n  rust:   {}",
                et["description"], at["description"]
            ));
        }
        if et["inputSchema"] != at["inputSchema"] {
            diffs.push(format!(
                "{name}: schema differs\n  golden: {}\n  rust:   {}",
                serde_json::to_string_pretty(&et["inputSchema"]).unwrap(),
                serde_json::to_string_pretty(&at["inputSchema"]).unwrap()
            ));
        }
    }
    assert!(
        diffs.is_empty(),
        "surface drift from the frozen golden ({} diff(s)):\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

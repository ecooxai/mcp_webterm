//! Tool result schemas are data, shared with wire-level conformance tests.
use serde_json::Value;
use std::sync::OnceLock;
pub fn schema(name: &str) -> Value {
    static SCHEMAS: OnceLock<Value> = OnceLock::new();
    SCHEMAS
        .get_or_init(|| {
            serde_json::from_str(include_str!("mcp_output_schemas.json"))
                .expect("valid bundled output schemas")
        })
        .get(name)
        .unwrap_or_else(|| panic!("Missing output schema for {name}"))
        .clone()
}

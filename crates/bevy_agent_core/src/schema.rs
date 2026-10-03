//! Pure, cached JSON Schema compilation; external retrieval is disabled.

use crate::{AgentControlError, ControlResult};
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn compile_schema(schema: &Value) -> ControlResult<Arc<jsonschema::Validator>> {
    if schema
        .get("$schema")
        .and_then(Value::as_str)
        .is_some_and(|draft| draft != "https://json-schema.org/draft/2020-12/schema")
    {
        return Err(AgentControlError::InvalidSchema(
            "only JSON Schema draft 2020-12 is supported".into(),
        ));
    }
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)
        .map(Arc::new)
        .map_err(|error| AgentControlError::InvalidSchema(error.to_string()))
}

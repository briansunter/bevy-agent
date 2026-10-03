//! JSON Schemas generated from the same types that cross the wire.

use crate::rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use bevy_agent_core::{
    AgentAction, AgentActionCatalog, AgentActionKind, AgentObservationCatalog,
    LOOK_PITCH_DELTA_LIMIT_RADIANS, LOOK_YAW_DELTA_LIMIT_RADIANS, Observation, ResetResponse,
    StepManyResponse, StepResponse,
};
use bevy_agent_runner::VisualCaptureResult;
use schemars::JsonSchema;
use serde_json::{Value, json};

fn generated<T: JsonSchema>(title: &str, name: &str) -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema is JSON");
    schema["$id"] = json!(format!("https://bevy-agent.rs/schemas/{name}.json"));
    schema["title"] = json!(title);
    schema
}

#[must_use]
pub fn agent_action_schema() -> Value {
    agent_action_schema_with_custom_actions(None)
}

#[must_use]
pub fn agent_action_schema_with_custom_actions(catalog: Option<&AgentActionCatalog>) -> Value {
    let mut schema = generated::<AgentAction>("AgentAction", "action");
    let Some(variants) = schema.get_mut("oneOf").and_then(Value::as_array_mut) else {
        panic!("internally tagged AgentAction schema must contain variants");
    };
    for variant in variants.iter_mut() {
        match variant
            .pointer("/properties/type/const")
            .and_then(Value::as_str)
        {
            Some("Move") => {
                for field in ["x", "y"] {
                    variant["properties"][field]["minimum"] = json!(-1);
                    variant["properties"][field]["maximum"] = json!(1);
                }
            }
            Some("Look") => {
                for (field, limit) in [
                    ("yaw_delta", LOOK_YAW_DELTA_LIMIT_RADIANS),
                    ("pitch_delta", LOOK_PITCH_DELTA_LIMIT_RADIANS),
                ] {
                    variant["properties"][field]["minimum"] = json!(-f64::from(limit));
                    variant["properties"][field]["maximum"] = json!(f64::from(limit));
                }
            }
            Some("Custom") => variant["properties"]["value"] = custom_action_value_schema(catalog),
            _ => {}
        }
    }
    if let Some(catalog) = catalog {
        variants.retain(|variant| {
            variant
                .pointer("/properties/type/const")
                .and_then(Value::as_str)
                .and_then(action_kind_from_wire_name)
                .is_some_and(|kind| catalog.supports(kind))
        });
        if variants.is_empty() {
            schema
                .as_object_mut()
                .expect("schema object")
                .remove("oneOf");
            schema["not"] = json!({});
        }
    }
    schema
}

#[must_use]
pub fn supported_action_names(catalog: Option<&AgentActionCatalog>) -> Vec<&'static str> {
    const ACTIONS: [(&str, AgentActionKind); 11] = [
        ("Noop", AgentActionKind::Noop),
        ("Move", AgentActionKind::Move),
        ("Look", AgentActionKind::Look),
        ("Jump", AgentActionKind::Jump),
        ("Crouch", AgentActionKind::Crouch),
        ("Sprint", AgentActionKind::Sprint),
        ("Interact", AgentActionKind::Interact),
        ("Attack", AgentActionKind::Attack),
        ("UseItem", AgentActionKind::UseItem),
        ("Dodge", AgentActionKind::Dodge),
        ("Custom", AgentActionKind::Custom),
    ];
    ACTIONS
        .iter()
        .filter(|(_, kind)| catalog.is_none_or(|catalog| catalog.supports(*kind)))
        .map(|(name, _)| *name)
        .collect()
}

fn action_kind_from_wire_name(name: &str) -> Option<AgentActionKind> {
    Some(match name {
        "Noop" => AgentActionKind::Noop,
        "Move" => AgentActionKind::Move,
        "Look" => AgentActionKind::Look,
        "Jump" => AgentActionKind::Jump,
        "Crouch" => AgentActionKind::Crouch,
        "Sprint" => AgentActionKind::Sprint,
        "Interact" => AgentActionKind::Interact,
        "Attack" => AgentActionKind::Attack,
        "UseItem" => AgentActionKind::UseItem,
        "Dodge" => AgentActionKind::Dodge,
        "Custom" => AgentActionKind::Custom,
        _ => return None,
    })
}

#[must_use]
pub fn custom_action_schema_map(catalog: Option<&AgentActionCatalog>) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(catalog) = catalog
        && catalog.supports(AgentActionKind::Custom)
    {
        for (name, schema) in catalog.custom_actions() {
            map.insert(name.clone(), schema.schema.clone());
        }
    }
    Value::Object(map)
}

fn custom_action_value_schema(catalog: Option<&AgentActionCatalog>) -> Value {
    let Some(catalog) = catalog else {
        return Value::Bool(true);
    };
    if catalog.custom_actions().is_empty() {
        return Value::Bool(false);
    }
    // Catalog validation uses the same union semantics, including overlapping schemas.
    json!({"anyOf": catalog.custom_actions().values().map(|registered| {
        let mut schema = registered.schema.clone();
        if let Some(object) = schema.as_object_mut() {
            // Each registered schema was compiled as its own resource. Keep
            // its local references in that scope when composing the union.
            object.entry("$id").or_insert_with(|| {
                let name = registered.name.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
                json!(format!("urn:bevy-agent:custom:{name}"))
            });
        }
        schema
    }).collect::<Vec<_>>()})
}

#[must_use]
pub fn observation_schema() -> Value {
    observation_schema_with_catalog(None)
}

#[must_use]
pub fn observation_schema_with_catalog(catalog: Option<&AgentObservationCatalog>) -> Value {
    let mut schema = catalog
        .and_then(AgentObservationCatalog::schema)
        .cloned()
        .unwrap_or_else(|| generated::<Observation>("Observation", "observation"));
    if schema.is_object() {
        schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        schema
            .as_object_mut()
            .expect("schema object")
            .entry("$id")
            .or_insert_with(|| json!("https://bevy-agent.rs/schemas/observation.json"));
        schema["title"] = json!("Observation");
    }
    schema
}

fn with_observation<T: JsonSchema>(
    catalog: Option<&AgentObservationCatalog>,
    title: &str,
    name: &str,
) -> Value {
    let mut schema = generated::<T>(title, name);
    if let Some(definitions) = schema.get_mut("$defs").and_then(Value::as_object_mut) {
        definitions.insert(
            "Observation".to_owned(),
            observation_schema_with_catalog(catalog),
        );
    }
    schema
}

#[must_use]
pub fn step_response_schema() -> Value {
    step_response_schema_with_catalog(None)
}

#[must_use]
pub fn step_response_schema_with_catalog(catalog: Option<&AgentObservationCatalog>) -> Value {
    with_observation::<StepResponse>(catalog, "StepResponse", "step-response")
}

#[must_use]
pub fn reset_response_schema() -> Value {
    reset_response_schema_with_catalog(None)
}

#[must_use]
pub fn reset_response_schema_with_catalog(catalog: Option<&AgentObservationCatalog>) -> Value {
    with_observation::<ResetResponse>(catalog, "ResetResponse", "reset-response")
}

#[must_use]
pub fn step_many_response_schema() -> Value {
    step_many_response_schema_with_catalog(None)
}

#[must_use]
pub fn step_many_response_schema_with_catalog(catalog: Option<&AgentObservationCatalog>) -> Value {
    with_observation::<StepManyResponse>(catalog, "StepManyResponse", "step-many-response")
}

#[must_use]
pub fn visual_capture_schema() -> Value {
    let mut schema = generated::<VisualCaptureResult>("VisualCapture", "visual-capture");
    schema["properties"]["format"]["const"] = json!("png");
    schema
}

#[must_use]
pub fn operation_status_schema() -> Value {
    let mut schema =
        generated::<crate::operations::OperationStatus>("OperationStatus", "operation-status");
    schema["properties"]["response"] =
        json!({"anyOf": [jsonrpc_response_schema(), {"type": "null"}]});
    schema
}

fn identifier_schema() -> Value {
    json!({"anyOf":[{"type":"string","maxLength":256},{"type":"number"},{"type":"null"}]})
}

#[must_use]
pub fn jsonrpc_request_schema() -> Value {
    let mut schema = generated::<JsonRpcRequest>("JsonRpcRequest", "json-rpc-request");
    schema["properties"]["jsonrpc"]["const"] = json!("2.0");
    schema["properties"]["id"] = identifier_schema();
    schema
}

#[must_use]
pub fn jsonrpc_error_schema() -> Value {
    generated::<JsonRpcError>("JsonRpcError", "json-rpc-error")
}

#[must_use]
pub fn jsonrpc_response_schema() -> Value {
    let mut schema = generated::<JsonRpcResponse>("JsonRpcResponse", "json-rpc-response");
    if let Some(variants) = schema.get_mut("anyOf").and_then(Value::as_array_mut) {
        for variant in variants {
            variant["properties"]["jsonrpc"]["const"] = json!("2.0");
            variant["properties"]["id"] = identifier_schema();
        }
    }
    schema
}

/// Structured error data emitted after reset or step mutation has begun.
#[must_use]
pub fn mutation_failure_schema() -> Value {
    generated::<bevy_agent_runner::MutationFailure>("MutationFailure", "mutation-failure")
}

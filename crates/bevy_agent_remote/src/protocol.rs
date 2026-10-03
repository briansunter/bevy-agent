//! Typed method contracts shared by decoding, dispatch, and discovery.

use crate::operations::OperationStatusParams;
use crate::{MAX_ACTIONS_PER_REQUEST, MAX_CAPTURE_TIMEOUT_FRAMES, MAX_TICKS_PER_REQUEST};
use bevy_agent_core::{
    AgentAction, AgentActionCatalog, AgentObservationCatalog, ControlMode, ObservationMode,
    SnapshotId,
};
use bevy_agent_runner::{CaptureSource, ReplayBundle, ResetOptions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionParams {
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepParams {
    pub action: AgentAction,
    #[serde(default)]
    pub observation_mode: Option<ObservationMode>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepManyParams {
    pub actions: Vec<AgentAction>,
    #[serde(default)]
    pub return_observations: ObservationReturn,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FastForwardParams {
    pub ticks: u64,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveParams {
    #[serde(default)]
    pub observation_mode: Option<ObservationMode>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResetParams {
    #[serde(default)]
    pub options: ResetOptions,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotParams {
    pub snapshot_id: SnapshotId,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchParams {
    pub from_tick: u64,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestoreTickParams {
    pub tick: u64,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlModeParams {
    pub mode: ControlMode,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplayExportParams {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplayLoadParams {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(with = "Option<Value>")]
    pub bundle: Option<ReplayBundle>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct VisualCaptureParams {
    #[serde(default)]
    pub(crate) output_dir: Option<std::path::PathBuf>,
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) timeout_frames: Option<u32>,
    #[serde(default)]
    pub(crate) source: Option<CaptureSource>,
    #[serde(default)]
    pub(crate) session_token: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ObservationReturn {
    None,
    Last,
    #[default]
    All,
}

macro_rules! methods {
    ($( $variant:ident => $name:literal : $params:ty ),+ $(,)?) => {
        /// The complete set of methods supported by the protocol.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum RpcMethod { $( $variant ),+ }

        impl RpcMethod {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }

            #[must_use]
            pub fn from_name(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant)),+, _ => None }
            }
        }

        /// A command has been decoded exactly once before entering the owner queue.
        #[derive(Debug)]
        pub(crate) enum RpcCommand { $( $variant($params) ),+ }

        impl RpcCommand {
            pub(crate) fn decode(method: RpcMethod, params: Value) -> Result<Self, serde_json::Error> {
                match method { $(RpcMethod::$variant => serde_json::from_value(params).map(Self::$variant)),+ }
            }

            pub(crate) const fn method(&self) -> RpcMethod {
                match self { $(Self::$variant(_) => RpcMethod::$variant),+ }
            }

            pub(crate) fn token(&self) -> Option<&str> {
                match self { $(Self::$variant(params) => params.session_token.as_deref()),+ }
            }

            pub(crate) fn serialized_params(&self) -> Value {
                match self { $(Self::$variant(params) => serde_json::to_value(params).unwrap()),+ }
            }
        }

        fn derived_params_schema(method: RpcMethod) -> Value {
            match method { $(RpcMethod::$variant => serde_json::to_value(schemars::schema_for!($params)).expect("schema is JSON")),+ }
        }
    };
}

methods! {
    Info => "agent.info": SessionParams,
    ActionSpace => "agent.action_space": SessionParams,
    ObservationSpace => "agent.observation_space": SessionParams,
    Schema => "agent.schema": SessionParams,
    Reset => "agent.reset": ResetParams,
    Step => "agent.step": StepParams,
    StepMany => "agent.step_many": StepManyParams,
    FastForward => "agent.fast_forward": FastForwardParams,
    Observe => "agent.observe": ObserveParams,
    VisualCapture => "agent.visual.capture": VisualCaptureParams,
    SnapshotCreate => "agent.snapshot.create": SessionParams,
    SnapshotRestore => "agent.snapshot.restore": SnapshotParams,
    SnapshotList => "agent.snapshot.list": SessionParams,
    SnapshotDelete => "agent.snapshot.delete": SnapshotParams,
    TimelineCurrent => "agent.timeline.current": SessionParams,
    TimelineBranch => "agent.timeline.branch": BranchParams,
    TimelineRestoreTick => "agent.timeline.restore_tick": RestoreTickParams,
    ControlSetMode => "agent.control.set_mode": ControlModeParams,
    ControlPause => "agent.control.pause": SessionParams,
    ControlResume => "agent.control.resume": SessionParams,
    ReplayStart => "agent.replay.start": SessionParams,
    ReplayStop => "agent.replay.stop": SessionParams,
    ReplayExport => "agent.replay.export": ReplayExportParams,
    ReplayLoad => "agent.replay.load": Box<ReplayLoadParams>,
    OperationStatus => "agent.operations.status": OperationStatusParams,
}

/// Discoverable parameter contracts are generated from the decoded DTOs.
/// Game catalogs narrow the generated action and observation definitions.
#[must_use]
pub fn method_params_schema(
    method: RpcMethod,
    actions: Option<&AgentActionCatalog>,
    observations: Option<&AgentObservationCatalog>,
) -> Value {
    let mut schema = derived_params_schema(method);
    if let Some(definitions) = schema.get_mut("$defs").and_then(Value::as_object_mut) {
        if definitions.contains_key("AgentAction") {
            let mut action = crate::schema::agent_action_schema_with_custom_actions(actions);
            action
                .as_object_mut()
                .expect("action schema object")
                .remove("$id");
            definitions.insert("AgentAction".to_owned(), action);
        }
        if let Some(modes) = observations
            && let Some(mode) = definitions.get_mut("ObservationMode")
        {
            mode["enum"] = serde_json::to_value(modes.supported_modes()).expect("modes are JSON");
        }
    }
    match method {
        RpcMethod::StepMany => {
            schema["properties"]["actions"]["maxItems"] = json!(MAX_ACTIONS_PER_REQUEST)
        }
        RpcMethod::FastForward => {
            schema["properties"]["ticks"]["minimum"] = json!(1);
            schema["properties"]["ticks"]["maximum"] = json!(MAX_TICKS_PER_REQUEST);
        }
        RpcMethod::VisualCapture => {
            // The field is optional and nullable, but a supplied integer has this range.
            schema["properties"]["timeout_frames"]["minimum"] = json!(1);
            schema["properties"]["timeout_frames"]["maximum"] = json!(MAX_CAPTURE_TIMEOUT_FRAMES);
        }
        RpcMethod::OperationStatus => {
            schema["oneOf"] = json!([
                {"required":["operation_id"],"properties":{"operation_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[0-9a-f-]+$"},"retry_key":{"type":"null"}}},
                {"required":["retry_key"],"properties":{"retry_key":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.:-]+$"},"operation_id":{"type":"null"}}}
            ]);
        }
        RpcMethod::ReplayLoad => {
            // Explicit null means absent, matching Option decoding.
            schema["oneOf"] = json!([
                {"required":["path"],"properties":{"path":{"type":"string"},"bundle":{"type":"null"}}},
                {"required":["bundle"],"properties":{"bundle":{"type":"object"},"path":{"type":"null"}}}
            ]);
            schema["properties"]["bundle"]["description"] = json!(
                "A format-3 ReplayBundle; the runner validates its typed snapshot, replay, and topology contract before installation."
            );
        }
        _ => {}
    }
    schema
}

#[must_use]
pub fn rpc_method_schema(
    actions: Option<&AgentActionCatalog>,
    observations: Option<&AgentObservationCatalog>,
) -> Value {
    Value::Object(
        RpcMethod::ALL
            .iter()
            .map(|method| {
                (
                    method.as_str().to_owned(),
                    method_params_schema(*method, actions, observations),
                )
            })
            .collect(),
    )
}

impl RpcCommand {
    /// Bounds that do not require access to the simulation are checked before admission.
    pub(crate) fn validate_limits(&self) -> Result<(), String> {
        match self {
            Self::OperationStatus(params) => params.validate(),
            Self::StepMany(params) if params.actions.len() > MAX_ACTIONS_PER_REQUEST => {
                Err(format!(
                    "actions length {} exceeds limit {MAX_ACTIONS_PER_REQUEST}",
                    params.actions.len()
                ))
            }
            Self::FastForward(params) if !(1..=MAX_TICKS_PER_REQUEST).contains(&params.ticks) => {
                Err(format!(
                    "ticks must be between 1 and {MAX_TICKS_PER_REQUEST}"
                ))
            }
            Self::VisualCapture(params)
                if params
                    .timeout_frames
                    .is_some_and(|frames| !(1..=MAX_CAPTURE_TIMEOUT_FRAMES).contains(&frames)) =>
            {
                Err(format!(
                    "timeout_frames must be between 1 and {MAX_CAPTURE_TIMEOUT_FRAMES}"
                ))
            }
            Self::ReplayLoad(params)
                if usize::from(params.path.is_some()) + usize::from(params.bundle.is_some())
                    != 1 =>
            {
                Err("agent.replay.load requires exactly one of: path, bundle".to_owned())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        JsonRpcBridge, MAX_MESSAGE_BYTES, RPC_INVALID_PARAMS, RPC_INVALID_REQUEST, RPC_PARSE_ERROR,
    };

    fn parameters(method: RpcMethod) -> Value {
        match method {
            RpcMethod::Step => json!({"action":{"type":"Noop"}}),
            RpcMethod::StepMany => json!({"actions":[{"type":"Move","x":1,"y":0}]}),
            RpcMethod::FastForward => json!({"ticks":1}),
            RpcMethod::SnapshotRestore | RpcMethod::SnapshotDelete => {
                json!({"snapshot_id":"00000000-0000-4000-8000-000000000001"})
            }
            RpcMethod::TimelineBranch => json!({"from_tick":0}),
            RpcMethod::TimelineRestoreTick => json!({"tick":0}),
            RpcMethod::ControlSetMode => json!({"mode":"Agent"}),
            RpcMethod::ReplayLoad => json!({"path":"replay.json"}),
            RpcMethod::OperationStatus => json!({"operation_id":"ab12-1"}),
            _ => json!({}),
        }
    }

    fn request(method: RpcMethod, params: Value) -> String {
        json!({"jsonrpc":"2.0","id":"correlation-17","method":method.as_str(),"params":params})
            .to_string()
    }

    #[test]
    fn every_method_contract_accepts_actual_dto_serialization_and_rejects_unknown_fields() {
        let bridge = JsonRpcBridge::default();
        for method in RpcMethod::ALL {
            assert_eq!(RpcMethod::from_name(method.as_str()), Some(*method));
            let schema = method_params_schema(*method, None, None);
            let validator = jsonschema::validator_for(&schema)
                .unwrap_or_else(|error| panic!("{}: {error}", method.as_str()));
            let supplied = parameters(*method);
            assert!(
                validator.is_valid(&supplied),
                "{}: {:?}",
                method.as_str(),
                validator.iter_errors(&supplied).collect::<Vec<_>>()
            );
            let prepared = bridge
                .prepare_request(&request(*method, supplied.clone()))
                .unwrap();
            assert_eq!(prepared.id(), "correlation-17");
            assert_eq!(prepared.method(), *method);
            let canonical = prepared.command.serialized_params();
            assert!(
                validator.is_valid(&canonical),
                "{} canonical: {canonical} {:?}",
                method.as_str(),
                validator.iter_errors(&canonical).collect::<Vec<_>>()
            );

            let mut unknown = supplied.clone();
            unknown["unknown_option"] = json!(true);
            assert!(
                !validator.is_valid(&unknown),
                "{} unknown field advertised",
                method.as_str()
            );
            let response = bridge
                .prepare_request(&request(*method, unknown))
                .unwrap_err();
            let response = serde_json::to_value(response).unwrap();
            assert_eq!(response["id"], "correlation-17");
            assert_eq!(
                response["error"]["code"],
                RPC_INVALID_PARAMS,
                "{}: {response}",
                method.as_str()
            );

            for required in schema["required"].as_array().into_iter().flatten() {
                let Some(field) = required.as_str() else {
                    panic!("required field name")
                };
                let mut missing = supplied.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert!(!validator.is_valid(&missing));
                assert!(
                    bridge.prepare_request(&request(*method, missing)).is_err(),
                    "{} accepts missing {field}",
                    method.as_str()
                );
            }
        }
    }

    #[test]
    fn parameter_limits_match_generated_contracts() {
        let bridge = JsonRpcBridge::default();
        for (method, params) in [
            (RpcMethod::FastForward, json!({"ticks":0})),
            (
                RpcMethod::FastForward,
                json!({"ticks":MAX_TICKS_PER_REQUEST+1}),
            ),
            (
                RpcMethod::StepMany,
                json!({"actions":vec![json!({"type":"Noop"}); MAX_ACTIONS_PER_REQUEST+1]}),
            ),
            (RpcMethod::VisualCapture, json!({"timeout_frames":0})),
            (
                RpcMethod::VisualCapture,
                json!({"timeout_frames":MAX_CAPTURE_TIMEOUT_FRAMES+1}),
            ),
            (RpcMethod::ReplayLoad, json!({})),
            (RpcMethod::ReplayLoad, json!({"path":null,"bundle":null})),
            (
                RpcMethod::Step,
                json!({"action":{"type":"Move","x":1.00000001,"y":0}}),
            ),
            (
                RpcMethod::Step,
                json!({"action":{"type":"Look","yaw_delta":f64::from(bevy_agent_core::LOOK_YAW_DELTA_LIMIT_RADIANS)+0.00000001,"pitch_delta":0}}),
            ),
        ] {
            let validator =
                jsonschema::validator_for(&method_params_schema(method, None, None)).unwrap();
            assert!(
                !validator.is_valid(&params),
                "{} advertises {params}",
                method.as_str()
            );
            let response = serde_json::to_value(
                bridge
                    .prepare_request(&request(method, params))
                    .unwrap_err(),
            )
            .unwrap();
            assert_eq!(response["error"]["code"], RPC_INVALID_PARAMS, "{response}");
            assert_eq!(response["id"], "correlation-17");
        }
    }

    #[test]
    fn request_decoder_bounds_depth_size_and_retains_valid_correlation() {
        let bridge = JsonRpcBridge::default();
        for input in [
            r#"{"jsonrpc":"2.0","id":"kept","method":"agent.info","extra":true}"#,
            r#"{"jsonrpc":"2.0","id":"kept","method":"agent.info","params":[]}"#,
            r#"{"jsonrpc":"2.0","id":"kept","method":"agent.info","params":null}"#,
            r#"{"jsonrpc":"2.0","id":"kept","method":"agent.info","params":{"session_token":"a","session_token":"b"}}"#,
            r#"{"jsonrpc":"2.0","id":"kept","method":"agent.step","params":{"action":{"type":"Custom","value":{"v":1,"v":2}}}}"#,
            r#"{"jsonrpc":"2.0","id":"kept","method":"agent.info","method":"agent.observe"}"#,
        ] {
            let response =
                serde_json::to_value(bridge.prepare_request(input).unwrap_err()).unwrap();
            assert_eq!(response["id"], "kept");
            assert_eq!(response["error"]["code"], RPC_INVALID_REQUEST, "{input}");
        }
        for input in [
            "{".to_owned(),
            r#"{"method":false,"id":"kept","jsonrpc":"2.0""#.to_owned(),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"deep\",\"method\":\"agent.info\",\"params\":{{\"v\":{}{}}}}}",
                "[".repeat(200),
                "]".repeat(200)
            ),
        ] {
            let response =
                serde_json::to_value(bridge.prepare_request(&input).unwrap_err()).unwrap();
            assert_eq!(response["id"], Value::Null);
            assert_eq!(response["error"]["code"], RPC_PARSE_ERROR);
        }
        let oversized = " ".repeat(MAX_MESSAGE_BYTES + 1);
        assert!(bridge.prepare_request(&oversized).is_err());
    }

    #[test]
    fn generated_parser_mutations_always_return_bounded_valid_envelopes() {
        let bridge = JsonRpcBridge::default();
        let baseline = request(RpcMethod::Step, parameters(RpcMethod::Step));
        let mut random = 0x6a09_e667_f3bc_c909u64;
        for _ in 0..800 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut bytes = baseline.as_bytes().to_vec();
            let index = (random as usize) % bytes.len();
            match random % 4 {
                0 => {
                    bytes.remove(index);
                }
                1 => bytes[index] = ((random >> 16) as u8 & 0x7f).max(1),
                2 => {
                    bytes.insert(index, b'{');
                }
                _ => bytes.truncate(index),
            }
            let input = std::str::from_utf8(&bytes).unwrap();
            let result = std::panic::catch_unwind(|| bridge.prepare_request(input));
            match result.expect("malformed input must not panic") {
                Ok(prepared) => assert_eq!(prepared.method(), RpcMethod::Step),
                Err(response) => {
                    let encoded = crate::rpc::serialize_response(&response);
                    assert!(encoded.len() < MAX_MESSAGE_BYTES);
                    let value: Value = serde_json::from_str(&encoded).unwrap();
                    assert_eq!(value["jsonrpc"], "2.0");
                    assert!(value["error"]["code"].is_i64());
                    assert!(matches!(
                        value["id"],
                        Value::Null | Value::String(_) | Value::Number(_)
                    ));
                }
            }
        }
    }
}

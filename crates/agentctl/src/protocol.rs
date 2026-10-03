//! JSON-RPC envelope creation and validation, independent of HTTP and CLI syntax.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn request(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": NEXT_ID.fetch_add(1, Ordering::Relaxed),
        "method": method,
        "params": params,
    })
}

/// A response can reach the CLI only after its envelope and correlation ID have
/// been checked. Keep the original JSON for CLI output.
#[derive(Debug)]
pub(crate) struct Response(Value);

impl Response {
    pub(crate) fn parse(value: Value, expected_id: &Value) -> Result<Self> {
        let envelope = value
            .as_object()
            .ok_or_else(|| anyhow!("JSON-RPC response must be an object"))?;
        if envelope.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(anyhow!("JSON-RPC response must declare jsonrpc 2.0"));
        }
        if envelope.get("id") != Some(expected_id) {
            return Err(anyhow!("JSON-RPC response id does not match the request"));
        }
        match (envelope.get("result"), envelope.get("error")) {
            (Some(_), None) => {}
            (None, Some(error)) => {
                let error = error
                    .as_object()
                    .ok_or_else(|| anyhow!("JSON-RPC error must be an object"))?;
                if error.get("code").and_then(Value::as_i64).is_none()
                    || error.get("message").and_then(Value::as_str).is_none()
                {
                    return Err(anyhow!(
                        "JSON-RPC error requires an integer code and string message"
                    ));
                }
            }
            _ => {
                return Err(anyhow!(
                    "JSON-RPC response must contain exactly one of result or error"
                ));
            }
        }
        Ok(Self(value))
    }

    pub(crate) fn value(&self) -> &Value {
        &self.0
    }

    pub(crate) fn error(&self) -> Option<&Value> {
        self.0.get("error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_results_including_null_and_well_formed_errors() {
        for result in [Value::Null, json!({"ok": true}), json!([1, 2])] {
            let response = Response::parse(
                json!({"jsonrpc": "2.0", "id": 1, "result": result}),
                &json!(1),
            )
            .unwrap();
            assert_eq!(response.value()["result"], result);
            assert!(response.error().is_none());
        }
        let value = json!({"jsonrpc": "2.0", "id": "request", "error": {
            "code": -32603, "message": "invalid session token", "data": {"retry": false}
        }});
        let response = Response::parse(value.clone(), &json!("request")).unwrap();
        assert_eq!(response.value(), &value);
        assert_eq!(response.error().unwrap()["code"], -32603);
    }

    #[test]
    fn rejects_missing_wrong_or_mistyped_version_and_id() {
        for value in [
            json!({"id": 1, "result": {}}),
            json!({"jsonrpc": "1.0", "id": 1, "result": {}}),
            json!({"jsonrpc": 2, "id": 1, "result": {}}),
            json!({"jsonrpc": "2.0", "result": {}}),
            json!({"jsonrpc": "2.0", "id": 2, "result": {}}),
            json!({"jsonrpc": "2.0", "id": "1", "result": {}}),
            json!({"jsonrpc": "2.0", "id": true, "result": {}}),
            json!({"jsonrpc": "2.0", "id": 1.0, "result": {}}),
            json!({"jsonrpc": "2.0", "id": null, "result": {}}),
        ] {
            assert!(
                Response::parse(value.clone(), &json!(1)).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn rejects_non_objects_and_ambiguous_or_absent_payloads() {
        for value in [
            Value::Null,
            json!([]),
            json!("success"),
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({"jsonrpc": "2.0", "id": 1, "result": {}, "error": null}),
            json!({"jsonrpc": "2.0", "id": 1, "result": {}, "error": {"code": -1, "message": "error"}}),
        ] {
            assert!(
                Response::parse(value.clone(), &json!(1)).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn rejects_malformed_error_objects() {
        for error in [
            Value::Null,
            json!("error"),
            json!([]),
            json!({}),
            json!({"code": -1}),
            json!({"message": "failed"}),
            json!({"code": "-1", "message": "failed"}),
            json!({"code": -1.5, "message": "failed"}),
            json!({"code": -1, "message": null}),
        ] {
            assert!(
                Response::parse(
                    json!({"jsonrpc": "2.0", "id": 1, "error": error}),
                    &json!(1)
                )
                .is_err(),
                "{error}"
            );
        }
    }

    #[test]
    fn requests_have_distinct_ids_and_the_expected_envelope() {
        let first = request("agent.info", json!({}));
        let second = request("agent.schema", json!({"session_token": "secret"}));
        assert_eq!(first["jsonrpc"], "2.0");
        assert_eq!(first["method"], "agent.info");
        assert_ne!(first["id"], second["id"]);
        assert_eq!(second["params"]["session_token"], "secret");
    }
}

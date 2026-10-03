//! CLI syntax and command-to-method mapping, independent of networking.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

/// Consume global options before parsing command-specific arguments.
pub(crate) struct GlobalOptions {
    pub(crate) url: String,
    pub(crate) token: Option<String>,
    pub(crate) retry_key: Option<String>,
}

impl GlobalOptions {
    pub(crate) fn parse(args: &mut Vec<String>) -> Result<Self> {
        Ok(Self {
            url: take_value_option(args, "--url")?
                .unwrap_or_else(|| "http://127.0.0.1:4000/rpc".to_string()),
            token: take_value_option(args, "--token")?,
            retry_key: take_value_option(args, "--retry-key")?,
        })
    }
}

pub(crate) fn ensure_no_leftover_args(args: &[String]) -> Result<()> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("unknown argument(s): {}", args.join(" ")))
    }
}

fn parse_observation_mode(value: &str) -> Result<String> {
    const VALID: &[&str] = &[
        "PlayerKnowledge",
        "FullDebugState",
        "DiffSinceLastTick",
        "PixelFrame",
        "Hybrid",
    ];
    if VALID.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(anyhow!(
            "invalid --mode {value:?}; expected one of {}",
            VALID.join("|")
        ))
    }
}

fn parse_return_mode(value: &str) -> Result<String> {
    const VALID: &[&str] = &["last", "all", "none"];
    if VALID.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(anyhow!(
            "invalid --return {value:?}; expected one of {}",
            VALID.join("|")
        ))
    }
}

fn parse_capture_source(value: &str) -> Result<String> {
    const VALID: &[&str] = &["auto", "software", "primary_window"];
    if VALID.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(anyhow!(
            "invalid --source {value:?}; expected one of {}",
            VALID.join("|")
        ))
    }
}

pub(crate) fn build_request(
    command: &str,
    args: &mut Vec<String>,
) -> Result<(&'static str, Value)> {
    match command {
        "info" => Ok(("agent.info", json!({}))),
        "schema" => Ok(("agent.schema", json!({}))),
        "action-space" => Ok(("agent.action_space", json!({}))),
        "observation-space" => Ok(("agent.observation_space", json!({}))),
        "operation-status" => {
            if let Some(key) = take_value_option(args, "--key")? {
                if key.is_empty() {
                    return Err(anyhow!("retry key must be nonempty"));
                }
                return Ok(("agent.operations.status", json!({"retry_key": key})));
            }
            let operation_id =
                take_positional(args, "operation-status requires an operation ID or --key")?;
            if operation_id.is_empty() {
                return Err(anyhow!("operation ID must be a nonempty opaque string"));
            }
            Ok((
                "agent.operations.status",
                json!({"operation_id": operation_id}),
            ))
        }
        "reset" => {
            // A bare reset matches ResetOptions::default() (seed: Some(0)); an
            // explicit --seed still wins. --seed without a value is an error.
            let seed = take_value_option(args, "--seed")?
                .map(|value| value.parse::<u64>())
                .transpose()?
                .unwrap_or(0);
            let observation_mode = match take_value_option(args, "--mode")? {
                Some(mode) => parse_observation_mode(&mode)?,
                None => "Hybrid".to_string(),
            };
            Ok((
                "agent.reset",
                json!({
                    "options": {
                        "seed": seed,
                        "observation_mode": observation_mode,
                        "create_initial_snapshot": true,
                    }
                }),
            ))
        }
        "step" => {
            let action = take_positional(args, "step requires an action JSON argument")?;
            let observation_mode = take_value_option(args, "--mode")?
                .map(|mode| parse_observation_mode(&mode))
                .transpose()?;
            Ok((
                "agent.step",
                json!({
                    "action": serde_json::from_str::<Value>(&action)?,
                    "observation_mode": observation_mode,
                }),
            ))
        }
        "step-many" => {
            let actions =
                take_positional(args, "step-many requires an action array JSON argument")?;
            let return_observations = match take_value_option(args, "--return")? {
                Some(value) => parse_return_mode(&value)?,
                None => "last".to_string(),
            };
            Ok((
                "agent.step_many",
                json!({
                    "actions": serde_json::from_str::<Value>(&actions)?,
                    "return_observations": return_observations,
                }),
            ))
        }
        "observe" => {
            let observation_mode = take_value_option(args, "--mode")?
                .map(|mode| parse_observation_mode(&mode))
                .transpose()?;
            Ok((
                "agent.observe",
                json!({
                    "observation_mode": observation_mode,
                }),
            ))
        }
        "capture" => {
            let output_dir =
                take_value_option(args, "--out-dir")?.unwrap_or_else(|| "screenshots".to_string());
            let label = take_value_option(args, "--label")?;
            let timeout_frames = take_value_option(args, "--timeout-frames")?
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(8);
            let source = match take_value_option(args, "--source")? {
                Some(value) => parse_capture_source(&value)?,
                None => "auto".to_string(),
            };
            Ok((
                "agent.visual.capture",
                json!({
                    "output_dir": output_dir,
                    "label": label,
                    "timeout_frames": timeout_frames,
                    "source": source,
                }),
            ))
        }
        "fast-forward" => {
            let ticks =
                take_positional(args, "fast-forward requires a tick count")?.parse::<u64>()?;
            Ok(("agent.fast_forward", json!({ "ticks": ticks })))
        }
        "snapshot" => Ok(("agent.snapshot.create", json!({}))),
        "snapshots" => Ok(("agent.snapshot.list", json!({}))),
        "restore" => {
            let snapshot_id = take_positional(args, "restore requires a snapshot id")?;
            Ok((
                "agent.snapshot.restore",
                json!({ "snapshot_id": snapshot_id }),
            ))
        }
        "restore-tick" => {
            let tick = take_positional(args, "restore-tick requires a tick")?.parse::<u64>()?;
            Ok(("agent.timeline.restore_tick", json!({ "tick": tick })))
        }
        "branch" => {
            let from_tick = take_value_option(args, "--from-tick")?
                .ok_or_else(|| anyhow!("branch requires --from-tick <tick>"))?
                .parse::<u64>()?;
            let label = take_value_option(args, "--label")?;
            Ok((
                "agent.timeline.branch",
                json!({ "from_tick": from_tick, "label": label }),
            ))
        }
        "replay-start" => Ok(("agent.replay.start", json!({}))),
        "replay-stop" => Ok(("agent.replay.stop", json!({}))),
        "replay-export" => {
            let path = if args.first().is_some_and(|arg| !arg.starts_with("--")) {
                Some(args.remove(0))
            } else {
                None
            };
            Ok(("agent.replay.export", json!({ "path": path })))
        }
        "replay-load" => {
            let path = take_positional(args, "replay-load requires a path")?;
            Ok(("agent.replay.load", json!({ "path": path })))
        }
        _ => Err(anyhow!("unknown command {command}")),
    }
}

fn take_positional(args: &mut Vec<String>, missing_message: &str) -> Result<String> {
    if args.first().is_none_or(|value| value.starts_with("--")) {
        return Err(anyhow!("{missing_message}"));
    }
    Ok(args.remove(0))
}

/// Extract an option, rejecting a missing value or another option in its place.
/// Returns `Ok(None)`
/// when the option is absent, so callers can still apply their own default.
fn take_value_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    if args
        .get(index + 1)
        .is_none_or(|value| value.starts_with("--"))
    {
        return Err(anyhow!("{name} requires a value"));
    }
    args.remove(index);
    Ok(Some(args.remove(index)))
}

pub(crate) fn print_usage() {
    eprintln!(
        "Usage: agentctl [--url http://127.0.0.1:4000/rpc] [--token TOKEN] [--retry-key KEY] <command>\n\
         Commands:\n\
           info | schema | action-space | observation-space\n\
           operation-status <operation-id> | --key <retry-key>\n\
           reset [--seed N] [--mode Hybrid]\n\
           step '<action-json>' [--mode Hybrid]\n\
           step-many '<actions-json-array>' [--return last|all|none]\n\
           observe [--mode Hybrid]\n\
           capture [--out-dir screenshots] [--label name] [--timeout-frames N] [--source auto|software|primary_window]\n\
           fast-forward <ticks>\n\
           snapshot | snapshots | restore <snapshot-id> | restore-tick <tick>\n\
           branch --from-tick <tick> [--label name]\n\
           replay-start | replay-stop | replay-export [path] | replay-load <path>"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_status_preserves_opaque_identity() {
        let mut args = vec!["e0cfac39a-1".to_string()];
        let (method, params) = build_request("operation-status", &mut args).unwrap();
        assert_eq!(method, "agent.operations.status");
        assert_eq!(params, json!({"operation_id": "e0cfac39a-1"}));
        assert!(args.is_empty());
        assert!(build_request("operation-status", &mut vec![String::new()]).is_err());
        assert!(build_request("operation-status", &mut Vec::new()).is_err());
    }

    #[test]
    fn take_value_option_removes_option_and_value() {
        let mut args = vec![
            "reset".to_string(),
            "--seed".to_string(),
            "42".to_string(),
            "--mode".to_string(),
            "Hybrid".to_string(),
        ];

        assert_eq!(
            take_value_option(&mut args, "--seed").unwrap(),
            Some("42".to_string())
        );
        assert_eq!(args, vec!["reset", "--mode", "Hybrid"]);
    }

    #[test]
    fn build_request_creates_step_many_payload() {
        let mut args = vec![
            r#"[{"type":"Noop"}]"#.to_string(),
            "--return".to_string(),
            "none".to_string(),
        ];

        let (method, params) = build_request("step-many", &mut args).unwrap();

        assert_eq!(method, "agent.step_many");
        assert_eq!(params["return_observations"], "none");
        assert_eq!(params["actions"][0]["type"], "Noop");
    }

    #[test]
    fn build_request_reports_missing_required_arg() {
        let mut args = Vec::new();

        let error = build_request("restore", &mut args).unwrap_err();

        assert!(error.to_string().contains("snapshot id"));
    }

    #[test]
    fn build_request_covers_common_commands() {
        let cases = [
            ("info", vec![], "agent.info"),
            ("schema", vec![], "agent.schema"),
            ("action-space", vec![], "agent.action_space"),
            ("observation-space", vec![], "agent.observation_space"),
            ("observe", vec!["--mode", "FullDebugState"], "agent.observe"),
            (
                "capture",
                vec!["--out-dir", "shots"],
                "agent.visual.capture",
            ),
            ("fast-forward", vec!["5"], "agent.fast_forward"),
            ("snapshot", vec![], "agent.snapshot.create"),
            ("snapshots", vec![], "agent.snapshot.list"),
            ("restore", vec!["snapshot-id"], "agent.snapshot.restore"),
            ("restore-tick", vec!["9"], "agent.timeline.restore_tick"),
            ("replay-start", vec![], "agent.replay.start"),
            ("replay-stop", vec![], "agent.replay.stop"),
            ("replay-export", vec!["replay.json"], "agent.replay.export"),
            ("replay-load", vec!["replay.json"], "agent.replay.load"),
        ];

        for (command, args, expected_method) in cases {
            let mut args = args.into_iter().map(ToOwned::to_owned).collect::<Vec<_>>();
            let (method, _) = build_request(command, &mut args).unwrap();
            assert_eq!(method, expected_method);
        }
    }

    #[test]
    fn build_request_covers_reset_step_and_branch_options() {
        let mut reset = vec![
            "--seed".to_string(),
            "12".to_string(),
            "--mode".to_string(),
            "PlayerKnowledge".to_string(),
        ];
        let (_, reset_params) = build_request("reset", &mut reset).unwrap();
        assert_eq!(reset_params["options"]["seed"], 12);
        assert_eq!(
            reset_params["options"]["observation_mode"],
            "PlayerKnowledge"
        );

        let mut step = vec![
            r#"{"type":"Move","x":1.0,"y":0.0}"#.to_string(),
            "--mode".to_string(),
            "Hybrid".to_string(),
        ];
        let (_, step_params) = build_request("step", &mut step).unwrap();
        assert_eq!(step_params["action"]["type"], "Move");
        assert_eq!(step_params["observation_mode"], "Hybrid");

        let mut branch = vec![
            "--from-tick".to_string(),
            "42".to_string(),
            "--label".to_string(),
            "try-alt".to_string(),
        ];
        let (_, branch_params) = build_request("branch", &mut branch).unwrap();
        assert_eq!(branch_params["from_tick"], 42);
        assert_eq!(branch_params["label"], "try-alt");

        let mut capture = vec![
            "--out-dir".to_string(),
            "shots".to_string(),
            "--label".to_string(),
            "after-step".to_string(),
            "--timeout-frames".to_string(),
            "3".to_string(),
        ];
        let (_, capture_params) = build_request("capture", &mut capture).unwrap();
        assert_eq!(capture_params["output_dir"], "shots");
        assert_eq!(capture_params["label"], "after-step");
        assert_eq!(capture_params["timeout_frames"], 3);
    }

    #[test]
    fn bare_reset_serializes_the_default_seed_zero() {
        // A bare reset must match ResetOptions::default(), whose seed is Some(0),
        // rather than sending a null seed that skips reseeding entirely.
        let mut args: Vec<String> = Vec::new();

        let (_, params) = build_request("reset", &mut args).unwrap();

        assert_eq!(params["options"]["seed"], 0);
        assert_eq!(params["options"]["observation_mode"], "Hybrid");
        assert_eq!(params["options"]["create_initial_snapshot"], true);
    }

    #[test]
    fn explicit_seed_overrides_the_default() {
        let mut args = vec!["--seed".to_string(), "42".to_string()];

        let (_, params) = build_request("reset", &mut args).unwrap();

        assert_eq!(params["options"]["seed"], 42);
    }

    #[test]
    fn reset_seed_without_a_value_errors() {
        let mut args = vec!["--seed".to_string()];

        let error = build_request("reset", &mut args).unwrap_err();

        assert!(
            error.to_string().contains("--seed requires a value"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn leftover_args_are_rejected() {
        let args = vec!["--bogus".to_string()];
        let error = ensure_no_leftover_args(&args).unwrap_err();
        assert!(error.to_string().contains("unknown argument"));

        // Positional commands must consume their inputs; leftovers error.
        let mut step = vec![r#"{"type":"Noop"}"#.to_string(), "--extra".to_string()];
        build_request("step", &mut step).unwrap();
        let error = ensure_no_leftover_args(&step).unwrap_err();
        assert!(error.to_string().contains("--extra"));
    }

    #[test]
    fn invalid_mode_and_return_values_are_rejected() {
        let mut args = vec![
            r#"{"type":"Noop"}"#.to_string(),
            "--mode".to_string(),
            "Nope".to_string(),
        ];
        assert!(build_request("step", &mut args).is_err());

        let mut args = vec![
            r#"[]"#.to_string(),
            "--return".to_string(),
            "everything".to_string(),
        ];
        assert!(build_request("step-many", &mut args).is_err());

        let mut args = vec!["--source".to_string(), "bogus".to_string()];
        assert!(build_request("capture", &mut args).is_err());
    }

    #[test]
    fn missing_option_values_error_strictly() {
        for option in ["--out-dir", "--label", "--timeout-frames", "--source"] {
            let mut args = vec![option.to_string()];
            assert!(
                build_request("capture", &mut args).is_err(),
                "{option} should require a value"
            );
        }
        let mut args = vec![r#"{"type":"Noop"}"#.to_string(), "--mode".to_string()];
        assert!(build_request("step", &mut args).is_err());
        let mut args = vec!["--from-tick".to_string()];
        assert!(build_request("branch", &mut args).is_err());
    }

    #[test]
    fn capture_includes_source_param() {
        let mut args = vec!["--source".to_string(), "software".to_string()];
        let (_, params) = build_request("capture", &mut args).unwrap();
        assert_eq!(params["source"], "software");
        assert!(ensure_no_leftover_args(&args).is_ok());

        let mut defaults: Vec<String> = Vec::new();
        let (_, params) = build_request("capture", &mut defaults).unwrap();
        assert_eq!(params["source"], "auto");
    }

    #[test]
    fn option_followed_by_another_option_is_a_missing_value() {
        for option in [
            "--url",
            "--token",
            "--seed",
            "--mode",
            "--return",
            "--out-dir",
            "--label",
            "--timeout-frames",
            "--source",
            "--from-tick",
        ] {
            let mut args = vec![
                option.to_string(),
                "--next-option".to_string(),
                "value".to_string(),
            ];
            let original = args.clone();
            let error = take_value_option(&mut args, option).unwrap_err();
            assert_eq!(error.to_string(), format!("{option} requires a value"));
            assert_eq!(
                args, original,
                "failed parsing must preserve the following option"
            );
        }
    }

    #[test]
    fn global_option_values_and_command_options_are_parsed_independently() {
        let mut args = vec![
            "reset",
            "--url",
            "http://localhost:4001/rpc",
            "--token",
            "secret",
            "--seed",
            "5",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
        let globals = GlobalOptions::parse(&mut args).unwrap();
        assert_eq!(globals.url, "http://localhost:4001/rpc");
        assert_eq!(globals.token.as_deref(), Some("secret"));
        let command = args.remove(0);
        let (_, params) = build_request(&command, &mut args).unwrap();
        assert_eq!(params["options"]["seed"], 5);
        assert!(ensure_no_leftover_args(&args).is_ok());
        for args in [vec!["--url"], vec!["--token", "--url", "http://localhost"]] {
            let mut args = args.into_iter().map(ToOwned::to_owned).collect();
            assert!(GlobalOptions::parse(&mut args).is_err());
        }
    }

    #[test]
    fn required_positionals_do_not_consume_options() {
        for command in [
            "step",
            "step-many",
            "fast-forward",
            "restore",
            "restore-tick",
            "replay-load",
        ] {
            let mut args = vec!["--label".to_string()];
            assert!(build_request(command, &mut args).is_err(), "{command}");
            assert_eq!(args, ["--label"]);
        }
    }
}

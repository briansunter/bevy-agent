use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let url = take_value_option(&mut args, "--url")?
        .unwrap_or_else(|| "http://127.0.0.1:4000/rpc".to_string());
    let token =
        take_value_option(&mut args, "--token")?.or_else(|| std::env::var("AGENT_TOKEN").ok());

    if args.is_empty() {
        print_usage();
        std::process::exit(2);
    }

    let command = args.remove(0);
    let (method, mut params) = build_request(&command, &mut args)?;
    ensure_no_leftover_args(&args)?;
    if let Some(token) = token {
        params["session_token"] = Value::String(token);
    }

    let request = json!({
        "jsonrpc": "2.0",
        "id": NEXT_ID.fetch_add(1, Ordering::Relaxed),
        "method": method,
        "params": params,
    });
    let response = post_json_rpc(&url, &request)?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    if is_jsonrpc_error(&response) {
        eprintln!("json-rpc error: {}", response["error"]);
        std::process::exit(2);
    }
    Ok(())
}

fn ensure_no_leftover_args(args: &[String]) -> Result<()> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("unknown argument(s): {}", args.join(" ")))
    }
}

fn is_jsonrpc_error(response: &Value) -> bool {
    response.get("error").is_some_and(|error| !error.is_null())
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

fn build_request(command: &str, args: &mut Vec<String>) -> Result<(&'static str, Value)> {
    match command {
        "info" => Ok(("agent.info", json!({}))),
        "schema" => Ok(("agent.schema", json!({}))),
        "action-space" => Ok(("agent.action_space", json!({}))),
        "observation-space" => Ok(("agent.observation_space", json!({}))),
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
            if args.is_empty() {
                return Err(anyhow!("step requires an action JSON argument"));
            }
            let action = args.remove(0);
            let observation_mode = match take_value_option(args, "--mode")? {
                Some(mode) => parse_observation_mode(&mode)?,
                None => "Hybrid".to_string(),
            };
            Ok((
                "agent.step",
                json!({
                    "action": serde_json::from_str::<Value>(&action)?,
                    "observation_mode": observation_mode,
                }),
            ))
        }
        "step-many" => {
            if args.is_empty() {
                return Err(anyhow!("step-many requires an action array JSON argument"));
            }
            let actions = args.remove(0);
            let return_observations = match take_value_option(args, "--return")? {
                Some(value) => parse_return_mode(&value)?,
                None => "last".to_string(),
            };
            Ok((
                "agent.step_many",
                json!({
                    "actions": serde_json::from_str::<Value>(&actions)?,
                    "stop_on_done": !has_flag(args, "--no-stop-on-done"),
                    "return_observations": return_observations,
                }),
            ))
        }
        "observe" => {
            let observation_mode = match take_value_option(args, "--mode")? {
                Some(mode) => parse_observation_mode(&mode)?,
                None => "Hybrid".to_string(),
            };
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
            if args.is_empty() {
                return Err(anyhow!("fast-forward requires a tick count"));
            }
            let ticks = args.remove(0).parse::<u64>()?;
            Ok(("agent.fast_forward", json!({ "ticks": ticks })))
        }
        "snapshot" => Ok(("agent.snapshot.create", json!({}))),
        "snapshots" => Ok(("agent.snapshot.list", json!({}))),
        "restore" => {
            if args.is_empty() {
                return Err(anyhow!("restore requires a snapshot id"));
            }
            let snapshot_id = args.remove(0);
            Ok((
                "agent.snapshot.restore",
                json!({ "snapshot_id": snapshot_id }),
            ))
        }
        "restore-tick" => {
            if args.is_empty() {
                return Err(anyhow!("restore-tick requires a tick"));
            }
            let tick = args.remove(0).parse::<u64>()?;
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
            if args.is_empty() {
                return Err(anyhow!("replay-load requires a path"));
            }
            let path = args.remove(0);
            Ok(("agent.replay.load", json!({ "path": path })))
        }
        _ => Err(anyhow!("unknown command {command}")),
    }
}

fn post_json_rpc(url: &str, request: &Value) -> Result<Value> {
    let endpoint = HttpEndpoint::parse(url)?;
    let body = serde_json::to_string(request)?;
    let mut stream = TcpStream::connect((&*endpoint.host, endpoint.port))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let host_header = endpoint.host_header();
    let http_request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        endpoint.path,
        body.len(),
        body
    );
    stream.write_all(http_request.as_bytes())?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (status_line, response_body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line"))?;
    if !(200..300).contains(&status_code) {
        // Surface transport/server errors with their body. A 2xx response may
        // still carry a JSON-RPC level error, which is returned verbatim below.
        return Err(anyhow!("HTTP {status_code}: {}", response_body.trim()));
    }
    let value = serde_json::from_str(response_body.trim())?;
    Ok(value)
}

#[derive(Debug)]
struct HttpEndpoint {
    host: String,
    port: u16,
    path: String,
}

impl HttpEndpoint {
    fn parse(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("only http:// URLs are supported"))?;
        let (host_port, path) = rest
            .split_once('/')
            .map(|(host, path)| (host, format!("/{path}")))
            .unwrap_or((rest, "/rpc".to_string()));
        let (host, port) = parse_host_port(host_port)?;
        Ok(Self { host, port, path })
    }

    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_host_port(host_port: &str) -> Result<(String, u16)> {
    if let Some(bracketed) = host_port.strip_prefix('[') {
        let closing = bracketed
            .find(']')
            .ok_or_else(|| anyhow!("invalid bracketed host {host_port}"))?;
        let host = bracketed[..closing].to_string();
        let suffix = &bracketed[closing + 1..];
        let port = if suffix.is_empty() {
            80
        } else {
            suffix
                .strip_prefix(':')
                .ok_or_else(|| anyhow!("invalid host/port {host_port}"))?
                .parse::<u16>()?
        };
        return Ok((host, port));
    }

    if host_port.matches(':').count() > 1 {
        return Err(anyhow!(
            "IPv6 addresses must be enclosed in brackets: {host_port}"
        ));
    }

    if let Some((host, port)) = host_port.rsplit_once(':') {
        Ok((host.to_string(), port.parse::<u16>()?))
    } else {
        Ok((host_port.to_string(), 80))
    }
}

#[allow(dead_code)]
fn take_option(args: &mut Vec<String>, name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    args.remove(index);
    if index < args.len() {
        Some(args.remove(index))
    } else {
        None
    }
}

/// Like `take_option`, but errors when the option is present without a value
/// (for example `--seed` at the end of the argument list). Returns `Ok(None)`
/// when the option is absent, so callers can still apply their own default.
fn take_value_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    args.remove(index);
    if index < args.len() {
        Ok(Some(args.remove(index)))
    } else {
        Err(anyhow!("{name} requires a value"))
    }
}

fn has_flag(args: &mut Vec<String>, name: &str) -> bool {
    if let Some(index) = args.iter().position(|arg| arg == name) {
        args.remove(index);
        true
    } else {
        false
    }
}

fn print_usage() {
    eprintln!(
        "Usage: agentctl [--url http://127.0.0.1:4000/rpc] [--token TOKEN] <command>\n\
         Commands:\n\
           info | schema | action-space | observation-space\n\
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
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn endpoint_parse_defaults_path_and_port() {
        let endpoint = HttpEndpoint::parse("http://localhost").unwrap();

        assert_eq!(endpoint.host, "localhost");
        assert_eq!(endpoint.port, 80);
        assert_eq!(endpoint.path, "/rpc");
    }

    #[test]
    fn endpoint_parse_reads_host_port_and_path() {
        let endpoint = HttpEndpoint::parse("http://127.0.0.1:4010/rpc").unwrap();

        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 4010);
        assert_eq!(endpoint.path, "/rpc");
    }

    #[test]
    fn endpoint_parse_supports_bracketed_ipv6() {
        let endpoint = HttpEndpoint::parse("http://[::1]:4010/rpc").unwrap();

        assert_eq!(endpoint.host, "::1");
        assert_eq!(endpoint.port, 4010);
        assert_eq!(endpoint.host_header(), "[::1]:4010");
    }

    #[test]
    fn endpoint_parse_rejects_unbracketed_ipv6() {
        let error = HttpEndpoint::parse("http://::1:4010/rpc").unwrap_err();

        assert!(error.to_string().contains("enclosed in brackets"));
    }

    #[test]
    fn take_option_removes_option_and_value() {
        let mut args = vec![
            "reset".to_string(),
            "--seed".to_string(),
            "42".to_string(),
            "--mode".to_string(),
            "Hybrid".to_string(),
        ];

        assert_eq!(take_option(&mut args, "--seed"), Some("42".to_string()));
        assert_eq!(args, vec!["reset", "--mode", "Hybrid"]);
    }

    #[test]
    fn has_flag_removes_present_flag() {
        let mut args = vec!["step-many".to_string(), "--no-stop-on-done".to_string()];

        assert!(has_flag(&mut args, "--no-stop-on-done"));
        assert_eq!(args, vec!["step-many"]);
        assert!(!has_flag(&mut args, "--missing"));
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
    fn post_json_rpc_sends_request_and_parses_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 1024];
            let read = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..read]);
            assert!(request.contains("POST /rpc HTTP/1.1"));
            assert!(request.contains(r#""method":"agent.info""#));
            let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let value = post_json_rpc(
            &format!("http://{addr}/rpc"),
            &json!({"jsonrpc":"2.0","id":1,"method":"agent.info","params":{}}),
        )
        .unwrap();
        handle.join().unwrap();

        assert_eq!(value["result"]["ok"], true);
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
    fn post_json_rpc_errors_on_non_2xx_status() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 1024];
            let _ = stream.read(&mut buf);
            let body = "server exploded";
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let error = post_json_rpc(
            &format!("http://{addr}/rpc"),
            &json!({"jsonrpc":"2.0","id":1,"method":"agent.info","params":{}}),
        )
        .unwrap_err();
        handle.join().unwrap();

        assert!(
            error.to_string().contains("HTTP 500"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn post_json_rpc_preserves_jsonrpc_error_on_http_200() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 1024];
            let _ = stream.read(&mut buf);
            // The bridge returns JSON-RPC errors in an HTTP 200 body.
            let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"invalid or missing session token"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let value = post_json_rpc(
            &format!("http://{addr}/rpc"),
            &json!({"jsonrpc":"2.0","id":1,"method":"agent.info","params":{}}),
        )
        .unwrap();
        handle.join().unwrap();

        assert_eq!(value["error"]["code"], -32603);
        assert!(is_jsonrpc_error(&value));
        assert!(!is_jsonrpc_error(
            &json!({"jsonrpc":"2.0","id":1,"result":{}})
        ));
        assert!(!is_jsonrpc_error(
            &json!({"jsonrpc":"2.0","id":1,"error":null,"result":{}})
        ));
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
}

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let url =
        take_option(&mut args, "--url").unwrap_or_else(|| "http://127.0.0.1:4000/rpc".to_string());
    let token = take_option(&mut args, "--token").or_else(|| std::env::var("AGENT_TOKEN").ok());

    if args.is_empty() {
        print_usage();
        return Ok(());
    }

    let command = args.remove(0);
    let (method, mut params) = build_request(&command, &mut args)?;
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
    Ok(())
}

fn build_request(command: &str, args: &mut Vec<String>) -> Result<(&'static str, Value)> {
    match command {
        "info" => Ok(("agent.info", json!({}))),
        "schema" => Ok(("agent.schema", json!({}))),
        "action-space" => Ok(("agent.action_space", json!({}))),
        "observation-space" => Ok(("agent.observation_space", json!({}))),
        "reset" => {
            let seed = take_option(args, "--seed")
                .map(|value| value.parse::<u64>())
                .transpose()?;
            let observation_mode =
                take_option(args, "--mode").unwrap_or_else(|| "Hybrid".to_string());
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
            let action = args
                .first()
                .ok_or_else(|| anyhow!("step requires an action JSON argument"))?;
            Ok((
                "agent.step",
                json!({
                    "action": serde_json::from_str::<Value>(action)?,
                    "observation_mode": take_option(args, "--mode").unwrap_or_else(|| "Hybrid".to_string()),
                }),
            ))
        }
        "step-many" => {
            let actions = args
                .first()
                .ok_or_else(|| anyhow!("step-many requires an action array JSON argument"))?;
            Ok((
                "agent.step_many",
                json!({
                    "actions": serde_json::from_str::<Value>(actions)?,
                    "stop_on_done": !has_flag(args, "--no-stop-on-done"),
                    "return_observations": take_option(args, "--return").unwrap_or_else(|| "last".to_string()),
                }),
            ))
        }
        "observe" => Ok((
            "agent.observe",
            json!({
                "observation_mode": take_option(args, "--mode").unwrap_or_else(|| "Hybrid".to_string()),
            }),
        )),
        "fast-forward" => {
            let ticks = args
                .first()
                .ok_or_else(|| anyhow!("fast-forward requires a tick count"))?
                .parse::<u64>()?;
            Ok(("agent.fast_forward", json!({ "ticks": ticks })))
        }
        "snapshot" => Ok(("agent.snapshot.create", json!({}))),
        "snapshots" => Ok(("agent.snapshot.list", json!({}))),
        "restore" => {
            let snapshot_id = args
                .first()
                .ok_or_else(|| anyhow!("restore requires a snapshot id"))?;
            Ok((
                "agent.snapshot.restore",
                json!({ "snapshot_id": snapshot_id }),
            ))
        }
        "restore-tick" => {
            let tick = args
                .first()
                .ok_or_else(|| anyhow!("restore-tick requires a tick"))?
                .parse::<u64>()?;
            Ok(("agent.timeline.restore_tick", json!({ "tick": tick })))
        }
        "branch" => {
            let from_tick = take_option(args, "--from-tick")
                .ok_or_else(|| anyhow!("branch requires --from-tick <tick>"))?
                .parse::<u64>()?;
            let label = take_option(args, "--label");
            Ok((
                "agent.timeline.branch",
                json!({ "from_tick": from_tick, "label": label }),
            ))
        }
        "replay-start" => Ok(("agent.replay.start", json!({}))),
        "replay-stop" => Ok(("agent.replay.stop", json!({}))),
        "replay-export" => Ok((
            "agent.replay.export",
            json!({ "path": args.first().cloned() }),
        )),
        "replay-load" => {
            let path = args
                .first()
                .ok_or_else(|| anyhow!("replay-load requires a path"))?;
            Ok(("agent.replay.load", json!({ "path": path })))
        }
        _ => Err(anyhow!("unknown command {command}")),
    }
}

fn post_json_rpc(url: &str, request: &Value) -> Result<Value> {
    let endpoint = HttpEndpoint::parse(url)?;
    let body = serde_json::to_string(request)?;
    let mut stream = TcpStream::connect((&*endpoint.host, endpoint.port))?;
    let http_request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}:{}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        endpoint.path,
        endpoint.host,
        endpoint.port,
        body.len(),
        body
    );
    stream.write_all(http_request.as_bytes())?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (_, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let value = serde_json::from_str(body.trim())?;
    Ok(value)
}

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
        let (host, port) = if let Some((host, port)) = host_port.rsplit_once(':') {
            (host.to_string(), port.parse::<u16>()?)
        } else {
            (host_port.to_string(), 80)
        };
        Ok(Self { host, port, path })
    }
}

fn take_option(args: &mut Vec<String>, name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    args.remove(index);
    if index < args.len() {
        Some(args.remove(index))
    } else {
        None
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
}

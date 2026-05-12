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

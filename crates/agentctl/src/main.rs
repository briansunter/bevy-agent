mod commands;
mod protocol;
mod transport;

use anyhow::Result;
use serde_json::Value;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 1 && matches!(args[0].as_str(), "help" | "--help" | "-h") {
        commands::print_usage();
        return Ok(());
    }
    if args.len() == 1 && matches!(args[0].as_str(), "--version" | "-V") {
        println!("agentctl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let options = commands::GlobalOptions::parse(&mut args)?;
    let token = options.token.or_else(|| std::env::var("AGENT_TOKEN").ok());
    if args.is_empty() {
        commands::print_usage();
        std::process::exit(2);
    }

    let command = args.remove(0);
    let (method, mut params) = commands::build_request(&command, &mut args)?;
    commands::ensure_no_leftover_args(&args)?;
    if let Some(token) = token {
        params["session_token"] = Value::String(token);
    }

    let mut request = protocol::request(method, params);
    if let Some(key) = options.retry_key {
        request["retry_key"] = Value::String(key);
    }
    let response = transport::post_json_rpc(&options.url, &request)?;
    println!("{}", serde_json::to_string_pretty(response.value())?);
    if let Some(error) = response.error() {
        eprintln!("json-rpc error: {error}");
        std::process::exit(2);
    }
    Ok(())
}

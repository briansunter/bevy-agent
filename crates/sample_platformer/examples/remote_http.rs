use std::path::PathBuf;

use bevy_agent_remote::{AgentCapability, HttpRemoteServer, JsonRpcBridge, RemoteSecurity};
use bevy_agent_runner::AgentApp;

fn main() -> anyhow::Result<()> {
    // Usage: remote_http [bind_addr] [--artifact-dir <dir> | --artifact-dir=<dir>]
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut bind_addr = "127.0.0.1:4000".to_string();
    let mut artifact_dir = PathBuf::from("./artifacts");
    let mut positional = 0;
    let mut index = 0;
    while index < raw.len() {
        let arg = raw[index].as_str();
        if let Some(value) = arg.strip_prefix("--artifact-dir=") {
            artifact_dir = PathBuf::from(value);
        } else if arg == "--artifact-dir" {
            index += 1;
            if let Some(value) = raw.get(index) {
                artifact_dir = PathBuf::from(value);
            }
        } else if !arg.starts_with("--") && positional == 0 {
            bind_addr = arg.to_string();
            positional += 1;
        }
        index += 1;
    }
    let token = std::env::var("AGENT_TOKEN").ok();

    // Filesystem-touching RPCs (replay export/load `path`, visual capture
    // `output_dir`) are confined under this directory. The library default
    // denies FILESYSTEM; this example opts in explicitly so screenshot and
    // file-export workflows work out of the box.
    std::fs::create_dir_all(&artifact_dir)?;
    let mut capabilities = AgentCapability::default();
    capabilities.insert(AgentCapability::FILESYSTEM);
    let security = RemoteSecurity {
        session_token: token,
        capabilities,
        artifact_root: Some(artifact_dir),
        ..Default::default()
    };

    let mut env = AgentApp::new(sample_platformer::build_headless_app);
    let bridge = JsonRpcBridge::new(security);
    HttpRemoteServer::new(bind_addr, bridge).serve(&mut env)
}

#[path = "support/http_config.rs"]
mod http_config;

use bevy_agent_remote::{HttpRemoteServer, JsonRpcBridge};
use bevy_agent_runner::AgentApp;

fn main() -> anyhow::Result<()> {
    // Usage: remote_http [bind_addr] [--artifact-dir <dir> | --artifact-dir=<dir>]
    // Filesystem-touching RPCs (replay export/load `path`, visual capture
    // `output_dir`) are confined under this directory. The library default
    // denies FILESYSTEM; this example opts in explicitly so screenshot and
    // file-export workflows work out of the box.
    let (bind_addr, security) = http_config::configuration()?;

    let mut env = AgentApp::new(sample_platformer::build_headless_app)?;
    let bridge = JsonRpcBridge::new(security)?;
    HttpRemoteServer::new(bind_addr, bridge).serve(&mut env)
}

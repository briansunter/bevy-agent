use bevy_agent_remote::{JsonRpcBridge, RemoteSecurity};
use bevy_agent_runner::AgentApp;

fn main() -> anyhow::Result<()> {
    let mut env = AgentApp::new(sample_platformer::build_headless_app);
    let bridge = JsonRpcBridge::new(RemoteSecurity::default());
    bridge.serve_stdio(&mut env)
}

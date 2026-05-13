use bevy_agent_remote::{HttpRemoteServer, JsonRpcBridge, RemoteSecurity};
use bevy_agent_runner::AgentApp;

fn main() -> anyhow::Result<()> {
    let bind_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:4000".to_string());
    let token = std::env::var("AGENT_TOKEN").ok();

    let security = RemoteSecurity {
        session_token: token,
        ..Default::default()
    };

    let mut env = AgentApp::new(sample_platformer::build_visual_app);
    let bridge = JsonRpcBridge::new(security);
    HttpRemoteServer::new(bind_addr, bridge).serve(&mut env)
}

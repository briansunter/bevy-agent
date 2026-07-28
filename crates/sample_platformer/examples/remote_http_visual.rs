use bevy_agent_remote::{BevyRemoteControlPlugin, JsonRpcBridge, RemoteSecurity};

fn main() -> anyhow::Result<()> {
    let bind_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:4000".to_string());
    let token = std::env::var("AGENT_TOKEN").ok();

    let security = RemoteSecurity {
        session_token: token,
        ..Default::default()
    };

    let bridge = JsonRpcBridge::new(security);
    let remote = BevyRemoteControlPlugin::bind(bind_addr, bridge)?;
    let mut app = sample_platformer::build_visual_app();
    app.add_plugins(remote);
    app.run();
    Ok(())
}

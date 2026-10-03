#[path = "support/http_config.rs"]
mod http_config;

use bevy_agent_remote::{BevyRemoteControlPlugin, JsonRpcBridge};

fn main() -> anyhow::Result<()> {
    let (bind_addr, security) = http_config::configuration()?;

    let bridge = JsonRpcBridge::new(security)?;
    let remote = BevyRemoteControlPlugin::bind(bind_addr, bridge)?;
    let mut app = sample_platformer::build_visual_app();
    app.add_plugins(remote);
    app.run();
    Ok(())
}

# bevy_agent_remote

JSON-RPC control for Bevy agent environments over HTTP, WebSocket, and stdio.

Part of [bevy-agent](https://github.com/briansunter/bevy-agent). Requires **Bevy 0.18.1** and **Rust 1.91+**. Version **0.0.2** is experimental; pin all companion crates to the same exact version.

```toml
[dependencies]
bevy_agent_remote = "=0.0.2"
bevy_agent_runner = "=0.0.2"
```

## Serve a headless environment

```rust,no_run
use bevy_agent_remote::{HttpRemoteServer, JsonRpcBridge, RemoteSecurity};
use bevy_agent_runner::AgentApp;

fn serve(env: &mut AgentApp) -> anyhow::Result<()> {
    let bridge = JsonRpcBridge::new(RemoteSecurity::default())?;
    HttpRemoteServer::new("127.0.0.2:4000", bridge).serve(env)
}
```

The server exposes `POST /rpc`, `GET /health`, and WebSocket JSON-RPC at `GET /ws`. Discovery methods expose the environment's actions, observations, schemas, and capabilities.

For a rendered Bevy app, install `BevyRemoteControlPlugin` before `app.run()`. Network I/O runs separately while world mutations and primary-window captures remain on the Bevy main thread. Enable `visual` for primary-window screenshot support; software capture works headlessly when the game supplies a renderer.

## Remote access

Bind to loopback for local use. Configure a nonempty session token before exposing the listener beyond loopback. `RemoteSecurity` controls capabilities; filesystem access is disabled by default. Enable `FILESYSTEM` explicitly and set an artifact root when you need server-side captures or replay files.

Timed-out mutations return an operation ID. Retrieve the retained result with `agent.operations.status`; use a request `retry_key` to deduplicate retries after a lost response. The retry ledger belongs to one running HTTP/WebSocket server and expires with retained outcomes; stdio does not provide it.

## Next steps

[Read the guide](https://briansunter.github.io/bevy-agent/guides/remote-control.html) for an organized walkthrough, examples, and troubleshooting.

- [Protocol and client guide](https://briansunter.github.io/bevy-agent/guides/remote-control.html)
- [JSON-RPC operation reference](https://briansunter.github.io/bevy-agent/reference/protocol.html)
- [API reference](https://docs.rs/bevy_agent_remote)

Licensed under **MIT OR Apache-2.0**, at your option.

# HTTP and the CLI

Use a remote server when your agent or test harness runs in another process. The same environment can be controlled through Rust, HTTP, WebSocket, stdio, the CLI, or Python.

## 1. Install the client

```sh
cargo install bevy_agent_cli --version 0.0.2 --locked
agentctl --version
```

Expected: `agentctl 0.0.2`. The package is `bevy_agent_cli`; the executable is `agentctl`. The crates.io package named `agentctl` belongs to another project.

## 2. Start a complete game server

Clone the repository, then leave this command running:

```sh
git clone https://github.com/briansunter/bevy-agent.git
cd bevy-agent
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

The example includes the game. Installing the CLI alone does not start a server. `sample_platformer` is distributed through Git, not crates.io.

## 3. Discover, reset, and step

In a second terminal:

```sh
agentctl info
agentctl action-space
agentctl observation-space
agentctl reset --seed 42
agentctl step '{"type":"Move","x":1.0,"y":0.0}'
agentctl observe
```

The default endpoint is `http://127.0.0.1:4000/rpc`. Use `--url` for another server:

```sh
agentctl --url http://127.0.0.1:4001/rpc info
```

Read the returned observation and check terminal state before choosing the next action. Discover supported actions; movement is part of the platformer's contract.

## Add a server to your own environment

Add `bevy_agent_remote = "=0.0.2"` to the [getting-started manifest](../getting-started.md#_1-create-a-small-rust-project). After building your `AgentApp`:

```rust
use bevy_agent_remote::{HttpRemoteServer, JsonRpcBridge, RemoteSecurity};
use bevy_agent_runner::AgentApp;

fn serve(env: &mut AgentApp) -> anyhow::Result<()> {
    let bridge = JsonRpcBridge::new(RemoteSecurity::default())?;
    HttpRemoteServer::new("127.0.0.1:4000", bridge).serve(env)
}
```

This library configuration disables filesystem operations. The sample explicitly enables them and confines files under its artifact root.

## Call JSON-RPC directly

```sh
curl -s http://127.0.0.1:4000/rpc   -H 'content-type: application/json'   -d '{"jsonrpc":"2.0","id":1,"method":"agent.info","params":{}}'
```

The server also exposes `GET /health` and the WebSocket endpoint `GET /ws`. Always inspect the JSON-RPC body: authentication and capability failures can arrive in an HTTP 200 response.

## Authentication and files

Bind to loopback for local use. Configure a nonempty session token before binding beyond loopback. The sample reads `AGENT_TOKEN`; the CLI reads the same variable or `--token`. Do not commit tokens or replay private game data without reviewing its contents.

Server-side paths resolve under the configured artifact root. A CLI `--out-dir screenshots` does not place screenshots next to the client process.

Continue with [Python](./python.md), [screenshots](./capture.md), or [retries and recovery](./recovery.md).

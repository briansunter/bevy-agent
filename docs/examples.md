# Runnable examples

Choose the smallest example that answers your question. Commands below run from a repository checkout and use the committed lockfile.

```sh
git clone https://github.com/briansunter/bevy-agent.git
cd bevy-agent
```

## A complete environment without a window

```sh
cargo run -p bevy_agent_runner --example counter --locked
cargo test -p bevy_agent_runner --example counter --locked
```

**You should see:** the snapshot/replay success message, then two passing tests. Read [Getting started](./getting-started.md) for the complete app and [Testing](./guides/testing.md) for what its assertions prove.

[Counter source](https://github.com/briansunter/bevy-agent/blob/master/crates/bevy_agent_runner/examples/counter.rs) · Distributed in the runner crate.

## Scripted gameplay

```sh
cargo run -p sample_platformer --example agent_play --locked
```

**You should see:** JSON describing the initial state, and a terminal response if the scripted sequence ends the episode. This example runs movement and jump actions through the Rust API without a window.

[Platformer source](https://github.com/briansunter/bevy-agent/tree/master/crates/sample_platformer) · Repository only.

## A game controlled by another process

```sh
cargo run -p sample_platformer --example remote_http --locked -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

**You should see:** a server that stays running. In another terminal, follow [HTTP and the CLI](./guides/remote-control.md) or [Python](./guides/python.md). HTTP uses `/rpc`, health checks use `/health`, and WebSocket clients use `/ws`.

## A visible game and screenshots

```sh
cargo run -p sample_platformer --features visual --example remote_http_visual --locked -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

**You should see:** a Bevy window and a running RPC server. This needs a working graphics environment and the platform libraries used by Bevy. It uses the same port as the headless server: stop that process first or choose a different port.

Follow [Screenshots and rendering](./guides/capture.md) to choose software versus primary-window capture. A displayed frame and a controlled simulation tick are separate events.

## A subprocess transport

```sh
cargo run -p sample_platformer --example remote_stdio --locked
```

This process waits for newline-delimited JSON-RPC on stdin; it is not an interactive game prompt. Follow [WebSocket and stdio](./guides/transports.md) for requests and Python subprocess control. Use HTTP/WebSocket when you need retained operation status and retry-key deduplication.

## Which files are published?

The counter is packaged with `bevy_agent_runner`. The platformer, Python client, and agent skills are distributed through Git. Installing `bevy_agent_cli` gives you the `agentctl` client; start a compatible environment separately.

On the pinned personal Mac mini, use the guarded launchers described in [Contributing](./reference/contributing.md#personal-mac-mini).

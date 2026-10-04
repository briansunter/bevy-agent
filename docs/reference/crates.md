# Choose your crates

There are five runtime crates and one independent command-line client. Start with core, snapshot, and runner for a headless environment. Add remote when another process needs to control it.

## Runtime packages

| Package | Use it for | Rust API |
| --- | --- | --- |
| [bevy_agent_core](https://crates.io/crates/bevy_agent_core/0.0.3) | Schedules, actions, clock, RNG, observations, identities, checksums | [docs.rs](https://docs.rs/bevy_agent_core) |
| [bevy_agent_snapshot](https://crates.io/crates/bevy_agent_snapshot/0.0.3) | Registered gameplay state, snapshots, validation, restore, retention | [docs.rs](https://docs.rs/bevy_agent_snapshot) |
| [bevy_agent_replay](https://crates.io/crates/bevy_agent_replay/0.0.3) | Input logs, checkpoint indexes, recording, branch topology | [docs.rs](https://docs.rs/bevy_agent_replay) |
| [bevy_agent_runner](https://crates.io/crates/bevy_agent_runner/0.0.3) | AgentApp, plugin composition, stepping, navigation, capture | [docs.rs](https://docs.rs/bevy_agent_runner) |
| [bevy_agent_remote](https://crates.io/crates/bevy_agent_remote/0.0.3) | JSON-RPC, HTTP, WebSocket, stdio, capabilities, operation outcomes | [docs.rs](https://docs.rs/bevy_agent_remote) |

All runtime packages target Bevy 0.18.1 and Rust 1.91+. They are experimental; pin matching exact versions. docs.rs API availability depends on its build processing; these guides are published independently.

## The CLI

[bevy_agent_cli on crates.io](https://crates.io/crates/bevy_agent_cli/0.0.3) installs the `agentctl` executable:

```sh
cargo install bevy_agent_cli --version 0.0.3 --locked
```

It does not link the simulation runtime or include a game. It sends JSON-RPC requests to a running environment. [HTTP and the CLI](../guides/remote-control.md) shows a complete server/client workflow.

## Feature selection

Runtime defaults are headless. `AgentControlPlugins::default()` composes core, snapshots, and replay. The runner and remote `visual` features add render/window capture support; your game chooses its rendering plugins.

| Goal | Direct dependencies |
| --- | --- |
| First counter | core, snapshot, runner |
| Use replay/timeline types directly | Add replay |
| Serve a separate client | Add remote |
| Capture the primary window | Enable visual on runner/remote and install presentation plugins |
| Use the command-line client only | Install bevy_agent_cli |

The platformer, Python client, agent skills, and repository documents are Git-distributed. They are not additional published Cargo packages.

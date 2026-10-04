# bevy_agent_cli

The `agentctl` command-line client for [bevy-agent](https://github.com/briansunter/bevy-agent) JSON-RPC environments.

The Cargo package is named **`bevy_agent_cli`**; the installed executable is **`agentctl`**. Requires **Rust 1.91+**. Version **0.0.1** is experimental.

## Install

After the initial release is published:

```sh
cargo install bevy_agent_cli --version 0.0.1 --locked
```

Before publication, install from a repository checkout:

```sh
cargo install --path crates/agentctl --locked
```

## Control a running game

Start a compatible server first. By default the client connects to `http://127.0.0.1:4000/rpc`.

```sh
agentctl info
agentctl schema
agentctl reset --seed 42
agentctl step '{"type":"Move","x":1.0,"y":0.0}'
agentctl snapshot
agentctl capture --out-dir screenshots --label after_step
agentctl replay-export replay.json
```

Actions depend on the game's declared catalog. Discover it with `agentctl action-space`. Captures and file exports require server filesystem permission; paths resolve under the server's artifact root.

```sh
agentctl --url http://127.0.0.1:4000/rpc --token secret info
agentctl --retry-key episode-1.tick-1 step '{"type":"Noop"}'
agentctl operation-status --key episode-1.tick-1
```

You can also set `AGENT_TOKEN`; use `--url` to select the server. Run `agentctl help` for all commands. The CLI does not start a server or include a game.

[Full interaction guide](https://github.com/briansunter/bevy-agent/blob/master/docs/codex-interaction.md)

Licensed under **MIT OR Apache-2.0**, at your option.

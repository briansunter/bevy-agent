# Sample platformer

A repository-only reference game for [bevy-agent](https://github.com/briansunter/bevy-agent). This package is not published to crates.io.

The sample demonstrates deterministic movement, gravity, collision, coin pickup, rewards, terminal checks, observations, software screenshots, snapshots, replays, and timeline branches.

From the repository root:

```sh
cargo run -p sample_platformer --example agent_play
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

The HTTP example keeps running. Use the `agentctl` CLI or the Python client from another terminal.

Enable Bevy rendering and primary-window capture:

```sh
cargo run -p sample_platformer --features visual --example remote_http_visual -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

- [Integration guide](https://github.com/briansunter/bevy-agent/blob/master/docs/controllable-game.md)
- [Interaction guide](https://github.com/briansunter/bevy-agent/blob/master/docs/codex-interaction.md)

Licensed under **MIT OR Apache-2.0**, at your option.

# Documentation

Read the [Bevy Agent guide](https://briansunter.github.io/bevy-agent/) for searchable, organized documentation.

- [Getting started](./getting-started.md): a complete standalone counter with expected output.
- [Control loop](./concepts.md): actions, ticks, observations, and authoritative state.
- [Game integration](./controllable-game.md): registration and deterministic gameplay.
- [Snapshots and replay](./guides/snapshots-replay.md): save, restore, reconstruct, and branch.
- [HTTP and CLI](./guides/remote-control.md), [Python](./guides/python.md), and [other transports](./guides/transports.md).
- [Crates](./reference/crates.md), [protocol](./reference/protocol.md), and [troubleshooting](./reference/troubleshooting.md).
- [Architecture](./architecture.md), [contributing](./reference/contributing.md), and [publishing](./publishing.md).

The source remains readable as Markdown in the repository. `npm ci` and `npm run docs:dev` start VitePress locally.

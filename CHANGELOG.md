# Changelog

## 0.0.1 — initial release (unreleased)

The first experimental release targets Bevy 0.18.1 and Rust 1.91+. APIs may change before 1.0; use matching exact versions of all companion crates.

### Runtime

- Explicit simulation ticks, seeded randomness, domain action catalogs, and validated observation contracts.
- Gameplay snapshots with stable entity/type identities, required resource registration, checked restore, and bounded retention.
- Action recording, checkpoints, timeline branches, and portable replay bundles.
- `AgentApp` for reset, stepping, fast-forward, observation, snapshots, restore, branching, and capture.
- Headless software capture hooks and optional primary-window capture through the `visual` feature.

### Clients

- JSON-RPC over HTTP, WebSocket, and stdio, with capability checks and optional session tokens.
- HTTP/WebSocket operation status and retry-key deduplication for uncertain mutation outcomes.
- `bevy_agent_cli`, installing the `agentctl` executable with help and version commands.
- Repository Python client, platformer reference game, and agent integration/control skills.

### Packaging and compatibility

- Package-specific READMEs used as library API documentation, a standalone counter example, and a release checklist.
- Explicit package contents, bundled MIT/Apache-2.0 licenses, and docs.rs all-feature builds.
- Locked the compatible `spin` 0.10.1 patch release to avoid the yanked 0.10.0 dependency.
- Snapshot/replay format version 3; older artifacts are rejected without migration. Wire formats and per-game schema versions are independent of Cargo package versions.
- The platformer is a repository example with publication disabled.

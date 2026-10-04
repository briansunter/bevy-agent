# Changelog

## 0.0.4 — 2026-10-03

- Reorganize the README around runnable examples, integration choices, and the client/server workflow.
- Add an example directory and a testing guide with two runnable counter regression tests.
- Explain the getting-started state transitions, integration milestones, and CLI response fields.
- Add a custom simulation illustration and editable control-loop and snapshot/replay diagrams.
- Improve all six package READMEs and guide navigation. Runtime APIs and snapshot/replay format 3 are unchanged.

## 0.0.3 — 2026-10-03

- Correct loopback addresses in CLI and server documentation to `127.0.0.1`.
- Keep companion crates synchronized for this documentation patch; runtime behavior and wire formats are unchanged.

## 0.0.2 — 2026-10-03

- Searchable VitePress documentation with a complete getting-started app, interactive control-loop walkthrough, focused guides, and crate/API reference.
- Self-hosted typography, responsive navigation, dark mode, and manual GitHub Pages deployment.
- Cargo homepage/documentation metadata and package READMEs link to the guide; this documentation release updates the registry metadata without changing runtime behavior.

## 0.0.1 — 2026-10-03

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

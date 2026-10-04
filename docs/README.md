# Documentation

Start with [`getting-started.md`](getting-started.md) for installation and a complete headless example. The initial release is `0.0.1` (experimental, pre-1.0).

Use the guide that matches the job:

## Run and control

- [`codex-interaction.md`](codex-interaction.md) covers the HTTP, WebSocket, stdio, CLI, Python, capture, snapshot, replay, and security workflows.
- [`../skills/control-bevy-agent-game/SKILL.md`](../skills/control-bevy-agent-game/SKILL.md) is the agent-facing control procedure, including observation and determinism checks.

## Integrate a game

- [`controllable-game.md`](controllable-game.md) is the short integration checklist and minimal Bevy shape.
- [`../skills/integrate-bevy-agent-control/SKILL.md`](../skills/integrate-bevy-agent-control/SKILL.md) contains the full agent-facing integration workflow and review checklist.
- [`../skills/integrate-bevy-agent-control/references/integration-patterns.md`](../skills/integrate-bevy-agent-control/references/integration-patterns.md) contains concrete Rust patterns.
- [`../skills/control-bevy-agent-game/references/protocol.md`](../skills/control-bevy-agent-game/references/protocol.md) is the JSON-RPC operation reference.

## Maintain and publish

- [`architecture.md`](architecture.md) explains crate/module ownership, deterministic execution, snapshot/replay transactions, transport contracts, the architecture review, and its implementation plan.
- [`publishing.md`](publishing.md) lists formatting, test, lint, coverage, packaging, and publish checks.

For a high-level overview and the quickest sample run, start with the [root README](../README.md).

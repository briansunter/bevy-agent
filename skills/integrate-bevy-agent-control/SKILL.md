---
name: integrate-bevy-agent-control
description: "Integrate bevy_agent_control into a Bevy game so AI agents can control deterministic simulation ticks. Use when Codex is modifying or scaffolding a Bevy game, separating authoritative gameplay from rendering, defining serializable domain actions, converting keyboard/gamepad/network/test input into action frames, moving gameplay systems into AgentTick, using CurrentInputFrame and SimClock, registering snapshot state, writing symbolic observations and checksums, exposing visual screenshot capture, exposing localhost JSON-RPC or stdio control, or testing replay/snapshot determinism."
metadata:
  short-description: Make Bevy games agent-controllable
---

# Integrate Bevy Agent Control

## Integration Rule

Make the authoritative simulation controllable by domain actions and deterministic ticks. Rendering, UI, audio, asset loading, debug overlays, and wall-clock effects must stay outside the authoritative gameplay state.

The result should be playable by humans and agents through the same action path. Human input becomes domain actions; it does not bypass the simulation interface.

## Target Shape

A good integration has:

- a headless app builder using `MinimalPlugins` for tests and agent runs;
- an optional visual app builder for watching/debugging;
- a serializable domain action type;
- one action queue/frame consumed by deterministic gameplay systems;
- gameplay systems scheduled in `AgentTick` and `AgentSet::Simulation`;
- deterministic time from `SimClock`, not Bevy `Time`;
- snapshot registration for every gameplay component/resource needed to resume;
- stable IDs for entities visible in observations, actions, replays, and snapshots;
- compact symbolic observations plus optional privileged debug fields;
- optional on-demand PNG visual capture for agent inspection;
- checksums over all state that can affect future simulation;
- remote control bound to localhost by default with token/capability checks.

## Workflow

1. Map the current gameplay loop: input collection, movement/physics, spawning, scoring, terminal conditions, RNG, and reset.
2. Define the domain action type. Keep actions semantic, serializable, and stable across replays.
3. Convert every input source, including keyboard/gamepad/network/tests/agents/replay, into the same action queue.
4. Add `AgentControlPlugin`, `AgentSnapshotPlugin`, and `AgentReplayPlugin`.
5. Build a deterministic headless app with `MinimalPlugins`.
6. Move authoritative gameplay systems into `AgentTick` and `AgentSet::Simulation`.
7. Read current tick input only from `CurrentInputFrame`.
8. Read deterministic time only from `SimClock`.
9. Add `SnapshotEntity` and `StableEntityId` to gameplay entities.
10. Register gameplay components and resources for snapshot/restore.
11. Add a reset system that recreates a clean playable state from seed/options.
12. Add an observation extractor and a checksum extractor.
13. Add a visual capture renderer if agents need screenshots in headless runs.
14. Expose HTTP or stdio control only after local stepping works.
15. Add tests for step, batch step, action scheduling, snapshot/restore, replay, branch, remote schema, visual capture, and determinism.

## Minimal Shape

```rust
use bevy::prelude::*;
use bevy_agent_core::{AgentControlPlugin, AgentSet, AgentTick};
use bevy_agent_replay::AgentReplayPlugin;
use bevy_agent_snapshot::AgentSnapshotPlugin;

pub fn build_headless_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugin::deterministic())
        .add_plugins(AgentSnapshotPlugin)
        .add_plugins(AgentReplayPlugin)
        .add_plugins(GamePlugin);
    app
}

impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            AgentTick,
            (apply_actions, physics_step, scoring, terminal_check)
                .chain()
                .in_set(AgentSet::Simulation),
        );
    }
}
```

## Migration Notes

When converting an existing Bevy game:

- keep rendering systems in `Update`, but make them read simulation state rather than mutate rules;
- use `VisualCaptureAppExt::insert_visual_capture_renderer` for headless screenshots, or enable the runner `visual` feature for Bevy primary-window screenshots;
- move random decisions behind a seeded gameplay RNG resource and register it for snapshots;
- move timers that affect gameplay to tick counters or `SimClock`;
- replace raw `Entity` IDs in observations/actions with stable IDs;
- make reset idempotent by clearing snapshot entities and respawning from seed/options;
- test the headless app before adjusting visual mode.

## Code Review Checklist

Reject or revise integrations that:

- read keyboard, mouse, gamepad, wall-clock time, OS randomness, or network state directly in authoritative gameplay systems;
- run gameplay rules in ordinary frame `Update` when they should be deterministic;
- snapshot render, window, audio, asset-server, socket, or UI internals;
- use raw Bevy `Entity` IDs as semantic identity in observations, actions, snapshots, or replay logs;
- omit gameplay resources/components from snapshot registration;
- compute observations by dumping excessive ECS state when compact symbolic state would work;
- implement screenshot capture by mutating gameplay state or adding visual state to checksums;
- expose mutation or restore over remote control without a session token and capabilities;
- validate only through visuals instead of `tick`, `reward`, terminal flags, observation fields, and checksum.

## Validation

Run focused tests after integration:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Add or update tests for:

- `step` increments one tick;
- queued and future actions apply once on the intended tick;
- `step_many` matches repeated `step`;
- snapshot, restore, and subsequent replay produce matching checksums;
- branch creation does not mutate the parent timeline;
- reset with the same seed produces the same initial observation/checksum;
- observation policy hides state that should not be visible to the player;
- `agent.visual.capture` writes a non-empty PNG and enforces token/capability checks;
- remote methods return valid JSON-RPC schemas and enforce token/capabilities.

## References

Read `references/integration-patterns.md` for concrete imports, component/resource registration, action, reset, observation, checksum, remote, and testing patterns.

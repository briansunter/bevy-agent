---
name: integrate-bevy-agent-control
description: "Integrate bevy_agent_control into a Bevy game so AI agents can control deterministic simulation ticks. Use when Codex is modifying or scaffolding a Bevy game, separating authoritative gameplay from rendering, defining stable agent actions or custom action schemas, converting keyboard/gamepad/network/test input into action frames, moving gameplay systems into AgentTick, using CurrentInputFrame and SimClock, registering snapshot state with macros, writing symbolic observations and stable checksums, exposing visual screenshot capture, exposing localhost JSON-RPC or stdio control, or testing replay/snapshot determinism."
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
- `AgentControlPlugins::default()` for the standard control/snapshot/replay stack;
- an optional visual app builder using `DefaultPlugins` plus `AgentControlPlugins::default()`;
- stable built-in `AgentAction` variants or registered custom action schemas for `AgentAction::Custom`;
- environment metadata plus explicit supported-action and supported-observation-mode catalogs;
- required observation and checksum extractors, with no fallback observation mode;
- one action queue/frame consumed by deterministic gameplay systems;
- autonomous policies registered in `AgentDecision`, once per simulation tick;
- gameplay systems scheduled in `AgentTick` and `AgentSet::Simulation`;
- deterministic time from `SimClock`, not Bevy `Time`;
- checked snapshot registration for every gameplay component/resource needed to resume, using stable `SnapshotType` IDs and per-type versions;
- stable IDs for entities visible in observations, actions, replays, and snapshots;
- compact symbolic observations plus optional privileged debug fields;
- optional on-demand PNG visual capture for agent inspection;
- stable checksums over all state that can affect future simulation;
- remote control bound to loopback by default; non-loopback binds require a session token.

## Workflow

1. Map the current gameplay loop: input collection, movement/physics, spawning, scoring, terminal conditions, RNG, and reset.
2. Define the action and observation surface. Declare metadata, supported actions, supported observation modes, and a supported default mode. Use `AgentAction::Custom` plus a registered JSON schema for game-specific actions.
3. Convert every input source into the checked action queue. Handle scheduling results and ensure the selected control mode accepts the source; use `Hybrid` for shared human/agent control. Leave privileged history reconstruction to the runner.
4. Add `AgentControlPlugins::default()` in headless, visual, and remote builds. Choose rendering and remote plugins independently.
5. Build a deterministic headless app with `MinimalPlugins`.
6. Move authoritative gameplay systems into `AgentTick` and `AgentSet::Simulation`.
7. Read current tick input only from `CurrentInputFrame`.
8. Read deterministic time only from `SimClock`.
9. Add `SnapshotEntity` and `StableEntityId` to gameplay entities.
10. Implement `SnapshotType` with stable wire IDs and positive per-type versions. Register components with `register_snapshot_components!` and required resources with `register_required_snapshot_resource`, handling each result.
11. Add a reset system that recreates a clean playable state from seed/options.
12. Install both required extractors. Register the full serialized `Observation` schema when applicable and schemas for custom actions; handle schema compilation results.
13. Add a visual capture renderer if agents need screenshots in headless runs. For a real window, install `BevyRemoteControlPlugin` in the visual app and let the normal Bevy runner own the main thread.
14. Expose HTTP or stdio control only after local stepping works; keep tokenless HTTP on loopback only.
15. Add tests for step, batch step, action scheduling, custom action schemas, snapshot/restore, replay, branch, remote schema, visual capture, and determinism.

## Minimal Shape

```rust
use bevy::prelude::*;
use bevy_agent_core::{
    AgentActionKind, AgentControlAppExt, AgentDecision, AgentReset, AgentResetSet, AgentSet,
    AgentTick, ObservationMode, StableEntityId,
};
use bevy_agent_runner::AgentControlPlugins;
use bevy_agent_snapshot::{SnapshotAppExt, register_snapshot_components};

pub fn build_headless_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugins::default())
        .add_plugins(GamePlugin);
    app
}

impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        register_snapshot_components!(app, StableEntityId, Transform, Velocity, Player)
            .expect("game snapshot component IDs and versions are valid");
        app.init_resource::<GameScore>()
            .init_resource::<GameplayRng>()
            .register_required_snapshot_resource::<GameScore>()
            .expect("score snapshot identity is valid")
            .register_required_snapshot_resource::<GameplayRng>()
            .expect("RNG snapshot identity is valid")
            .set_snapshot_metadata("my_game", env!("CARGO_PKG_VERSION"));

        app.set_environment_metadata("my_game", env!("CARGO_PKG_VERSION"), None)
            .set_supported_actions([
                AgentActionKind::Noop,
                AgentActionKind::Move,
                AgentActionKind::Jump,
                AgentActionKind::Custom,
            ])
            .set_supported_observation_modes([
                ObservationMode::PlayerKnowledge,
                ObservationMode::Hybrid,
            ])
            .set_observation_schema(game_observation_schema())
            .expect("game observation schema is valid")
            .register_custom_action_schema(
                "game_action",
                serde_json::json!({
                    "oneOf": [
                        {
                            "type": "object",
                            "required": ["type"],
                            "properties": { "type": { "const": "dash" } }
                        },
                        {
                            "type": "object",
                            "required": ["type", "x", "y"],
                            "properties": {
                                "type": { "const": "interact_at" },
                                "x": { "type": "number", "minimum": -100000, "maximum": 100000 },
                                "y": { "type": "number", "minimum": -100000, "maximum": 100000 }
                            }
                        }
                    ]
                }),
            )
            .expect("game custom action schema is valid");

        app.insert_observation_extractor(game_observation)
            .insert_checksum_extractor(game_checksum)
            .add_systems(AgentDecision, agent_policy)
            .add_systems(AgentReset, reset_level.in_set(AgentResetSet::Game))
            .add_systems(
                AgentTick,
                (apply_actions, physics_step, scoring)
                    .chain()
                    .in_set(AgentSet::Simulation),
            )
            .add_systems(AgentTick, terminal_check.in_set(AgentSet::TerminalCheck));
    }
}
```

Game-owned `Player`, `Velocity`, `GameScore`, and `GameplayRng` must implement
`SnapshotType`, `Clone`, serialization, and deserialization, in addition to
their Bevy component/resource traits. The reference patterns show stable IDs,
the observation schema, and checked scheduling.

Schemas are enforced contracts. Core compiles and caches draft 2020-12 JSON
Schema validators when registration succeeds. A custom payload must match a
registered schema before scheduling or controller execution; a collected
observation must match the declared **full `Observation` envelope**, including
its `kind` tag. Invalid schema registration leaves the prior contract intact.
Local schema references are supported; remote references are rejected.

`AgentApp::new`, `from_app`, and `from_running_app` return `Result` and reject an
incomplete integration. `enqueue_action_at`, `schedule_action`, queue
`schedule`, `run_agent_tick`, and observation collection also return results.
Propagate dynamic failures with `?`; use descriptive `expect` calls for static
plugin configuration, as above. Unsupported observation modes fail before
extraction and must not silently fall back to a different view.

After `AgentPostTick`, `AgentFinalize` extracts observations, records history,
and captures periodic snapshots. Keep this ordering intact so post-tick hooks
cannot invalidate a returned response or checkpoint.

## Migration Notes

When converting an existing Bevy game:

- keep rendering systems in `Update`, but make them read simulation state rather than mutate rules;
- use `AgentControlPlugins::default()` for the standard core/snapshot/replay stack;
- use `with_snapshot_policy`, `without_snapshots`, and `without_replay` only when the app intentionally needs a non-standard control stack;
- use `VisualCaptureAppExt::insert_visual_capture_renderer` for headless screenshots, or enable the runner `visual` feature for Bevy primary-window screenshots;
- install `BevyRemoteControlPlugin` into visual apps and call `app.run()` so winit and screenshot capture stay on the main thread;
- register autonomous decision systems in `AgentDecision`, never render-frame `Update`;
- declare environment metadata, supported actions, supported observation modes, and both extractors before constructing an environment;
- register full observation and custom-action schemas; core enforces their cached validators and remote discovery exposes those contracts;
- implement `SnapshotType` using an explicit ID that survives Rust module moves, and increase its version when its serialized representation changes;
- handle registration macro/method results; register essential resources as required and only genuinely optional resources with `register_snapshot_resources!`;
- inspect private live registry/store/recorder/timeline collections through getters and use checked owner APIs; use runner methods to coordinate state, history, and branch changes;
- move random decisions behind a seeded gameplay RNG resource and register it for snapshots;
- move timers that affect gameplay to tick counters or `SimClock`;
- replace raw `Entity` IDs in observations/actions with stable IDs;
- make reset idempotent by clearing snapshot entities and respawning from seed/options;
- compute checksums with `StableHasher` or another explicitly stable deterministic hasher, not Rust's default hasher;
- test the headless app before adjusting visual mode.

Snapshot manifests, replay manifests, and portable replay bundles use schema
version 3. Regenerate older artifacts; do not add aliases, legacy layouts, or
migrations. Per-type versions and the checksum encoding version are independent
of the artifact schema version.

## Code Review Checklist

Reject or revise integrations that:

- read keyboard, mouse, gamepad, wall-clock time, OS randomness, or network state directly in authoritative gameplay systems;
- run gameplay rules in ordinary frame `Update` when they should be deterministic;
- snapshot render, window, audio, asset-server, socket, or UI internals;
- use raw Bevy `Entity` IDs as semantic identity in observations, actions, snapshots, or replay logs;
- omit gameplay resources/components from snapshot registration;
- use Rust type names as snapshot wire identity, ignore per-type versions, or allow required resources to disappear during restore;
- ignore fallible integration, schema, registration, scheduling, or collection results;
- publish `AgentAction::Custom` payloads without registering a matching JSON schema;
- compute observations by dumping excessive ECS state when compact symbolic state would work;
- use `DefaultHasher` or another unspecified hash implementation for determinism checks;
- implement screenshot capture by mutating gameplay state or adding visual state to checksums;
- expose mutation or restore over non-loopback remote control without a session token and capabilities;
- validate only through visuals instead of `tick`, `reward`, terminal flags, observation fields, and checksum.

## Validation

Run focused tests after integration:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

Add or update tests for:

- `step` increments one tick;
- queued and future actions apply once on the intended tick;
- `step_many` matches repeated `step`;
- snapshot, restore, and subsequent replay produce matching checksums;
- exported replay bundles load and restore in a fresh environment;
- branch creation does not mutate the parent timeline;
- reset with the same seed produces the same initial observation/checksum;
- observation policy hides state that should not be visible to the player;
- unsupported modes, invalid custom payloads, incompatible type versions, and rejected input sources fail before mutation;
- failed operations preserve state, pending input, cached output, and exported history;
- generated action/snapshot/rewind/fork/import sequences match fresh forward execution, including future behavior after restore;
- `agent.visual.capture` writes a non-empty PNG and enforces token/capability checks;
- a visual app captures its real primary window through the remote callback under Xvfb/Mesa, with correct tick/dimensions and clean listener shutdown;
- remote methods return valid JSON-RPC schemas, include registered custom actions and observation variants, and enforce token/capabilities.

## References

Read `references/integration-patterns.md` for concrete imports, component/resource registration, action, reset, observation, checksum, remote, and testing patterns.

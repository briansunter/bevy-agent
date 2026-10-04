---
name: control-bevy-agent-game
description: "Operate a Bevy game that exposes the bevy-agent protocol. Use when Codex needs to run a controllable Bevy environment, inspect action and observation schemas, drive simulation through JSON-RPC, agentctl, Python, or stdio, step ticks deterministically, capture screenshots on demand, fast-forward, snapshot, restore, branch timelines, export or replay action traces, debug observations/rewards/checksums, or verify agent-game behavior without keyboard or mouse input."
metadata:
  short-description: Drive bevy-agent games
---

# Control Bevy Agent Game

## Operating Rule

Drive the game through its structured simulation API. Do not fake keyboard, mouse, gamepad, or browser input unless the user explicitly asks to test a human-input adapter.

The unit of work is a simulation tick plus a domain action, not a rendered frame. Treat observations, rewards, terminal flags, snapshots, and checksums as the source of truth.

## First Checks

1. Identify the control surface in this order: existing `agentctl`, Python client, HTTP JSON-RPC, stdio JSON-RPC, direct Rust `AgentApp` tests/examples.
2. Start or locate the runtime. Prefer localhost-only servers and record the command you used.
3. Inspect capabilities before acting: `agent.info`, `agent.action_space`, `agent.observation_space`, and `agent.schema`.
4. Reset with an explicit seed when behavior should be reproducible.
5. Step one small action first, verify the response shape, then run batches.

For the sample platformer:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
cargo run -p bevy_agent_cli --bin agentctl -- info
cargo run -p bevy_agent_cli --bin agentctl -- schema
cargo run -p bevy_agent_cli --bin agentctl -- reset --seed 42
```

## Action Loop

Use short inspect-act-check loops:

1. Read the current observation.
2. Choose one action or a small batch.
3. Step or `step_many`.
4. Check `tick`, `reward`, `done`, `truncated`, `info.actions_applied`, and `checksum`.
5. Snapshot before trying a divergent plan.
6. Restore a snapshot or tick before comparing alternatives.

Example domain actions:

```json
{"type":"Move","x":1.0,"y":0.0}
{"type":"Jump"}
{"type":"Noop"}
```

Example commands:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p bevy_agent_cli --bin agentctl -- step-many '[{"type":"Move","x":1.0,"y":0.0},{"type":"Jump"}]'
cargo run -p bevy_agent_cli --bin agentctl -- capture --out-dir screenshots --label after_step
cargo run -p bevy_agent_cli --bin agentctl -- fast-forward 30
cargo run -p bevy_agent_cli --bin agentctl -- snapshot
cargo run -p bevy_agent_cli --bin agentctl -- restore-tick 10
cargo run -p bevy_agent_cli --bin agentctl -- branch --from-tick 10 --label try_jump
```

## Observations

Read observations as state, not prose. For `Observation::Hybrid`, prefer:

- `symbolic.player` for controllable character state;
- `symbolic.visible_entities` for nearby interactables and hazards;
- `symbolic.objectives` for task progress;
- `debug` only when the test intentionally needs privileged state.

Do not infer success from visuals alone. A good verification cites the final tick, reward/terminal status, and checksum or relevant symbolic fields.

## Visual Capture

Use `agent.visual.capture` or `agentctl capture` only when visual context is needed. For low-speed play, alternate one structured `step` with an optional `capture`, inspect the returned PNG path and symbolic response, then choose the next action.

Example:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p bevy_agent_cli --bin agentctl -- capture --out-dir screenshots --label tick_1
```

Treat screenshots as supplemental evidence. Report the capture path alongside tick, reward, terminal state, and checksum.

## Determinism

When debugging behavior or validating a fix:

1. Reset with the same seed.
2. Run the same action list twice.
3. Compare final `tick`, `reward`, `done`, and `checksum`.
4. If they differ, inspect hidden nondeterminism before changing the agent strategy.

Use snapshots for branching. A branch should let you try alternate action plans without mutating the parent timeline.

## Remote Safety

Use tokened remote mode when `AGENT_TOKEN` is set:

```sh
AGENT_TOKEN=secret cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
cargo run -p bevy_agent_cli --bin agentctl -- --token secret info
```

Never expose mutation, restore, branch, or replay-load endpoints on a public interface unless the user explicitly asks and the app has authentication/capability checks.

## Stdio Mode

Use stdio when a long-lived HTTP server is unnecessary:

```sh
cargo run -p sample_platformer --example remote_stdio
```

Then send one JSON-RPC request per line. Keep request IDs stable enough that responses can be matched to actions.

## Cleanup

Stop any server process you started unless the user asked to keep it running. In the final response, report:

- runtime command or URL used;
- seed and action sequence for reproducible runs;
- final tick/reward/done/truncated/checksum;
- snapshots or replay files created;
- any server left running.

## References

Read `references/protocol.md` when you need exact method names, request shapes, response fields, Python client examples, or troubleshooting checks.

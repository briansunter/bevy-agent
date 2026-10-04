---
name: bevy-agent
description: "Set up Bevy Agent, integrate a Bevy game's deterministic action/state contract, and operate it through Rust, agentctl, Python, or JSON-RPC. Use for a first environment, end-to-end game integration, or runtime stepping, snapshots, replay, capture, and recovery."
metadata:
  short-description: Set up, integrate, and operate Bevy agent games
---

# Bevy Agent

Make the user's game controllable through explicit simulation ticks, then use that interface to observe, act, and verify outcomes. The game owns its rules; Bevy Agent owns the control and history machinery. A rendered frame is not a simulation tick.

## Choose the starting point

Inspect the user's repository or running endpoint before changing it. Establish the requested outcome, existing integration, dependency versions, app builder, transport, and ownership of the running process. Reuse a working server when the task is runtime-only. Do not reset a user's running episode merely to discover its capabilities.

| Situation | Read next | First useful result |
| --- | --- | --- |
| New environment or dependency setup | [Setup](references/setup.md) | Runnable counter with two passing regression tests |
| Existing Bevy game | [Integration](references/integration.md) | One domain action produces a verified gameplay transition |
| Running game or client workflow | [Runtime](references/runtime.md) | Discover the contract, observe, then perform the requested action |
| Timeout, fault, rejected input, or divergence | [Recovery](references/recovery.md) | Identify the failed boundary and recover without duplicating a mutation |

Read only the reference needed for the current phase. The complete [counter starter](assets/counter/src/main.rs) and [manifest](assets/counter/Cargo.toml) can be copied together into a new project; no sibling skill or repository checkout is required for that starter.

## Compatibility baseline

This skill's executable starter targets **Bevy Agent 0.0.4**, **Bevy 0.18.1**, and **Rust 1.91+**. Inspect an existing game's manifest and lockfile before adapting it. Do not silently upgrade Bevy or mix companion versions to make an example fit.

- Use exact matching `=0.0.4` requirements for companion crates at this baseline. There is no umbrella `bevy_agent` package.
- Cargo package `bevy_agent_cli` installs the binary `agentctl`. Installing it does not start a game.
- `sample_platformer`, the Python client, and these skills are distributed through Git. The counter is also included in the runner crate.
- Snapshot/replay artifact format **3**, per-type schema versions, and Cargo versions are independent contracts. Do not infer format compatibility from a package number.
- For another release, use its source and contract rather than guessing from these examples. The [public guide](https://briansunter.github.io/bevy-agent/) explains current behavior.

## Integration contract

Keep these invariants across setup, implementation, and operation:

1. Run authoritative game systems in `AgentTick`; consume `CurrentInputFrame<AgentAction>`. Convert human input into the same domain action path when human control is in scope.
2. Use `SimClock`, seeded gameplay randomness, stable identities, and explicit ordering for dependent systems. Keep render time, sockets, UI, audio, and asset-loading state outside the simulation.
3. Treat reset, snapshots, and checksums as one inventory of all state that affects future outcomes, including hidden state. Register required resources as required.
4. Declare supported actions, observation modes, and metadata. Install both observation and checksum extractors before constructing `AgentApp`. Validate custom action payloads and the full observation envelope with their schemas.
5. Handle fallible construction, registration, scheduling, and runtime calls. A stale observation is not a substitute for a failed step.
6. For a rendered app, keep Bevy's main thread and normal runner; use `BevyRemoteControlPlugin`. Add networking only after local stepping works.

## Runtime discipline

Discover before acting: `info`, `action-space`, `observation-space`, `schema`. Use an observation mode the game actually supports and honor the user's visibility constraints; do not default to privileged debug state for a player-limited task.

Choose one supported action, inspect the response, and expand to small batches only after confirming its meaning. Check tick, observation, reward, `done`, `truncated`, and checksum. Stop when the episode ends. Compare meaningful game outcomes as well as hashes.

For HTTP/WebSocket mutations, give each intended operation a unique retry key. Reuse that key only for the identical retry while retained by the same server. After uncertainty, consult operation status; unknown status does not establish that nothing ran. Read [Recovery](references/recovery.md) before issuing another mutation.

## Finish with evidence

Match validation to what changed:

- **Setup:** compile/run the actual starter; report its output and dependency versions.
- **Integration:** assert game outcomes, repeated seeded resets, restore-and-repeat, and replay reconstruction. Extend tests to changed custom actions or hidden state.
- **Runtime:** report the endpoint/process, seed if known, action sequence, final tick, relevant observation, terminal flags, and checksum. Save a compact trace for a bug reproduction.
- **Capture:** inspect the returned image and its tick/dimensions; compilation or a path alone is not visual evidence.

Distinguish verified behavior from untested transports, graphics, or platforms. This skill does not authorize publishing crates, deploying listeners, posting announcements, or resetting unrelated sessions. Use the user's task scope and the host's build/storage instructions. On the pinned personal Mac mini, run `build-storage-check` and invoke `cargo-storage` from the canonical worktree instead of redirecting compiler output.

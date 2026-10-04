# Use Bevy Agent with an agent

The repository includes a portable **`bevy-agent` skill** for setup, integration, and runtime control. It gives a coding agent the project's contracts, a complete starter, and operational guidance so it can work from a concrete game or endpoint.

[Read the skill on GitHub](https://github.com/briansunter/bevy-agent/blob/master/skills/bevy-agent/SKILL.md)

## Use it from the repository

Point your agent at the skill file:

```text
Use skills/bevy-agent/SKILL.md to integrate this Bevy game.
Preserve its gameplay rules, start with one controllable action,
and verify seeded reset, snapshot restore, and replay.
```

For a running environment:

```text
Use skills/bevy-agent/SKILL.md to inspect the environment at
http://127.0.0.1:4000/rpc. Preserve the current episode, discover
its actions and observation modes, and report the current state.
```

Once installed in your agent's supported skill directory, invoke it as `$bevy-agent`. Follow that agent's installation mechanism; simply having a folder in an arbitrary checkout does not guarantee automatic discovery.

## What's included

| Resource | Use it for |
| --- | --- |
| `SKILL.md` | Choose the workflow and preserve the core integration/runtime contracts |
| `references/setup.md` | Dependencies, a first environment, CLI installation, repository examples |
| `references/integration.md` | State inventory, schedules, schemas, snapshots, checksums, server integration |
| `references/runtime.md` | Discovery, action loops, snapshots, branches, replay, Python, capture, transports |
| `references/recovery.md` | Uncertain mutations, operation status, partial failures, and diagnosis |
| `assets/counter/` | Complete standalone Rust project with two regression tests |
| `agents/openai.yaml` | Display metadata and a default invocation prompt |

Copy the entire `skills/bevy-agent/` directory when moving the skill. Its references and starter are self-contained; the optional platformer and Python examples require the repository. The starter targets Bevy Agent 0.0.4, Bevy 0.18.1, and Rust 1.91+.

## What a useful result looks like

For setup, the agent should run the starter and its tests. For integration, it should prove a meaningful gameplay outcome and repeatability after reset/restore. For runtime work, it should report the action sequence, tick, observation, terminal state, and checksum where available.

A build alone does not prove gameplay or capture. The skill asks for evidence at the boundary the task actually changes, and treats a timeout as an uncertain operation rather than permission to repeat it blindly.

The existing [integration skill](https://github.com/briansunter/bevy-agent/tree/master/skills/integrate-bevy-agent-control) and [runtime-control skill](https://github.com/briansunter/bevy-agent/tree/master/skills/control-bevy-agent-game) remain available for focused work.

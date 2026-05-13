# bevy_agent_control Operation Reference

## Local HTTP Runtime

Start the sample remote server:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
```

Use `agentctl`:

```sh
cargo run -p agentctl -- info
cargo run -p agentctl -- action-space
cargo run -p agentctl -- observation-space
cargo run -p agentctl -- schema
cargo run -p agentctl -- reset --seed 42
cargo run -p agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p agentctl -- fast-forward 30
cargo run -p agentctl -- snapshot
cargo run -p agentctl -- snapshots
cargo run -p agentctl -- restore <snapshot-id>
cargo run -p agentctl -- restore-tick 10
cargo run -p agentctl -- branch --from-tick 10 --label alt
cargo run -p agentctl -- replay-export replay.json
cargo run -p agentctl -- replay-load replay.json
```

Use tokened mode when available:

```sh
AGENT_TOKEN=secret cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
cargo run -p agentctl -- --token secret info
```

## JSON-RPC Methods

Core methods:

- `agent.info`
- `agent.action_space`
- `agent.observation_space`
- `agent.schema`
- `agent.reset`
- `agent.step`
- `agent.step_many`
- `agent.fast_forward`
- `agent.observe`
- `agent.snapshot.create`
- `agent.snapshot.restore`
- `agent.snapshot.list`
- `agent.snapshot.delete`
- `agent.timeline.current`
- `agent.timeline.branch`
- `agent.timeline.restore_tick`
- `agent.replay.start`
- `agent.replay.stop`
- `agent.replay.export`
- `agent.replay.load`
- `agent.control.pause`
- `agent.control.resume`
- `agent.control.set_mode`

## Curl Smoke Tests

Use curl when no CLI wrapper exists:

```sh
curl -s http://127.0.0.1:4000/rpc \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"agent.info","params":{}}'
```

Reset with a seed:

```sh
curl -s http://127.0.0.1:4000/rpc \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"agent.reset","params":{"options":{"seed":42,"observation_mode":"Hybrid","create_initial_snapshot":true}}}'
```

## Request Examples

Step:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "agent.step",
  "params": {
    "action": {"type": "Move", "x": 1.0, "y": 0.0},
    "observation_mode": "Hybrid"
  }
}
```

Step many:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "agent.step_many",
  "params": {
    "actions": [
      {"type": "Move", "x": 1.0, "y": 0.0},
      {"type": "Jump"}
    ],
    "stop_on_done": true,
    "return_observations": "last"
  }
}
```

Reset:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "agent.reset",
  "params": {
    "options": {
      "seed": 42,
      "observation_mode": "Hybrid",
      "create_initial_snapshot": true
    }
  }
}
```

## Python Client

```python
from bevy_agent_client import AgentClient

env = AgentClient("http://127.0.0.1:4000/rpc")
print(env.info())
obs = env.reset(seed=42)
step = env.step({"type": "Move", "x": 1.0, "y": 0.0})
snapshot = env.snapshot()
env.restore(snapshot["snapshot_id"])
```

## Response Checks

Always inspect:

- `tick`: confirms one simulation tick or intended batch range advanced.
- `reward`: immediate objective signal.
- `done` and `truncated`: stop action loops when true.
- `info.actions_applied`: confirms input was consumed.
- `info.snapshot_created`: records checkpoint creation.
- `checksum`: compare across replay or restore tests.

## Troubleshooting

- No movement: inspect `agent.action_space` and confirm action names/fields match the schema exactly.
- Actions ignored: check `info.actions_applied`, scheduled tick, pause/control mode, and whether the episode is already `done` or `truncated`.
- Nondeterministic replay: compare initial seed/options, action order, tick count, RNG resources, and checksum inputs.
- Restore mismatch: ensure the snapshot ID or restore tick belongs to the active timeline/branch.
- HTTP 401/403: pass the same token used by the runtime, usually through `AGENT_TOKEN` or an `agentctl --token` flag.
- Connection refused: verify the server command is still running, the bind address is localhost, and the port matches the client.

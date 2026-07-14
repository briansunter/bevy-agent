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
cargo run -p agentctl -- capture --out-dir screenshots --label tick_1
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
- `agent.visual.capture`
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

## Security and Capabilities

The bridge gates methods with an optional session token and a capability set.

- Session token: when `RemoteSecurity.session_token` is configured, every method requires `params.session_token` to match it. No token is required when none is configured. `agentctl` sends it through `--token` or `AGENT_TOKEN`.
- Read-only inspectors (`agent.info`, `agent.action_space`, `agent.observation_space`, `agent.schema`, `agent.timeline.current`) require the token when configured but need no capability.
- Mutating methods require both the token and the matching capability.

Capability bits — the default set grants every capability except `MUTATE_ECS` and `SPAWN_DESPAWN`:

- `STEP`: `agent.reset`, `agent.step`, `agent.step_many`, `agent.fast_forward`, and the replay methods.
- `OBSERVE_PLAYER`: `agent.observe`.
- `VISUAL_CAPTURE`: `agent.visual.capture`.
- `SNAPSHOT`: `agent.snapshot.create`, `agent.snapshot.list`, `agent.snapshot.delete`.
- `RESTORE`: `agent.snapshot.restore`, `agent.timeline.restore_tick`.
- `BRANCH`: `agent.timeline.branch`.
- `CONTROL`: `agent.control.pause`, `agent.control.resume`, `agent.control.set_mode`.
- `MUTATE_ECS`, `SPAWN_DESPAWN`: reserved, not granted by default.

Authentication and capability failures are returned as JSON-RPC errors in an HTTP 200 response, not as HTTP 401/403. Unauthenticated binds are only permitted on loopback; set `AGENT_TOKEN` before binding a public interface such as `0.0.0.0`.

Tokenless WebSocket sessions must omit the browser `Origin` header; browser clients should configure a session token.

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

Capture:

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "agent.visual.capture",
  "params": {
    "output_dir": "screenshots",
    "label": "tick_1",
    "timeout_frames": 8
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
capture = env.capture(output_dir="screenshots", label="after_step")
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
- `path`: for `agent.visual.capture`, verify the returned PNG exists and is non-empty before using it as visual evidence.

## Troubleshooting

- No movement: inspect `agent.action_space` and confirm action names/fields match the schema exactly.
- Actions ignored: check `info.actions_applied`, scheduled tick, pause/control mode, and whether the episode is already `done` or `truncated`.
- Nondeterministic replay: compare initial seed/options, action order, tick count, RNG resources, and checksum inputs.
- Restore mismatch: ensure the snapshot ID or restore tick belongs to the active timeline/branch.
- Capture missing: ensure the app has the `VISUAL_CAPTURE` capability and either a registered visual capture renderer or a visual build with screenshot support.
- Auth or capability rejected: the bridge reports these as JSON-RPC errors inside an HTTP 200 body (for example an `-32603` "invalid or missing session token" or "missing remote capability" message), not as HTTP 401/403. Pass the runtime's token via `AGENT_TOKEN` or `agentctl --token`; mutating methods also need the matching capability in `RemoteSecurity.capabilities`.
- Connection refused: verify the server command is still running, the bind address is localhost, and the port matches the client.

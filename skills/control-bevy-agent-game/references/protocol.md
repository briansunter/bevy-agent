# bevy-agent Operation Reference

## Local HTTP Runtime

Start the sample remote server:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
```

Use `agentctl`:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- info
cargo run -p bevy_agent_cli --bin agentctl -- action-space
cargo run -p bevy_agent_cli --bin agentctl -- observation-space
cargo run -p bevy_agent_cli --bin agentctl -- schema
cargo run -p bevy_agent_cli --bin agentctl -- reset --seed 42
cargo run -p bevy_agent_cli --bin agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p bevy_agent_cli --bin agentctl -- capture --out-dir screenshots --label tick_1
cargo run -p bevy_agent_cli --bin agentctl -- fast-forward 30
cargo run -p bevy_agent_cli --bin agentctl -- snapshot
cargo run -p bevy_agent_cli --bin agentctl -- snapshots
cargo run -p bevy_agent_cli --bin agentctl -- restore <snapshot-id>
cargo run -p bevy_agent_cli --bin agentctl -- restore-tick 10
cargo run -p bevy_agent_cli --bin agentctl -- branch --from-tick 10 --label alt
cargo run -p bevy_agent_cli --bin agentctl -- replay-export replay.json
cargo run -p bevy_agent_cli --bin agentctl -- replay-load replay.json
```

Use tokened mode when available:

```sh
AGENT_TOKEN=secret cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
cargo run -p bevy_agent_cli --bin agentctl -- --token secret info
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
- `agent.operations.status`

`agent.reset` returns a stable envelope with `tick`, `observation`,
`checksum`, `snapshot_id`, `timeline_id`, and `branch_id`.
`agent.step_many` always returns the same object shape. Its top-level fields
include aggregate `reward`, separate `done` and `truncated`, last `info`,
last `checksum`, and `responses` (populated when `return_observations` is
`all`). Replay export returns a portable `bundle` containing the log and every
referenced snapshot.

Inline replay export returns `bundle`; file export returns the artifact path
and counts. Load accepts exactly one of `bundle` or `path`. Only current
version-3 bundles are supported. `agent.replay.stop` returns recording status
and record count; retrieve payloads with export.

## Request contracts and recoverable outcomes

Parameter objects reject unknown fields. `agent.schema.methods` is generated
from the same DTOs decoded by the dispatcher. Custom action values must satisfy
a registered JSON Schema. Observation schemas describe the full serialized
`Observation`, including the outer `Domain` wrapper when used. Discovery lists
only supported modes; unsupported requests fail explicitly. Omitted step and
observe modes inherit the current validated mode.

An HTTP/WebSocket timeout includes an opaque string `error.data.operation_id` and
`execution_state`. Query `agent.operations.status` with
`{"operation_id":"<opaque-id>"}` and the same session token before retrying a
mutation. CLI: `operation-status <id>`; Python: `operation_status(id)`.
Status contains `state` (`queued`, `running`, `completed`, `cancelled`, `error`)
and the original `response` when terminal. Default retention is five minutes,
up to 256 entries and 64 MiB, with earlier eviction under pressure. Expiration
or an unknown ID does not establish that execution was cancelled.

For idempotent HTTP/WebSocket mutations, supply an envelope `retry_key` such as
`"episode-2.tick-1"`. It accepts 1–128 ASCII letters, digits, or `-_.:`. Resend the
same method and parameters with the same key and any new JSON-RPC ID; the
server returns the same retained outcome without repeating the mutation.
Different parameters with an existing key are rejected. Status accepts exactly
one of `operation_id` or `retry_key`, so a client can recover before learning an
operation ID. CLI: `--retry-key KEY step ...`, then `operation-status --key KEY`.
Python: `step(action, retry_key=KEY)`, then `operation_status(retry_key=KEY)`.
Keys expire with outcomes and are scoped to one server instance. Unknown or
expired keys cannot guarantee deduplication after eviction or restart. Stdio
and direct bridge calls reject envelope retry keys.

Batches always stop at terminal state. `stop_on_done` is removed and rejected.
Errors after reset/step mutation expose `tick_before`, `tick_after`,
`tick_committed`, `completed_steps`, and `recovery_required` in `error.data`.
When recovery is required, reset must succeed before further gameplay access.
Python raises `RemoteError` and preserves that data.

## Security and Capabilities

The bridge gates methods with an optional session token and a capability set.

- Session token: when `RemoteSecurity.session_token` is configured, every method requires `params.session_token` to match it. No token is required when none is configured. `agentctl` sends it through `--token` or `AGENT_TOKEN`.
- Read-only inspectors (`agent.info`, `agent.action_space`, `agent.observation_space`, `agent.schema`, `agent.timeline.current`, `agent.operations.status`) require the token when configured but need no capability.
- Mutating methods require both the token and the matching capability.

Capability bits — the default set grants supported capabilities except `FILESYSTEM`:

- `STEP`: `agent.reset`, `agent.step`, `agent.step_many`, `agent.fast_forward`, `agent.replay.start`, and `agent.replay.stop`.
- `OBSERVE_PLAYER`: observations in `PlayerKnowledge`, `DiffSinceLastTick`, or `PixelFrame` mode.
- `OBSERVE_FULL_STATE`: observations in `Hybrid` or `FullDebugState` mode, including responses from reset/step/restore.
- `VISUAL_CAPTURE`: `agent.visual.capture`.
- `SNAPSHOT`: `agent.snapshot.create`, `agent.snapshot.list`, `agent.snapshot.delete`.
- `RESTORE`: `agent.snapshot.restore`, `agent.timeline.restore_tick`, and `agent.replay.load`.
- `BRANCH`: `agent.timeline.branch`.
- `CONTROL`: `agent.control.pause`, `agent.control.resume`, `agent.control.set_mode`.
- `SNAPSHOT_EXPORT`: `agent.replay.export`.
- `FILESYSTEM`: replay export/load using `path`, and visual captures using `output_dir`; required in addition to the operation's other capabilities.

The headless and visual HTTP examples explicitly grant `FILESYSTEM` and confine
artifacts to `./artifacts`, configurable with `--artifact-dir`. Relative paths
resolve under that root. Without filesystem permission, exchange replay bundles
inline; the stdio example uses the restrictive library defaults.

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
    "timeout_frames": 8,
    "source": "primary_window"
  }
}
```

## Python Client

Run `PYTHONPATH=python python3` from the repository root, then:

```python
from bevy_agent_client import AgentClient

env = AgentClient("http://127.0.0.1:4000/rpc")
print(env.info())
reset = env.reset(seed=42)
obs = reset["observation"]
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
- Restore mismatch: ensure the portable replay bundle contains the referenced snapshot and matches the running game/version.
- Capture missing: ensure the app has the `VISUAL_CAPTURE` capability and either a registered visual capture renderer or a visual app using `BevyRemoteControlPlugin`; use `source: "primary_window"` to bypass software capture.
- Auth or capability rejected: the bridge reports these as JSON-RPC errors inside an HTTP 200 body (code `-32001`, with an "invalid or missing session token" or "missing remote capability" message). Pass the runtime's token via `AGENT_TOKEN` or `agentctl --token`; mutating methods also need the matching capability in `RemoteSecurity.capabilities`.
- Connection refused: verify the server command is still running, the bind address is localhost, and the port matches the client.

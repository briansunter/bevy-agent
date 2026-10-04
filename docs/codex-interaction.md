# Codex Interaction Guide

Start the sample HTTP remote:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
```

Inspect it:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- info
cargo run -p bevy_agent_cli --bin agentctl -- schema
```

Reset and step:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- reset --seed 42
cargo run -p bevy_agent_cli --bin agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p bevy_agent_cli --bin agentctl -- step-many '[{"type":"Move","x":1.0,"y":0.0},{"type":"Jump"}]'
```

Capture visual state on demand:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- capture --out-dir screenshots --label tick_1
```

For low-speed visual play, alternate one `step` command with `capture`, inspect the symbolic response and PNG, then choose the next domain action. The sample platformer can write capture PNGs from the headless HTTP runtime; visual builds can also use Bevy primary-window screenshots.

Snapshot, restore, and branch:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- snapshot
cargo run -p bevy_agent_cli --bin agentctl -- restore <snapshot-id>
cargo run -p bevy_agent_cli --bin agentctl -- branch --from-tick 3 --label try_jump
```

Replay files:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- replay-export replay.json
cargo run -p bevy_agent_cli --bin agentctl -- replay-load replay.json
```

Security:

```sh
AGENT_TOKEN=secret cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
cargo run -p bevy_agent_cli --bin agentctl -- --token secret info
```

Remote servers may run without a token only on loopback binds. Set `AGENT_TOKEN` before binding to a public interface such as `0.0.0.0`. A missing or wrong token, or a missing capability, is returned as a JSON-RPC error in the HTTP 200 response body rather than as an HTTP 401/403; mutating methods such as `agent.control.pause`/`resume`/`set_mode` require the `CONTROL` capability.

Tokenless WebSocket sessions are intended for non-browser local clients; browser-originated WebSocket handshakes require a session token.

For direct tool sessions where HTTP is unnecessary, use stdio:

```sh
cargo run -p sample_platformer --example remote_stdio
```

Then send one JSON-RPC request per line.

## Recovering a timed-out operation

An HTTP/WebSocket server timeout error includes `error.data.operation_id` and
`execution_state`. A queued operation can be cancelled before execution; a
running operation can still complete. Preserve its opaque string identifier and
query the outcome before repeating a mutation:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- operation-status <operation-id>
```

Python exposes `env.operation_status(operation_id)`. Status retrieval uses the
same session token and returns `queued`, `running`, `completed`, `cancelled`, or
`error`, plus the original JSON-RPC response when available. Default retention
is five minutes, bounded by 256 entries and 64 MiB; older terminal results can
be evicted sooner under pressure. An unknown result does not prove a mutation
never happened.

To survive a disconnect before receiving an operation ID, choose a retry key
before sending the mutation:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- --retry-key episode-1.tick-1 step '{"type":"Noop"}'
cargo run -p bevy_agent_cli --bin agentctl -- operation-status --key episode-1.tick-1
```

Python supports `env.step(action, retry_key="episode-1.tick-1")` and
`env.operation_status(retry_key="episode-1.tick-1")`. Identical retries share one
execution while the outcome is retained. A changed method or payload with the
same key is rejected. Keys belong to one server instance and expire with the
outcome; they do not survive server restart. Use a new key for a new intent.

Step/reset errors after mutation begins include committed-tick and recovery
information in `error.data`; Python exposes it through `RemoteError.data`.
When `recovery_required` is true, reset must complete successfully before
stepping, observing, exporting, or navigating again. Batches always stop at
terminal state; the former `stop_on_done` parameter is rejected.

Discovery lists only the game's supported actions and observation modes. The
sample supports `PlayerKnowledge` and `Hybrid`; requesting another mode returns
an error. Step and observe inherit the current mode when none is supplied.
Snapshot, replay-log, and bundle formats are version 3; regenerate older files.

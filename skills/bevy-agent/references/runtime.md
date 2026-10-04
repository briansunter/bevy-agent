# Operate a running environment

## Locate or start the process

For an existing server, begin with read-only discovery. Record its endpoint and determine whether the user wants to preserve the current episode. Do not reset or start a duplicate listener automatically.

For a new local sample session, run from a repository checkout and leave the process alive:

```sh
cargo run -p sample_platformer --example remote_http --locked -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

Use another terminal for the client. The sample explicitly allows files under `./artifacts`; the library default disables filesystem operations. `sample_platformer` is not a registry package. For a custom game, use its own server command.

The CLI's default URL is `http://127.0.0.1:4000/rpc`. Override it explicitly when the server differs:

```sh
agentctl --url http://127.0.0.1:4001/rpc info
```

Use the same URL on subsequent commands. `AGENT_TOKEN` supplies a configured server token; keep its value out of traces and chat. Do not print credentials to debug an authentication failure.

## Discover the actual game contract

```sh
agentctl info
agentctl action-space
agentctl observation-space
agentctl schema
```

Check the supported actions, observation modes, capabilities, and game metadata. Do not assume every environment supports `Move`, `Jump`, `Hybrid`, or screenshots. `Noop` is also a declared action, not a universal fallback.

The CLI prints the full JSON-RPC envelope. Require a matching successful `result` before interpreting gameplay fields; an HTTP 200 may contain `error`. The Python client returns the unwrapped result or raises `RemoteError`.

Use the current observation for an existing episode:

```sh
agentctl observe
```

Choose a supported observation mode with the command's `--mode` option when the default does not fit. Respect player-limited visibility. Reset only when starting/restarting an episode is part of the task:

```sh
agentctl reset --seed 42
```

If the game does not support `Hybrid`, pass a supported `--mode` on reset and relevant requests. Preserve the explicit seed and selected mode in reproducibility evidence.

## Inspect → act → verify

For the sample platformer, a single movement request is:

```sh
agentctl --retry-key session-a.episode-1.tick-1 step '{"type":"Move","x":1.0,"y":0.0}'
```

Replace that example key with one unique to this session and intended operation. Reusing a retained key for a new operation can return an old result or a conflict. Choose actions from the game's catalog; a platformer movement example is not a generic action for another game.

Check:

| Field | Interpretation |
| --- | --- |
| `tick` | Completed tick; one successful `step` should advance the expected simulation tick |
| `observation` | State exposed by the selected observation mode |
| `reward` | Transition reward, interpreted using this game's objective |
| `done`, `truncated` | Stop the action loop when either is true |
| `info.actions_applied` | Input consumption evidence when present |
| `checksum` | Compare equivalent transitions when the environment returns it |

An applied action may legally produce no movement, for example against a wall. Confirm the actual objective in the observation. Once one action is understood, use small `step-many` batches or `fast-forward` only when their semantics fit the task. Batches stop at terminal state; do not assume the entire requested range ran.

## Save and compare decisions

```sh
agentctl snapshot
agentctl snapshots
```

Record the returned `result.snapshot_id`, current tick, and observation. Replace the placeholders below with values returned by this session:

```sh
agentctl restore <snapshot-id>
agentctl restore-tick <retained-tick>
agentctl branch --from-tick <retained-tick> --label try-alternate
```

Use restore to compare repeated behavior from the same saved state. Use a branch when you need a separate alternate future with lineage. History must actually be retained: the snapshot and replay owners each default to configurable 64 MiB budgets. An arbitrary old tick or ID may be unavailable.

For portable history on the sample server:

```sh
agentctl replay-export replay.json
agentctl replay-load replay.json
```

The file lives under the server's artifact root, not the client's directory. Loading history changes the environment; export for evidence without loading unless the task calls for reconstruction. Without `FILESYSTEM`, `replay-export` with no path returns an inline bundle; JSON-RPC load accepts exactly one of `bundle` or `path`.

## Capture meaningful visual evidence

If the game has a software renderer:

```sh
agentctl capture --out-dir screenshots --label after-step --source software
```

For the rendered repository example, stop an owned headless listener on that port first or choose another port:

```sh
cargo run -p sample_platformer --features visual --example remote_http_visual --locked -- 127.0.0.1:4000 --artifact-dir ./artifacts
agentctl capture --out-dir screenshots --label window --source primary_window
```

Run the capture command from another terminal while the server stays alive. `visual` alone does not give the counter a renderer. Capture needs the appropriate capability; an output path also requires filesystem permission.

Inspect the returned PNG and its `tick`, `frame`, width, and height. Resolve a relative path on the server. A remote server path is not automatically accessible on the client; use the user's existing file-access mechanism. Pair screenshots with semantic state instead of declaring a gameplay result from pixels alone.

## Python policy loop

The client is in the repository's `python/` directory, not PyPI. Start the sample server, then run from the checkout with `PYTHONPATH=python python3`:

```python
from bevy_agent_client import AgentClient, RemoteError

client = AgentClient("http://127.0.0.1:4000/rpc")
print(client.action_space())
initial = client.reset(seed=42)  # Starts a new episode intentionally.
for index in range(10):
    try:
        step = client.step(
            {"type": "Move", "x": 1.0, "y": 0.0},
            retry_key=f"unique-session.episode-1.step-{index}",
        )
    except RemoteError as error:
        print(error.code, error.data)
        break  # Resolve the error before any further mutation.
    print(step["tick"], step["observation"], step["checksum"])
    if step["done"] or step["truncated"]:
        break
```

Use a genuinely unique session prefix when executing this example. A production policy chooses actions from the observation instead of repeating sample movement. For timeouts or transport exceptions, preserve the retry key and follow [Recovery](recovery.md); do not wrap mutations in blind automatic retries.

## Other transports

HTTP uses `POST /rpc`; `GET /health` only checks the listener. WebSocket uses `GET /ws` on the same server. Send JSON-RPC envelopes and include `params.session_token` when configured. Browser-originated WebSocket sessions require a token; tokenless local sessions must omit the browser `Origin` header.

Stdio uses one JSON request per stdin line and one response per stdout line. Keep logs on stderr. The repository's `remote_stdio` example and Python `StdioAgentClient` support a subprocess workflow. Stdio rejects envelope `retry_key` and has no retained operation ledger.

For exact payloads, use discovery or the [protocol reference](https://briansunter.github.io/bevy-agent/reference/protocol.html). Stop only a process you own when the task is finished; leave an explicitly requested running environment available and report how to reconnect.

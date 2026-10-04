# Recovery and diagnosis

## A lost response is an uncertain mutation

HTTP and WebSocket share retained operation outcomes within a running server. If a mutation times out, preserve its method, payload, retry key, and any returned `operation_id` or `execution_state`.

```sh
agentctl operation-status --key <retry-key>
agentctl operation-status <operation-id>
```

Use one available identifier, replacing the placeholder with the real value. Status may be `queued`, `running`, `completed`, `cancelled`, or `error`. Inspect any retained original response; an operation reaching a terminal state does not by itself mean the game action succeeded.

| Evidence | Next action |
| --- | --- |
| Queued or running | Wait and check status at a bounded interval; do not submit the same intent under a new key |
| Completed with a successful response | Consume that response and verify its tick and state |
| Retained error | Inspect the original error, committed ticks, and recovery flag |
| Key unknown/expired, connection lost, or server restarted | Outcome remains uncertain; reconcile with safe discovery/current state where available before deciding to reset or continue |
| `recovery_required: true` | A successful reset is required before gameplay access can resume |

An identical request with the same retained key shares one execution. Changing method or payload under that key is rejected. Keys are 1–128 ASCII letters, digits, or `-_.:`. Default retention is five minutes, bounded by 256 entries and 64 MiB; eviction can happen earlier. These limits do not guarantee a key remains available. Stdio and direct bridge calls do not provide this ledger.

If the user wants to preserve the current episode and reconciliation cannot establish state, explain the unresolved outcome before discarding it. A reset starts a new episode, not a transparent retry.

## Inspect partial failure

A mutation error can include `tick_before`, `tick_after`, `tick_committed`, `completed_steps`, and `recovery_required` in `error.data`. Preserve these fields. Do not report the requested batch length when only a prefix committed, or present an earlier observation as the failed operation's result.

When faulted, reset must succeed before stepping, observing, exporting, or navigating history. Discovery and operation status can still help diagnose the boundary. Handle failure of reset itself rather than assuming the world recovered.

## Troubleshoot by boundary

| Symptom | Inspect first |
| --- | --- |
| Cargo cannot find `bevy_agent` | Use the named companion crates; there is no umbrella package |
| Installed `agentctl` is unrelated | Package must be `bevy_agent_cli`; confirm its version |
| Connection refused | Existing process, listener address, port, and `/rpc`; installing the CLI starts no server |
| HTTP 200 but no useful result | JSON-RPC `error`, token, capabilities, schema, and supported mode |
| Construction fails | Plugins, catalogs, metadata, supported default mode, both extractors, required resources |
| Custom action rejected | Discovered envelope and registered payload schema; do not bypass validation |
| Observation rejected | Schema must describe the full `Observation`, including `kind`, `tick`, and nested data |
| Input has no effect | Applied-action count, schedule tick, control mode, pause state, terminal flags, and game rules |
| State moves without an agent tick | Authoritative systems in `Update`, wall-clock timing, or input adapters mutating state directly |
| Repeated seeded runs disagree | Reset inventory, unseeded RNG, system/query ordering, clock use, and external state |
| Restore changes the next transition | Hidden state coverage, required resources, RNG, stable entity/type identities |
| Hashes agree but behavior differs | Missing checksum fields or stale presentation; assert game state directly |
| Old replay rejected | Artifact format, game/type schemas, and checkpoint compatibility; no built-in legacy migration |
| Tick or snapshot unavailable | Retention budget, history ancestry, and whether the checkpoint still exists |
| Export/capture cannot write | `FILESYSTEM` plus the operation capability and configured artifact root |
| PNG in unexpected directory | Paths resolve on the server; inspect the returned path |
| Primary-window capture fails | Rendered app, `visual` features, main-thread remote plugin, active graphics environment |

## Keep a useful reproduction

Record the game and runtime versions, seed/options, observation mode, shortest failing action sequence, expected game outcome, first divergent tick, response/error, and checksums. Keep a portable replay bundle when available and appropriate. Exclude session tokens from saved commands and logs.

Prove a fix by repeating the failing sequence and checking the affected next transition after restore, not just by getting a build to pass. Use the local counter to isolate setup problems; use the game's own semantic assertions to validate its behavior.

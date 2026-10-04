# Retries and recovery

A network timeout is uncertainty about the result. The game may have executed a mutation even though the client did not receive its response.

## Give one mutation a stable identity

```sh
agentctl --retry-key episode-1.tick-1 step '{"type":"Noop"}'
agentctl operation-status --key episode-1.tick-1
```

Identical requests with the same retained key share one execution. A different method or payload with that key is rejected. Use a new key for a new intended action, not for a retry of the same action.

Keys accept 1–128 ASCII letters, digits, or `-_.:`. They belong to one HTTP/WebSocket server and expire with its retained outcomes. Stdio has no retry ledger.

## Look up a timed-out operation

A server timeout may return `error.data.operation_id` and `execution_state`:

```sh
agentctl operation-status <operation-id>
```

Replace the placeholder with the opaque identifier returned by the server. Status can be `queued`, `running`, `completed`, `cancelled`, or `error`; a terminal result can include the original JSON-RPC response.

Default retention is five minutes, bounded by 256 entries and 64 MiB. Eviction can happen earlier under pressure. An unknown or expired result does not prove the mutation never ran.

## When the environment needs a reset

Errors after a reset or step begins include committed-tick information and `recovery_required` in `error.data`. If recovery is required, reset must succeed before stepping, observing, exporting, or navigating again.

A partially committed operation is not a successful requested tick. Preserve the error and its committed state instead of silently using an older observation.

## Check response bodies

Authentication and capability errors can arrive with HTTP 200. Inspect the JSON-RPC `error` field. The Python client raises `RemoteError` and preserves its `data`.

The [protocol reference](../reference/protocol.md) lists these request contracts. [Troubleshooting](../reference/troubleshooting.md) maps common symptoms to the next check.

# WebSocket and stdio

Choose the transport that fits your process boundary. All transports use JSON-RPC 2.0 and expose the game's declared action and observation contract.

| Transport | Endpoint | Best fit | Retained retry keys |
| --- | --- | --- | --- |
| HTTP | `POST /rpc` | CLI, Python, one-off requests | Yes, within one running server |
| WebSocket | `GET /ws` | A long-lived connection | Yes, shared server ledger |
| Stdio | One JSON request per input line | A tool-managed subprocess | No |

## WebSocket

The HTTP server exposes the WebSocket endpoint on the same listener:

```text
ws://127.0.0.1:4000/ws
```

Send a JSON-RPC envelope as a text message:

```json
{"jsonrpc":"2.0","id":1,"method":"agent.info","params":{}}
```

Browser-originated sessions require a configured session token. Tokenless local clients must omit the browser `Origin` header. Put the session token in `params.session_token` for each request when authentication is enabled.

HTTP and WebSocket share retained operation outcomes and retry keys within one server instance. A reconnect does not reset a still-retained key; a server restart does.

## Stdio

Run the repository example:

```sh
cargo run -p sample_platformer --example remote_stdio --locked
```

Send one request per line to the child process's stdin:

```json
{"jsonrpc":"2.0","id":1,"method":"agent.info","params":{}}
```

Read one response per line from stdout. Keep logs on stderr so they cannot be mistaken for protocol responses. The example uses the restrictive library filesystem defaults.

The Python client also exposes `StdioAgentClient` for an owned subprocess. See the [client source](https://github.com/briansunter/bevy-agent/blob/master/python/bevy_agent_client.py) for constructor and lifecycle options.

Stdio does not provide the server retry ledger. It rejects envelope `retry_key`; do not assume a lost response implies a mutation did not execute.

## A rendered game

A rendered app uses `BevyRemoteControlPlugin` to keep world mutation and primary-window capture on the Bevy main thread. Network I/O runs separately. Use the [rendered sample](./capture.md#capture-a-rendered-window) to see the integration.

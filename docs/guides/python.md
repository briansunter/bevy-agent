# Python

The repository provides a standard-library client for Python 3.10 or newer. It uses the same JSON-RPC contract as the CLI. It is not currently a PyPI package.

## Connect to the sample server

First start the [HTTP platformer](./remote-control.md#_2-start-a-complete-game-server). From the repository, open a Python session:

```sh
PYTHONPATH=python python3
```

```python
from bevy_agent_client import AgentClient

client = AgentClient("http://127.0.0.1:4000/rpc")
print(client.info())
print(client.action_space())
initial = client.reset(seed=42)
step = client.step({"type": "Move", "x": 1.0, "y": 0.0})
print(step["observation"])
```

The Python facade returns the JSON-RPC `result`, while the CLI displays the full envelope.

## Run a small policy loop

```python
client.reset(seed=42)
for _ in range(60):
    step = client.step({"type": "Move", "x": 1.0, "y": 0.0})
    print(step["tick"], step["reward"])
    if step["done"] or step["truncated"]:
        break
```

This simple policy is specific to the platformer. A useful agent chooses its next action from the current observation and the discovered action catalog.

## Compare two decisions

```python
saved = client.snapshot()
first = client.step({"type": "Move", "x": 1.0, "y": 0.0})
client.restore(saved["snapshot_id"])
alternate = client.step({"type": "Jump"})
```

The snapshot identifies server-owned state. It is not a local Python copy of the Bevy world.

## Recover uncertain mutations

Use a stable key for one intended action:

```python
step = client.step({"type": "Noop"}, retry_key="episode-1.tick-1")
status = client.operation_status(retry_key="episode-1.tick-1")
```

Catch `RemoteError` and inspect its `data` when an operation times out or a mutation fails. A key belongs to one running HTTP/WebSocket server and expires with retained outcomes. [Retries and recovery](./recovery.md) explains when it is safe to repeat a request.

For a subprocess connection, use [stdio](./transports.md#stdio).

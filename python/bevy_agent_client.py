"""Small stdlib client for bevy_agent_control JSON-RPC environments."""

from __future__ import annotations

import json
import subprocess
import urllib.request
from dataclasses import dataclass
from typing import Any
from urllib.error import HTTPError, URLError


HTTP_TIMEOUT_SECONDS = 30.0


class AgentError(RuntimeError):
    pass


@dataclass
class AgentClient:
    url: str = "http://127.0.0.1:4000/rpc"
    token: str | None = None
    _next_id: int = 1

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        params = dict(params or {})
        if self.token is not None:
            params["session_token"] = self.token
        request = {
            "jsonrpc": "2.0",
            "id": self._next_id,
            "method": method,
            "params": params,
        }
        self._next_id += 1
        payload = json.dumps(request).encode("utf-8")
        http_request = urllib.request.Request(
            self.url,
            data=payload,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(
                http_request, timeout=HTTP_TIMEOUT_SECONDS
            ) as response:
                message = json.loads(response.read().decode("utf-8"))
        except HTTPError as error:
            body = error.read().decode("utf-8", errors="replace").strip()
            detail = f": {body}" if body else ""
            raise AgentError(f"HTTP {error.code}{detail}") from error
        except URLError as error:
            raise AgentError(f"request failed: {error.reason}") from error
        if "error" in message:
            raise AgentError(message["error"]["message"])
        return message["result"]

    def info(self) -> Any:
        return self.call("agent.info")

    def reset(self, seed: int | None = 0, observation_mode: str = "Hybrid") -> Any:
        return self.call(
            "agent.reset",
            {
                "options": {
                    "seed": seed,
                    "observation_mode": observation_mode,
                    "create_initial_snapshot": True,
                }
            },
        )

    def step(self, action: dict[str, Any], observation_mode: str = "Hybrid") -> Any:
        return self.call(
            "agent.step",
            {"action": action, "observation_mode": observation_mode},
        )

    def step_many(
        self,
        actions: list[dict[str, Any]],
        return_observations: str = "last",
        stop_on_done: bool = True,
    ) -> Any:
        return self.call(
            "agent.step_many",
            {
                "actions": actions,
                "return_observations": return_observations,
                "stop_on_done": stop_on_done,
            },
        )

    def observe(self, observation_mode: str = "Hybrid") -> Any:
        return self.call("agent.observe", {"observation_mode": observation_mode})

    def capture(
        self,
        output_dir: str = "screenshots",
        label: str | None = None,
        timeout_frames: int = 8,
    ) -> Any:
        return self.call(
            "agent.visual.capture",
            {
                "output_dir": output_dir,
                "label": label,
                "timeout_frames": timeout_frames,
            },
        )

    def snapshot(self) -> Any:
        return self.call("agent.snapshot.create")

    def restore(self, snapshot_id: str) -> Any:
        return self.call("agent.snapshot.restore", {"snapshot_id": snapshot_id})

    def branch(self, from_tick: int, label: str | None = None) -> Any:
        return self.call(
            "agent.timeline.branch",
            {"from_tick": from_tick, "label": label},
        )

    def replay_export(self, path: str | None = None) -> Any:
        return self.call("agent.replay.export", {"path": path})

    def replay_load(self, path: str) -> Any:
        return self.call("agent.replay.load", {"path": path})


class StdioAgentClient:
    """JSON-RPC client for `cargo run -p sample_platformer --example remote_stdio`."""

    def __init__(self, argv: list[str] | None = None):
        self.process = subprocess.Popen(
            argv or ["cargo", "run", "-p", "sample_platformer", "--example", "remote_stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self._next_id = 1

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        if self.process.stdin is None or self.process.stdout is None:
            raise AgentError("stdio process is not available")
        request = {
            "jsonrpc": "2.0",
            "id": self._next_id,
            "method": method,
            "params": params or {},
        }
        self._next_id += 1
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        line = self.process.stdout.readline()
        message = json.loads(line)
        if "error" in message:
            raise AgentError(message["error"]["message"])
        return message["result"]

    def close(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()


if __name__ == "__main__":
    client = AgentClient()
    print(json.dumps(client.info(), indent=2))
    print(json.dumps(client.reset(), indent=2))
    print(json.dumps(client.step({"type": "Move", "x": 1.0, "y": 0.0}), indent=2))

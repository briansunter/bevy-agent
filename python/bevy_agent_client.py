"""Small stdlib client for bevy_agent_control JSON-RPC environments."""

from __future__ import annotations

import json
import os
import queue
import select
import socket
import subprocess
import threading
import time
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Literal, TypedDict
from urllib.error import HTTPError, URLError


HTTP_TIMEOUT_SECONDS = 30.0
STDIO_READ_TIMEOUT_SECONDS = 30.0
# Upper bound for a single JSON-RPC line on stdio (8 MiB).
MAX_LINE_BYTES = 8 * 1024 * 1024


class AgentError(RuntimeError):
    pass


class Checksum(TypedDict):
    tick: int
    hash: int


class StepInfo(TypedDict, total=False):
    frame: int
    timeline_id: str
    branch_id: str
    actions_applied: int
    snapshot_created: str | None
    episode_reason: str | None


class StepResponse(TypedDict):
    tick: int
    observation: dict[str, Any]
    reward: float
    done: bool
    truncated: bool
    info: StepInfo
    checksum: Checksum | None


class ResetResponse(TypedDict):
    tick: int
    observation: dict[str, Any]
    checksum: Checksum | None
    snapshot_id: str | None
    timeline_id: str
    branch_id: str


class StepManyResponse(TypedDict):
    start_tick: int
    end_tick: int
    steps: int
    observation: dict[str, Any] | None
    reward: float
    done: bool
    truncated: bool
    info: StepInfo | None
    checksum: Checksum | None
    responses: list[StepResponse]


class VisualCaptureResponse(TypedDict):
    tick: int
    frame: int
    path: str
    width: int
    height: int
    format: str


def validate_envelope(message: Any, expected_id: int) -> Any:
    """Validate a JSON-RPC response envelope, returning ``result``.

    Raises :class:`AgentError` on structural problems (non-object, wrong
    version, id mismatch, missing result) or on JSON-RPC error envelopes.
    """
    if not isinstance(message, dict):
        raise AgentError(f"invalid JSON-RPC envelope: expected object, got {type(message).__name__}")
    if message.get("jsonrpc") != "2.0":
        raise AgentError(f"invalid JSON-RPC version: {message.get('jsonrpc')!r}")
    if message.get("id") != expected_id:
        raise AgentError(
            f"JSON-RPC id mismatch: expected {expected_id}, got {message.get('id')!r}"
        )
    error = message.get("error")
    if error is not None:
        detail = error.get("message") if isinstance(error, dict) else error
        raise AgentError(f"JSON-RPC error {error!r}: {detail}")
    if "result" not in message:
        raise AgentError(f"invalid JSON-RPC envelope: missing result: {message!r}")
    return message["result"]


@dataclass
class AgentClient:
    url: str = "http://127.0.0.1:4000/rpc"
    token: str | None = None
    timeout: float = HTTP_TIMEOUT_SECONDS
    _next_id: int = 1

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        params = dict(params or {})
        if self.token is not None:
            params["session_token"] = self.token
        request_id = self._next_id
        request = {
            "jsonrpc": "2.0",
            "id": request_id,
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
            with urllib.request.urlopen(http_request, timeout=self.timeout) as response:
                raw = response.read().decode("utf-8")
        except HTTPError as error:
            try:
                body = error.read().decode("utf-8", errors="replace").strip()
            except Exception:
                body = ""
            detail = f": {body}" if body else ""
            raise AgentError(f"HTTP {error.code}{detail}") from error
        except (URLError, TimeoutError, socket.timeout) as error:
            reason = getattr(error, "reason", error)
            raise AgentError(f"request failed: {reason}") from error
        except OSError as error:
            raise AgentError(f"request failed: {error}") from error
        try:
            message = json.loads(raw)
        except json.JSONDecodeError as error:
            raise AgentError(f"invalid JSON response: {error}") from error
        try:
            return validate_envelope(message, request_id)
        except (AgentError, KeyError, TypeError) as error:
            if isinstance(error, AgentError):
                raise
            raise AgentError(f"invalid JSON-RPC envelope: {error}") from error

    def info(self) -> Any:
        return self.call("agent.info")

    def schema(self) -> Any:
        return self.call("agent.schema")

    def action_space(self) -> Any:
        return self.call("agent.action_space")

    def observation_space(self) -> Any:
        return self.call("agent.observation_space")

    def reset(
        self, seed: int | None = 0, observation_mode: str = "Hybrid"
    ) -> ResetResponse:
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

    def step(
        self, action: dict[str, Any], observation_mode: str = "Hybrid"
    ) -> StepResponse:
        return self.call(
            "agent.step",
            {"action": action, "observation_mode": observation_mode},
        )

    def step_many(
        self,
        actions: list[dict[str, Any]],
        return_observations: str = "last",
        stop_on_done: bool = True,
    ) -> StepManyResponse:
        return self.call(
            "agent.step_many",
            {
                "actions": actions,
                "return_observations": return_observations,
                "stop_on_done": stop_on_done,
            },
        )

    def fast_forward(self, ticks: int) -> StepResponse:
        return self.call("agent.fast_forward", {"ticks": ticks})

    def observe(self, observation_mode: str = "Hybrid") -> Any:
        return self.call("agent.observe", {"observation_mode": observation_mode})

    def capture(
        self,
        output_dir: str = "screenshots",
        label: str | None = None,
        timeout_frames: int = 8,
        source: Literal["auto", "software", "primary_window"] = "auto",
    ) -> VisualCaptureResponse:
        return self.call(
            "agent.visual.capture",
            {
                "output_dir": output_dir,
                "label": label,
                "timeout_frames": timeout_frames,
                "source": source,
            },
        )

    def snapshot(self) -> Any:
        return self.call("agent.snapshot.create")

    def snapshot_list(self) -> Any:
        return self.call("agent.snapshot.list")

    def snapshot_delete(self, snapshot_id: str) -> Any:
        return self.call("agent.snapshot.delete", {"snapshot_id": snapshot_id})

    def restore(self, snapshot_id: str) -> Any:
        return self.call("agent.snapshot.restore", {"snapshot_id": snapshot_id})

    def restore_tick(self, tick: int) -> Any:
        return self.call("agent.timeline.restore_tick", {"tick": tick})

    def timeline_current(self) -> Any:
        return self.call("agent.timeline.current")

    def branch(self, from_tick: int, label: str | None = None) -> Any:
        return self.call(
            "agent.timeline.branch",
            {"from_tick": from_tick, "label": label},
        )

    def control_set_mode(self, mode: str) -> Any:
        return self.call("agent.control.set_mode", {"mode": mode})

    def control_pause(self) -> Any:
        return self.call("agent.control.pause")

    def control_resume(self) -> Any:
        return self.call("agent.control.resume")

    def replay_start(self) -> Any:
        return self.call("agent.replay.start")

    def replay_stop(self) -> Any:
        return self.call("agent.replay.stop")

    def replay_export(self, path: str | None = None) -> Any:
        return self.call("agent.replay.export", {"path": path})

    def replay_load(self, path: str) -> Any:
        return self.call("agent.replay.load", {"path": path})


class StdioAgentClient:
    """JSON-RPC client for `cargo run -p sample_platformer --example remote_stdio`."""

    def __init__(
        self,
        argv: list[str] | None = None,
        timeout: float = STDIO_READ_TIMEOUT_SECONDS,
    ):
        self.process = subprocess.Popen(
            argv or ["cargo", "run", "-p", "sample_platformer", "--example", "remote_stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self._next_id = 1
        self.timeout = timeout
        self._closed = False
        self._pending = bytearray()

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        if self._closed:
            raise AgentError("stdio client is closed")
        if self.process.stdin is None or self.process.stdout is None:
            raise AgentError("stdio process is not available")
        if self.process.poll() is not None:
            raise AgentError(
                f"stdio process exited with returncode {self.process.returncode} before request"
            )
        request_id = self._next_id
        request = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params or {},
        }
        self._next_id += 1
        try:
            self.process.stdin.write(json.dumps(request) + "\n")
            self.process.stdin.flush()
        except (BrokenPipeError, OSError) as error:
            raise AgentError(
                f"stdio process unavailable (returncode {self.process.poll()}): {error}"
            ) from error
        line = self._readline_with_timeout()
        try:
            message = json.loads(line)
        except json.JSONDecodeError as error:
            raise AgentError(f"invalid JSON response: {error}") from error
        try:
            return validate_envelope(message, request_id)
        except (AgentError, KeyError, TypeError) as error:
            if isinstance(error, AgentError):
                raise
            raise AgentError(f"invalid JSON-RPC envelope: {error}") from error

    def _readline_with_timeout(self) -> str:
        stdout = self.process.stdout
        assert stdout is not None
        # Prefer select() on POSIX for a true read timeout with an absolute
        # deadline covering the full line. Reads incrementally (os.read byte
        # chunks) so a partial single-byte write followed by a hang still
        # hits the deadline instead of blocking forever in readline().
        # Falls back to a reader thread on platforms without selectable pipes
        # (e.g. Windows).
        try:
            fileno = stdout.fileno()
        except Exception:
            fileno = -1
        if fileno >= 0:
            try:
                deadline = time.monotonic() + self.timeout
                buf = bytearray(getattr(self, "_pending", b""))
                # Fast path: a previous over-read already buffered a full line.
                if b"\n" in buf:
                    newline_idx = buf.index(b"\n") + 1
                    line_bytes = bytes(buf[:newline_idx])
                    self._pending = bytearray(buf[newline_idx:])
                    try:
                        return line_bytes.decode("utf-8")
                    except UnicodeDecodeError as error:
                        raise AgentError(
                            f"invalid UTF-8 in stdio response: {error}"
                        ) from error
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        self._pending = bytearray(buf)
                        raise AgentError(
                            f"stdio read timed out after {self.timeout}s waiting for response"
                        )
                    try:
                        ready, _, _ = select.select([fileno], [], [], remaining)
                    except (OSError, ValueError):
                        fileno = -1
                        break
                    if not ready:
                        self._pending = bytearray(buf)
                        raise AgentError(
                            f"stdio read timed out after {self.timeout}s waiting for response"
                        )
                    try:
                        chunk = os.read(fileno, 4096)
                    except OSError as error:
                        raise AgentError(f"stdio read failed: {error}") from error
                    if not chunk:
                        if buf:
                            self._pending = bytearray()
                            raise AgentError(
                                "stdio process hit EOF before newline "
                                f"(returncode {self.process.poll()})"
                            )
                        raise AgentError(
                            "stdio process hit EOF "
                            f"(returncode {self.process.poll()})"
                        )
                    buf.extend(chunk)
                    if len(buf) > MAX_LINE_BYTES:
                        self._pending = bytearray()
                        raise AgentError(
                            f"stdio line exceeds {MAX_LINE_BYTES} bytes"
                        )
                    if b"\n" in buf:
                        # Return up to and including the first newline to
                        # preserve readline() semantics; stash any over-read
                        # bytes for the next call.
                        newline_idx = buf.index(b"\n") + 1
                        line_bytes = bytes(buf[:newline_idx])
                        self._pending = bytearray(buf[newline_idx:])
                        try:
                            return line_bytes.decode("utf-8")
                        except UnicodeDecodeError as error:
                            raise AgentError(
                                f"invalid UTF-8 in stdio response: {error}"
                            ) from error
                # fileno became unusable; fall through to thread path.
            except AgentError:
                raise
            except Exception:  # pragma: no cover - defensive
                fileno = -1
        result: queue.Queue[str] = queue.Queue()

        def _read() -> None:
            try:
                result.put(stdout.readline())
            except Exception as error:  # pragma: no cover - defensive
                result.put(f"__READ_ERROR__:{error}")

        thread = threading.Thread(target=_read, daemon=True)
        thread.start()
        try:
            line = result.get(timeout=self.timeout)
        except queue.Empty as error:
            raise AgentError(
                f"stdio read timed out after {self.timeout}s waiting for response"
            ) from error
        if line.startswith("__READ_ERROR__:"):
            raise AgentError(f"stdio read failed: {line[len('__READ_ERROR__:'):]}")
        if line == "":
            raise AgentError(
                f"stdio process hit EOF (returncode {self.process.poll()})"
            )
        return line

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        process = self.process
        try:
            if process.stdin is not None:
                try:
                    process.stdin.close()
                except (BrokenPipeError, OSError, ValueError):
                    pass
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        finally:
            try:
                if process.stdout is not None:
                    process.stdout.close()
            except (OSError, ValueError):
                pass

    def __enter__(self) -> StdioAgentClient:
        return self

    def __exit__(self, *exc_info: Any) -> None:
        self.close()

    # Convenience wrappers mirroring AgentClient.

    def info(self) -> Any:
        return self.call("agent.info")

    def reset(
        self, seed: int | None = 0, observation_mode: str = "Hybrid"
    ) -> ResetResponse:
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

    def step(
        self, action: dict[str, Any], observation_mode: str = "Hybrid"
    ) -> StepResponse:
        return self.call(
            "agent.step",
            {"action": action, "observation_mode": observation_mode},
        )

    def step_many(
        self,
        actions: list[dict[str, Any]],
        return_observations: str = "last",
        stop_on_done: bool = True,
    ) -> StepManyResponse:
        return self.call(
            "agent.step_many",
            {
                "actions": actions,
                "return_observations": return_observations,
                "stop_on_done": stop_on_done,
            },
        )

    def fast_forward(self, ticks: int) -> StepResponse:
        return self.call("agent.fast_forward", {"ticks": ticks})

    def observe(self, observation_mode: str = "Hybrid") -> Any:
        return self.call("agent.observe", {"observation_mode": observation_mode})

    def capture(
        self,
        output_dir: str = "screenshots",
        label: str | None = None,
        timeout_frames: int = 8,
        source: Literal["auto", "software", "primary_window"] = "auto",
    ) -> VisualCaptureResponse:
        return self.call(
            "agent.visual.capture",
            {
                "output_dir": output_dir,
                "label": label,
                "timeout_frames": timeout_frames,
                "source": source,
            },
        )

    def snapshot(self) -> Any:
        return self.call("agent.snapshot.create")

    def snapshot_list(self) -> Any:
        return self.call("agent.snapshot.list")

    def snapshot_delete(self, snapshot_id: str) -> Any:
        return self.call("agent.snapshot.delete", {"snapshot_id": snapshot_id})

    def restore(self, snapshot_id: str) -> Any:
        return self.call("agent.snapshot.restore", {"snapshot_id": snapshot_id})

    def restore_tick(self, tick: int) -> Any:
        return self.call("agent.timeline.restore_tick", {"tick": tick})

    def timeline_current(self) -> Any:
        return self.call("agent.timeline.current")

    def branch(self, from_tick: int, label: str | None = None) -> Any:
        return self.call(
            "agent.timeline.branch",
            {"from_tick": from_tick, "label": label},
        )

    def control_set_mode(self, mode: str) -> Any:
        return self.call("agent.control.set_mode", {"mode": mode})

    def control_pause(self) -> Any:
        return self.call("agent.control.pause")

    def control_resume(self) -> Any:
        return self.call("agent.control.resume")

    def replay_start(self) -> Any:
        return self.call("agent.replay.start")

    def replay_stop(self) -> Any:
        return self.call("agent.replay.stop")

    def replay_export(self, path: str | None = None) -> Any:
        return self.call("agent.replay.export", {"path": path})

    def replay_load(self, path: str) -> Any:
        return self.call("agent.replay.load", {"path": path})


if __name__ == "__main__":
    client = AgentClient()
    print(json.dumps(client.info(), indent=2))
    print(json.dumps(client.reset(), indent=2))
    print(json.dumps(client.step({"type": "Move", "x": 1.0, "y": 0.0}), indent=2))

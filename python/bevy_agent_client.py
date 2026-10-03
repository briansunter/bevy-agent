"""Small stdlib client for bevy_agent_control JSON-RPC environments."""

from __future__ import annotations

import json
import io
import http.client
import math
import os
import queue
import select
import socket
import subprocess
import threading
import time
import urllib.request
from dataclasses import dataclass, field
from http.client import HTTPException
from typing import Any, Literal, TypedDict
from urllib.error import HTTPError, URLError


# Allow the server's 30-second request deadline and bounded timeout reply grace.
HTTP_TIMEOUT_SECONDS = 35.0
STDIO_TIMEOUT_SECONDS = 35.0
# The remote protocol bounds each complete message at 8 MiB.
MAX_LINE_BYTES = 8 * 1024 * 1024
MAX_RESPONSE_BYTES = 8 * 1024 * 1024
MAX_HTTP_ERROR_BYTES = 64 * 1024


class AgentError(RuntimeError):
    """An agent request failed at the transport, protocol, or server boundary."""


class RemoteError(AgentError):
    """A valid server error leaves the transport usable for the next call."""

    def __init__(self, error: dict[str, Any]):
        self.code = error["code"]
        self.data = error.get("data")
        super().__init__(f"JSON-RPC error {self.code}: {error['message']}")


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
        raise AgentError(
            f"invalid JSON-RPC envelope: expected object, got {type(message).__name__}"
        )
    version = message.get("jsonrpc")
    if version != "2.0":
        detail = repr(version) if isinstance(version, str) else type(version).__name__
        raise AgentError(f"invalid JSON-RPC version: {detail}")
    response_id = message.get("id")
    if type(response_id) is not int:
        raise AgentError(
            f"JSON-RPC id mismatch: expected integer {expected_id}, "
            f"got {type(response_id).__name__}"
        )
    if response_id != expected_id:
        raise AgentError(
            f"JSON-RPC id mismatch: expected {expected_id}, got {response_id}"
        )
    has_result, has_error = "result" in message, "error" in message
    if has_result and has_error:
        raise AgentError("invalid JSON-RPC envelope: contains both result and error")
    if has_error:
        error = message["error"]
        if (
            not isinstance(error, dict)
            or type(error.get("code")) is not int
            or not isinstance(error.get("message"), str)
        ):
            raise AgentError("invalid JSON-RPC envelope: malformed error object")
        raise RemoteError(error)
    if not has_result:
        raise AgentError("invalid JSON-RPC envelope: missing result")
    return message["result"]


def _validate_timeout(timeout: float) -> None:
    try:
        valid = not isinstance(timeout, bool) and math.isfinite(timeout) and timeout > 0
    except TypeError:
        valid = False
    if not valid:
        raise ValueError("timeout must be a finite positive number")


def _encode_request(
    request_id: int,
    method: str,
    params: dict[str, Any] | None,
    token: str | None,
    retry_key: str | None = None,
) -> str:
    # Copy caller-owned params before adding transport authentication.
    try:
        request_params = dict(params or {})
        if token is not None:
            request_params["session_token"] = token
        request = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": request_params,
        }
        if retry_key is not None:
            allowed = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.:"
            if (
                not isinstance(retry_key, str)
                or not 1 <= len(retry_key) <= 128
                or any(c not in allowed for c in retry_key)
            ):
                raise ValueError("retry_key must contain 1..128 ASCII letters, digits, or -_.:")
            request["retry_key"] = retry_key
        encoded = json.dumps(request, allow_nan=False, ensure_ascii=False)
        if len(encoded.encode("utf-8")) > MAX_LINE_BYTES:
            raise ValueError(f"request exceeds message limit of {MAX_LINE_BYTES} bytes")
        return encoded
    except (TypeError, ValueError, UnicodeError, RecursionError) as error:
        raise AgentError(f"invalid JSON request: {error}") from error


def _reject_nonfinite(value: str) -> Any:
    raise ValueError(f"non-finite JSON number {value}")


def _decode_response(raw: bytes | str, expected_id: int) -> Any:
    try:
        if isinstance(raw, bytes):
            raw = raw.decode("utf-8")
        message = json.loads(raw, parse_constant=_reject_nonfinite)
    except (UnicodeError, ValueError, RecursionError) as error:
        raise AgentError(f"invalid JSON response: {error}") from error
    return validate_envelope(message, expected_id)


def _remaining_http(deadline: float) -> float:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("HTTP call exceeded its total timeout")
    return remaining


_resolver_requests: queue.Queue = queue.Queue(maxsize=16)
_resolver_start_lock = threading.Lock()
_resolver_workers = 0


def _resolve_http_worker() -> None:
    while True:
        deadline, address, result = _resolver_requests.get()
        try:
            if time.monotonic() >= deadline:
                continue
            try:
                value = socket.getaddrinfo(address[0], address[1], 0, socket.SOCK_STREAM)
            except Exception as error:
                result.put((False, error))
            else:
                result.put((True, value))
        finally:
            _resolver_requests.task_done()


def _resolve_http(address: Any, deadline: float) -> Any:
    # The OS resolver cannot be interrupted. A fixed daemon pool bounds
    # outstanding lookups without tying the caller's deadline or process exit
    # to a blocked resolver; expired queued requests never start a lookup.
    global _resolver_workers
    if _resolver_workers < 4:
        if not _resolver_start_lock.acquire(timeout=_remaining_http(deadline)):
            raise TimeoutError("HTTP call timed out waiting for DNS resolution")
        try:
            for index in range(_resolver_workers, 4):
                threading.Thread(
                    target=_resolve_http_worker,
                    name=f"bevy-agent-dns-{index}",
                    daemon=True,
                ).start()
                _resolver_workers += 1
        except RuntimeError as error:
            raise AgentError(f"starting HTTP DNS resolver failed: {error}") from error
        finally:
            _resolver_start_lock.release()
    result: queue.Queue = queue.Queue(maxsize=1)
    try:
        _resolver_requests.put(
            (deadline, address, result), timeout=_remaining_http(deadline)
        )
        success, value = result.get(timeout=_remaining_http(deadline))
    except (queue.Full, queue.Empty) as error:
        raise TimeoutError("HTTP call timed out waiting for DNS resolution") from error
    _remaining_http(deadline)
    if not success:
        raise value
    return value


class _DeadlineReader(io.RawIOBase):
    def __init__(self, owner: "_DeadlineSocket"):
        super().__init__()
        self.owner = owner
        owner.readers += 1

    def readable(self) -> bool:
        return True

    def readinto(self, buffer: Any) -> int:
        return self.owner.recv_into(buffer)

    def close(self) -> None:
        if not self.closed:
            super().close()
            self.owner.readers -= 1
            self.owner.release()


class _DeadlineSocket:
    """Apply an absolute deadline to each raw operation, including header reads."""
    def __init__(self, stream: socket.socket, deadline: float):
        self.stream = stream
        self.deadline = deadline
        self.readers = 0
        self.closing = False

    def __getattr__(self, name: str) -> Any:
        return getattr(self.stream, name)

    def gettimeout(self) -> float:
        return _remaining_http(self.deadline)

    def recv_into(self, buffer: Any) -> int:
        self.stream.settimeout(_remaining_http(self.deadline))
        count = self.stream.recv_into(buffer)
        _remaining_http(self.deadline)
        return count

    def sendall(self, data: Any) -> None:
        view = memoryview(data)
        while view:
            self.stream.settimeout(_remaining_http(self.deadline))
            count = self.stream.send(view)
            _remaining_http(self.deadline)
            if count == 0:
                raise OSError("HTTP connection closed during request")
            view = view[count:]

    def makefile(self, mode: str) -> io.BufferedReader:
        if mode != "rb":
            raise ValueError("HTTP response requires binary reads")
        return io.BufferedReader(_DeadlineReader(self))

    def close(self) -> None:
        self.closing = True
        self.release()

    def release(self) -> None:
        # urllib closes its connection after headers; the response still owns
        # the reader until its body is consumed or its context exits.
        if self.closing and not self.readers:
            self.stream.close()


class _DeadlineConnection:
    def __init__(self, *args: Any, deadline: float, **kwargs: Any):
        self.deadline = deadline
        kwargs["timeout"] = _remaining_http(deadline)
        super().__init__(*args, **kwargs)
        self._create_connection = self._connect_deadline

    def _connect_deadline(
        self, address: Any, timeout: Any = None, source_address: Any = None
    ) -> socket.socket:
        addresses = _resolve_http(address, self.deadline)
        _remaining_http(self.deadline)
        last_error = None
        for family, kind, protocol, _, target in addresses:
            _remaining_http(self.deadline)
            stream = socket.socket(family, kind, protocol)
            try:
                stream.settimeout(_remaining_http(self.deadline))
                if source_address is not None:
                    stream.bind(source_address)
                stream.connect(target)
                stream.settimeout(_remaining_http(self.deadline))
                return stream
            except OSError as error:
                stream.close()
                last_error = error
        _remaining_http(self.deadline)
        raise last_error or OSError("HTTP host resolved to no addresses")

    def _tunnel(self) -> None:
        self.sock = _DeadlineSocket(self.sock, self.deadline)
        super()._tunnel()

    def connect(self) -> None:
        self.timeout = _remaining_http(self.deadline)
        super().connect()
        _remaining_http(self.deadline)
        if not isinstance(self.sock, _DeadlineSocket):
            self.sock = _DeadlineSocket(self.sock, self.deadline)


class _DeadlineHTTPConnection(_DeadlineConnection, http.client.HTTPConnection):
    pass


class _DeadlineHTTPSConnection(_DeadlineConnection, http.client.HTTPSConnection):
    pass


def _open_http(
    request: urllib.request.Request, *, timeout: float, deadline: float | None = None
) -> Any:
    if deadline is None:
        deadline = time.monotonic() + timeout

    class HTTPHandler(urllib.request.HTTPHandler):
        def http_open(self, req: Any) -> Any:
            return self.do_open(_DeadlineHTTPConnection, req, deadline=deadline)

    class HTTPSHandler(urllib.request.HTTPSHandler):
        def https_open(self, req: Any) -> Any:
            return self.do_open(
                _DeadlineHTTPSConnection, req, deadline=deadline, context=self._context
            )

    return urllib.request.build_opener(HTTPHandler(), HTTPSHandler()).open(
        request, timeout=_remaining_http(deadline)
    )


class _AgentCommands:
    """Transport-independent command facade shared by the HTTP and stdio clients."""

    def call(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        *,
        retry_key: str | None = None,
    ) -> Any:
        raise NotImplementedError

    def info(self) -> Any:
        return self.call("agent.info")

    def schema(self) -> Any:
        return self.call("agent.schema")

    def action_space(self) -> Any:
        return self.call("agent.action_space")

    def observation_space(self) -> Any:
        return self.call("agent.observation_space")

    def reset(
        self,
        seed: int | None = 0,
        observation_mode: str = "Hybrid",
        *,
        retry_key: str | None = None,
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
            **({"retry_key": retry_key} if retry_key is not None else {}),
        )

    def step(
        self,
        action: dict[str, Any],
        observation_mode: str | None = None,
        *,
        retry_key: str | None = None,
    ) -> StepResponse:
        return self.call(
            "agent.step",
            {"action": action, "observation_mode": observation_mode},
            **({"retry_key": retry_key} if retry_key is not None else {}),
        )

    def step_many(
        self,
        actions: list[dict[str, Any]],
        return_observations: str = "last",
        *,
        retry_key: str | None = None,
    ) -> StepManyResponse:
        return self.call(
            "agent.step_many",
            {
                "actions": actions,
                "return_observations": return_observations,
            },
            **({"retry_key": retry_key} if retry_key is not None else {}),
        )

    def fast_forward(self, ticks: int) -> StepResponse:
        return self.call("agent.fast_forward", {"ticks": ticks})

    def observe(self, observation_mode: str | None = None) -> Any:
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

    def operation_status(
        self, operation_id: str | None = None, *, retry_key: str | None = None
    ) -> Any:
        """Recover a retained outcome using an operation ID or client retry key."""
        if (operation_id is None) == (retry_key is None):
            raise AgentError("supply exactly one operation_id or retry_key")
        name, value = (
            ("operation_id", operation_id)
            if operation_id is not None
            else ("retry_key", retry_key)
        )
        if not isinstance(value, str) or not value:
            raise AgentError(f"{name} must be a nonempty opaque string")
        return self.call("agent.operations.status", {name: value})

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


@dataclass
class AgentClient(_AgentCommands):
    """HTTP client with bounded responses and independent request correlation."""

    url: str = "http://127.0.0.1:4000/rpc"
    token: str | None = field(default=None, repr=False)
    timeout: float = HTTP_TIMEOUT_SECONDS
    _next_id: int = field(default=1, init=False, repr=False, compare=False)
    _id_lock: Any = field(
        default_factory=threading.Lock, init=False, repr=False, compare=False
    )

    def __post_init__(self) -> None:
        _validate_timeout(self.timeout)

    def call(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        *,
        retry_key: str | None = None,
    ) -> Any:
        deadline = time.monotonic() + self.timeout
        if not self._id_lock.acquire(timeout=self.timeout):
            raise AgentError("HTTP call timed out waiting for request identifier")
        try:
            request_id = self._next_id
            self._next_id += 1
        finally:
            self._id_lock.release()
        try:
            payload = _encode_request(request_id, method, params, self.token, retry_key).encode("utf-8")
        except UnicodeError as error:
            raise AgentError(f"invalid JSON request: {error}") from error
        try:
            http_request = urllib.request.Request(
                self.url,
                data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with _open_http(http_request, timeout=self.timeout, deadline=deadline) as response:
                raw = response.read(MAX_RESPONSE_BYTES + 1)
                _remaining_http(deadline)
        except HTTPError as error:
            try:
                raw_body = error.read(MAX_HTTP_ERROR_BYTES + 1)
                body = raw_body[:MAX_HTTP_ERROR_BYTES].decode(
                    "utf-8", errors="replace"
                ).strip()
                if len(raw_body) > MAX_HTTP_ERROR_BYTES:
                    body += " [truncated]"
            except (OSError, ValueError, HTTPException):
                body = ""
            finally:
                error.close()
            detail = f": {body}" if body else ""
            raise AgentError(f"HTTP {error.code}{detail}") from error
        except (URLError, TimeoutError, socket.timeout) as error:
            reason = getattr(error, "reason", error)
            raise AgentError(f"request failed: {reason}") from error
        except (OSError, ValueError, HTTPException) as error:
            raise AgentError(f"request failed: {error}") from error
        if len(raw) > MAX_RESPONSE_BYTES:
            raise AgentError(f"HTTP response exceeds {MAX_RESPONSE_BYTES} bytes")
        result = _decode_response(raw, request_id)
        try:
            _remaining_http(deadline)
        except TimeoutError as error:
            raise AgentError(f"request failed: {error}") from error
        return result


class StdioAgentClient(_AgentCommands):
    """Own a child process and serialize JSON-RPC request/response pairs.

    Use as a context manager to reap the child. After a transport or malformed
    response failure, create a new client because response alignment is lost.
    """

    def __init__(
        self,
        argv: list[str] | None = None,
        timeout: float = STDIO_TIMEOUT_SECONDS,
        *,
        token: str | None = None,
    ):
        _validate_timeout(timeout)
        try:
            self.process = subprocess.Popen(
                argv or [
                    "cargo", "run", "-p", "sample_platformer", "--example", "remote_stdio"
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                bufsize=1,
            )
        except OSError as error:
            raise AgentError(f"unable to start stdio process: {error}") from error
        self._next_id = 1
        self.timeout = timeout
        self.token = token
        self._closed = False
        self._pending = bytearray()
        self._failure: str | None = None
        self._call_lock = threading.Lock()
        self._reader_queue: queue.Queue[tuple[str | None, Exception | None]] | None = None
        self._reader_thread: threading.Thread | None = None
        self._writer_thread: threading.Thread | None = None

    def call(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        *,
        retry_key: str | None = None,
    ) -> Any:
        # A pipe has no response routing: serialize complete request/response pairs.
        deadline = time.monotonic() + self.timeout
        if not self._call_lock.acquire(timeout=self.timeout):
            raise AgentError(
                f"stdio call timed out after {self.timeout}s waiting for another request"
            )
        try:
            return self._call(method, params, deadline, retry_key)
        finally:
            self._call_lock.release()

    def _call(
        self,
        method: str,
        params: dict[str, Any] | None,
        deadline: float,
        retry_key: str | None = None,
    ) -> Any:
        if self._closed:
            raise AgentError("stdio client is closed")
        if self._failure is not None:
            raise AgentError(
                f"stdio client is unusable after previous failure: {self._failure}"
            )
        if self.process.stdin is None or self.process.stdout is None:
            raise AgentError("stdio process is not available")
        if self.process.poll() is not None:
            raise AgentError(
                f"stdio process exited with returncode {self.process.returncode} before request"
            )
        request_id = self._next_id
        # Encode before writing so bad caller input does not invalidate the stream.
        payload = _encode_request(request_id, method, params, self.token, retry_key) + "\n"
        try:
            payload_size = len(payload.encode("utf-8"))
        except UnicodeError as error:
            raise AgentError(f"invalid JSON request: {error}") from error
        if payload_size > MAX_LINE_BYTES:
            raise AgentError(f"stdio request exceeds {MAX_LINE_BYTES} bytes")
        self._remaining_timeout(deadline, "call")
        self._next_id += 1
        try:
            self._write_request(payload, deadline)
            result = _decode_response(self._readline_with_timeout(deadline), request_id)
            self._remaining_timeout(deadline, "call")
            return result
        except RemoteError:
            # A well-formed error is a response, so the next request stays aligned.
            raise
        except AgentError as error:
            # A timeout or malformed response cannot safely be retried on this pipe.
            self._failure = str(error)
            raise

    def _remaining_timeout(self, deadline: float, operation: str) -> float:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AgentError(f"stdio {operation} timed out after {self.timeout}s")
        return remaining

    def _write_request(self, payload: str, deadline: float) -> None:
        stdin = self.process.stdin
        assert stdin is not None
        try:
            fileno = stdin.fileno()
        except (AttributeError, OSError, ValueError):
            fileno = -1
        has_fd = isinstance(fileno, int) and fileno >= 0
        data = memoryview(payload.encode("utf-8"))
        if os.name == "posix" and has_fd:
            # A writable pipe can still block on a large write. Nonblocking
            # writes and select share the same deadline as response reading.
            try:
                blocking = os.get_blocking(fileno)
            except OSError as error:
                raise AgentError(f"stdio write failed: {error}") from error
            try:
                os.set_blocking(fileno, False)
                while data:
                    remaining = self._remaining_timeout(deadline, "write")
                    _, ready, _ = select.select([], [fileno], [], remaining)
                    if not ready:
                        raise AgentError(f"stdio write timed out after {self.timeout}s")
                    try:
                        written = os.write(fileno, data)
                    except BlockingIOError:
                        continue
                    if written == 0:
                        raise AgentError("stdio process closed input before request completed")
                    data = data[written:]
            except (OSError, ValueError) as error:
                raise AgentError(f"stdio write failed: {error}") from error
            finally:
                try:
                    os.set_blocking(fileno, blocking)
                except OSError:
                    pass  # Concurrent close() may already have closed the pipe.
            return

        # Non-selectable pipes use a daemon writer. Timeout invalidates this
        # stream; close() reaps the child before closing pipes so an abandoned
        # writer cannot hold a TextIO lock and prevent process termination.
        result: queue.Queue[Exception | None] = queue.Queue(maxsize=1)

        def write_request() -> None:
            try:
                if has_fd:
                    remaining_data = data
                    while remaining_data and not self._closed:
                        written = os.write(fileno, remaining_data)
                        if written == 0:
                            raise OSError("stdio process closed input")
                        remaining_data = remaining_data[written:]
                else:
                    stdin.write(payload)
                    stdin.flush()
            except Exception as error:
                result.put(error)
            else:
                result.put(None)

        self._writer_thread = threading.Thread(target=write_request, daemon=True)
        self._writer_thread.start()
        try:
            error = result.get(timeout=self._remaining_timeout(deadline, "write"))
        except queue.Empty as error:
            raise AgentError(f"stdio write timed out after {self.timeout}s") from error
        if error is not None:
            raise AgentError(f"stdio write failed: {error}") from error

    def _take_pending_line(self) -> str | None:
        newline = self._pending.find(b"\n")
        length = newline + 1 if newline >= 0 else len(self._pending)
        if length > MAX_LINE_BYTES:
            self._pending.clear()
            raise AgentError(f"stdio line exceeds {MAX_LINE_BYTES} bytes")
        if newline < 0:
            return None
        line = bytes(self._pending[:length])
        del self._pending[:length]
        try:
            return line.decode("utf-8")
        except UnicodeError as error:
            raise AgentError(f"invalid UTF-8 in stdio response: {error}") from error

    def _readline_with_timeout(self, deadline: float | None = None) -> str:
        stdout = self.process.stdout
        assert stdout is not None
        if deadline is None:
            deadline = time.monotonic() + self.timeout
        self._remaining_timeout(deadline, "read")
        try:
            fileno = stdout.fileno()
        except (AttributeError, OSError, ValueError):
            fileno = -1
        if os.name == "posix" and isinstance(fileno, int) and fileno >= 0:
            # Read bytes without TextIO buffering. One absolute deadline covers
            # all chunks, including a partial line followed by a stalled child.
            while True:
                line = self._take_pending_line()
                if line is not None:
                    return line
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AgentError(
                        f"stdio read timed out after {self.timeout}s waiting for response"
                    )
                try:
                    ready, _, _ = select.select([fileno], [], [], remaining)
                    if not ready:
                        raise AgentError(
                            f"stdio read timed out after {self.timeout}s waiting for response"
                        )
                    chunk = os.read(fileno, 4096)
                except (OSError, ValueError) as error:
                    raise AgentError(f"stdio read failed: {error}") from error
                if not chunk:
                    detail = " before newline" if self._pending else ""
                    raise AgentError(
                        f"stdio process hit EOF{detail} (returncode {self.process.poll()})"
                    )
                self._pending.extend(chunk)
        return self._readline_from_thread(stdout, deadline)

    def _readline_from_thread(self, stdout: Any, deadline: float) -> str:
        # Windows pipes cannot be selected. Keep one bounded reader for the
        # lifetime of the client; per-call readers can steal subsequent replies
        # after a timeout. The failed client rejects further calls in either path.
        if self._reader_queue is None:
            results: queue.Queue[tuple[str | None, Exception | None]] = queue.Queue(
                maxsize=1
            )
            self._reader_queue = results

            def read_lines() -> None:
                while not self._closed:
                    try:
                        line = stdout.readline(MAX_LINE_BYTES + 1)
                        item = (line, None)
                    except Exception as error:
                        line = None
                        item = (None, error)
                    while not self._closed:
                        try:
                            results.put(item, timeout=0.1)
                            break
                        except queue.Full:
                            continue
                    if line is None or not line.endswith("\n"):
                        return

            self._reader_thread = threading.Thread(target=read_lines, daemon=True)
            self._reader_thread.start()
        try:
            line, read_error = self._reader_queue.get(
                timeout=self._remaining_timeout(deadline, "read")
            )
        except queue.Empty as error:
            raise AgentError(
                f"stdio read timed out after {self.timeout}s waiting for response"
            ) from error
        if read_error is not None:
            raise AgentError(f"stdio read failed: {read_error}") from read_error
        assert line is not None
        if not line:
            raise AgentError(f"stdio process hit EOF (returncode {self.process.poll()})")
        try:
            line_size = len(line.encode("utf-8"))
        except UnicodeError as error:
            raise AgentError(f"invalid UTF-8 in stdio response: {error}") from error
        if line_size > MAX_LINE_BYTES:
            raise AgentError(f"stdio line exceeds {MAX_LINE_BYTES} bytes")
        if not line.endswith("\n"):
            raise AgentError(
                f"stdio process hit EOF before newline (returncode {self.process.poll()})"
            )
        return line

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        process = self.process
        try:
            # Terminate before closing stdin: a timed-out fallback writer can
            # hold the text stream lock until the child's pipe reader exits.
            if process.poll() is None:
                try:
                    process.terminate()
                except ProcessLookupError:
                    pass  # The child can exit between poll() and terminate().
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    try:
                        process.kill()
                    except ProcessLookupError:
                        pass
                    process.wait(timeout=5)
        finally:
            if self._writer_thread is not None:
                self._writer_thread.join(timeout=0.2)
            if process.stdin is not None:
                try:
                    process.stdin.close()
                except (OSError, ValueError):
                    pass
            try:
                if process.stdout is not None:
                    process.stdout.close()
            except (OSError, ValueError):
                pass

    def __enter__(self) -> StdioAgentClient:
        return self

    def __exit__(self, *exc_info: Any) -> None:
        self.close()


if __name__ == "__main__":
    client = AgentClient()
    print(json.dumps(client.info(), indent=2))
    print(json.dumps(client.reset(), indent=2))
    print(json.dumps(client.step({"type": "Move", "x": 1.0, "y": 0.0}), indent=2))

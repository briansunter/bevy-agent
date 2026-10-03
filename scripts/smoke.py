"""Exercise built examples and clients through real HTTP, WebSocket, and stdio.

Build with cargo build --workspace --bins --examples --locked, then run this
script. Each run owns a fresh server, child process, and temporary artifact root.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "python"))
from bevy_agent_client import AgentClient, AgentError, StdioAgentClient  # noqa: E402


def exact(stream: socket.socket, size: int) -> bytes:
    data = bytearray()
    while len(data) < size:
        chunk = stream.recv(size - len(data))
        if not chunk:
            raise AssertionError("WebSocket closed before a complete response")
        data.extend(chunk)
    return bytes(data)


def websocket_discovery(port: int, token: str | None) -> None:
    params = {} if token is None else {"session_token": token}
    body = json.dumps({"jsonrpc": "2.0", "id": 7, "method": "agent.info", "params": params}).encode()
    mask = b"\x01\x02\x03\x04"
    length = bytes([0x80 | len(body)]) if len(body) < 126 else b"\xfe" + struct.pack("!H", len(body))
    frame = b"\x81" + length + mask + bytes(value ^ mask[index % 4] for index, value in enumerate(body))
    handshake = (
        f"GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
        "Sec-WebSocket-Version: 13\r\n\r\n"
    ).encode()
    with socket.create_connection(("127.0.0.1", port), timeout=5) as stream:
        # Coalesce handshake and first frame to exercise the upgrade boundary.
        stream.sendall(handshake + frame)
        headers = bytearray()
        while not headers.endswith(b"\r\n\r\n"):
            assert len(headers) < 32 * 1024, "oversized upgrade response"
            headers.extend(exact(stream, 1))
        assert headers.startswith(b"HTTP/1.1 101 "), "upgrade failed"
        header = exact(stream, 2)
        assert header[0] == 0x81 and header[1] & 0x80 == 0, "invalid server frame"
        size = header[1] & 0x7F
        if size == 126:
            size = struct.unpack("!H", exact(stream, 2))[0]
        elif size == 127:
            size = struct.unpack("!Q", exact(stream, 8))[0]
        assert size <= 8 * 1024 * 1024, "oversized WebSocket response"
        response = json.loads(exact(stream, size))
        assert response["jsonrpc"] == "2.0" and response["id"] == 7
        assert response["result"]["name"] == "sample_platformer"
        # An idle persistent connection must leave other workers available.
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as health:
            assert json.load(health)["ok"] is True
        assert AgentClient(f"http://127.0.0.1:{port}/rpc", token=token, timeout=2).info()["name"] == "sample_platformer"
        stream.sendall(b"\x88\x80" + mask)


def wait_ready(server: subprocess.Popen, port: int) -> None:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise RuntimeError(f"HTTP example exited with status {server.returncode}")
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1) as response:
                assert json.load(response)["ok"] is True
            return
        except OSError:
            time.sleep(0.05)
    raise TimeoutError("HTTP example did not become ready")


def http_contracts(client: AgentClient, artifacts: Path) -> None:
    assert client.info()["name"] == "sample_platformer"
    assert client.schema() and client.action_space() and client.observation_space()
    assert client.observation_space()["modes"] == ["PlayerKnowledge", "Hybrid"]
    before = client.timeline_current()
    for method, params in [
        ("agent.fast_forward", {"ticks": 0}),
        ("agent.step_many", {"actions": [{"type": "Move", "x": 1.0, "y": 0.0}, {"type": "Attack"}]}),
        ("agent.step_many", {"actions": [], "return_observations": "unknown"}),
    ]:
        try:
            client.call(method, params)
        except AgentError:
            pass
        else:
            raise AssertionError(f"{method} accepted invalid input")
        assert client.timeline_current() == before, "rejected request mutated timeline"

    initial = client.reset(seed=42)
    first_action = {"type": "Move", "x": 1.0, "y": 0.0}
    moved = client.step(first_action, retry_key="smoke-episode.tick1")
    assert client.step(first_action, retry_key="smoke-episode.tick1") == moved, "retry repeated a mutation"
    status = client.operation_status(retry_key="smoke-episode.tick1")
    assert status["state"] == "completed" and status["response"]["result"] == moved
    assert client.timeline_current()["tick"] == 1
    assert initial["tick"] == 0 and moved["tick"] == 1
    assert moved["observation"]["symbolic"]["player"]["position"][0] > initial["observation"]["symbolic"]["player"]["position"][0]
    snapshot = client.snapshot()
    action = {"type": "Move", "x": -1.0, "y": 0.0}
    expected = client.step(action)
    client.restore(snapshot["snapshot_id"])
    assert client.step(action)["checksum"] == expected["checksum"], "restore diverged"
    branch = client.branch(from_tick=1, label="smoke")
    client.step({"type": "Jump"})
    capture = client.capture(output_dir="screenshots", label="smoke", source="software")
    image = Path(capture["path"])
    assert image.resolve().is_relative_to(artifacts.resolve())
    assert image.read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
    assert (capture["width"], capture["height"]) == (640, 360)
    exported = client.replay_export()
    bundle = exported["bundle"]
    assert bundle["format_version"] == 3
    assert bundle["log"]["manifest"]["schema_version"] == 3
    assert all(snapshot["manifest"]["schema_version"] == 3 for snapshot in bundle["snapshots"])
    client.replay_export("smoke.json")
    client.reset(seed=999)
    client.replay_load("smoke.json")
    timeline = client.timeline_current()
    assert timeline["tick"] == 2 and timeline["branch_id"] == branch["branch_id"]
    client.restore_tick(1)
    assert client.timeline_current()["tick"] == 1


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target" / "debug")
    args = parser.parse_args()
    bins = args.bin_dir.resolve()
    executables = [bins / "agentctl", bins / "examples" / "remote_http", bins / "examples" / "remote_stdio"]
    for executable in executables:
        if not executable.is_file():
            raise FileNotFoundError(f"build the workspace binaries/examples first: {executable}")
    token = os.environ.get("AGENT_TOKEN")
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix="bevy-agent-smoke-") as directory:
        artifacts = Path(directory)
        with (artifacts / "server.log").open("wb") as log:
            server = subprocess.Popen(
                [str(executables[1]), f"127.0.0.1:{port}", "--artifact-dir", str(artifacts)],
                stdout=subprocess.DEVNULL, stderr=log,
            )
            try:
                wait_ready(server, port)
                url = f"http://127.0.0.1:{port}/rpc"
                client = AgentClient(url, token=token, timeout=5)
                http_contracts(client, artifacts)
                cli = subprocess.run([str(executables[0]), "--url", url, "info"], check=True, capture_output=True, text=True, timeout=10)
                assert json.loads(cli.stdout)["result"]["name"] == "sample_platformer"
                before_cli = client.timeline_current()["tick"]
                command = [str(executables[0]), "--url", url, "--retry-key", "cli-smoke.step-1", "step", '{"type":"Noop"}']
                first = json.loads(subprocess.run(command, check=True, capture_output=True, text=True, timeout=10).stdout)
                repeated = json.loads(subprocess.run(command, check=True, capture_output=True, text=True, timeout=10).stdout)
                assert first == repeated and client.timeline_current()["tick"] == before_cli + 1
                lookup = [str(executables[0]), "--url", url, "operation-status", "--key", "cli-smoke.step-1"]
                status = json.loads(subprocess.run(lookup, check=True, capture_output=True, text=True, timeout=10).stdout)["result"]
                assert status["state"] == "completed" and status["response"]["result"] == first["result"]
                websocket_discovery(port, token)
            finally:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=5)
        with StdioAgentClient([str(executables[2])], timeout=5) as client:
            assert client.info()["name"] == "sample_platformer"
            assert client.schema() and client.action_space() and client.observation_space()
            assert client.reset(seed=42)["tick"] == 0
            try:
                client.step({"type": "Noop"}, retry_key="stdio-unsupported")
            except AgentError as error:
                assert "service ledger" in str(error)
            else:
                raise AssertionError("stdio accepted a retry key without a ledger")
            assert client.info()["tick"] == 0
            assert client.step({"type": "Noop"})["tick"] == 1
    print("HTTP, CLI, WebSocket, stdio, idempotent retries, snapshot/restore, branch/replay, and PNG smoke checks passed")


if __name__ == "__main__":
    main()

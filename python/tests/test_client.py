"""Unit tests for bevy_agent_client using mocked transports."""

from __future__ import annotations

import io
import json
import socket
import sys
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch
from urllib.error import HTTPError, URLError

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from bevy_agent_client import (
    AgentClient,
    AgentError,
    StdioAgentClient,
    validate_envelope,
)


def _http_response(payload: bytes):
    response = MagicMock()
    response.read.return_value = payload
    response.__enter__.return_value = response
    response.__exit__.return_value = False
    return response


class ValidateEnvelopeTests(unittest.TestCase):
    def test_ok_result_passes_through(self):
        self.assertEqual(validate_envelope({"jsonrpc": "2.0", "id": 3, "result": {"ok": True}}, 3), {"ok": True})

    def test_error_envelope_raises(self):
        with self.assertRaises(AgentError):
            validate_envelope(
                {"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "boom"}}, 1
            )

    def test_id_mismatch_raises(self):
        with self.assertRaises(AgentError):
            validate_envelope({"jsonrpc": "2.0", "id": 2, "result": {}}, 1)

    def test_missing_result_raises(self):
        with self.assertRaises(AgentError):
            validate_envelope({"jsonrpc": "2.0", "id": 1}, 1)

    def test_non_object_raises(self):
        with self.assertRaises(AgentError):
            validate_envelope([1, 2, 3], 1)


class AgentClientErrorMappingTests(unittest.TestCase):
    def _client(self) -> AgentClient:
        return AgentClient(url="http://127.0.0.1:4000/rpc")

    def test_json_error_envelope_maps_to_agent_error(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "bad"}}).encode()
        with patch("bevy_agent_client.urllib.request.urlopen", return_value=_http_response(body)):
            with self.assertRaises(AgentError) as ctx:
                self._client().call("agent.info")
        self.assertIn("bad", str(ctx.exception))

    def test_malformed_json_maps_to_agent_error(self):
        with patch(
            "bevy_agent_client.urllib.request.urlopen",
            return_value=_http_response(b"not json{{"),
        ):
            with self.assertRaises(AgentError):
                self._client().call("agent.info")

    def test_http_error_maps_to_agent_error(self):
        error = HTTPError("http://x/rpc", 500, "boom", {}, io.BytesIO(b"server exploded"))
        with patch("bevy_agent_client.urllib.request.urlopen", side_effect=error):
            with self.assertRaises(AgentError) as ctx:
                self._client().call("agent.info")
        self.assertIn("HTTP 500", str(ctx.exception))

    def test_url_error_maps_to_agent_error(self):
        with patch(
            "bevy_agent_client.urllib.request.urlopen",
            side_effect=URLError("refused"),
        ):
            with self.assertRaises(AgentError):
                self._client().call("agent.info")

    def test_socket_timeout_maps_to_agent_error(self):
        with patch(
            "bevy_agent_client.urllib.request.urlopen",
            side_effect=socket.timeout("timed out"),
        ):
            with self.assertRaises(AgentError):
                self._client().call("agent.info")

    def test_missing_result_key_maps_to_agent_error(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 1}).encode()
        with patch("bevy_agent_client.urllib.request.urlopen", return_value=_http_response(body)):
            with self.assertRaises(AgentError):
                self._client().call("agent.info")

    def test_wrapper_params_include_source_parity(self):
        seen: dict = {}

        def fake_call(method, params=None):
            seen["method"] = method
            seen["params"] = params
            return {"ok": True}

        client = self._client()
        client.call = fake_call  # type: ignore[method-assign]
        client.capture(output_dir="shots", label="l", timeout_frames=3, source="software")
        self.assertEqual(seen["method"], "agent.visual.capture")
        self.assertEqual(seen["params"]["source"], "software")
        client.fast_forward(5)
        self.assertEqual(seen["method"], "agent.fast_forward")
        client.snapshot_list()
        self.assertEqual(seen["method"], "agent.snapshot.list")
        client.snapshot_delete("snap-1")
        self.assertEqual(seen["method"], "agent.snapshot.delete")
        client.timeline_current()
        self.assertEqual(seen["method"], "agent.timeline.current")
        client.control_set_mode("Paused")
        self.assertEqual(seen["method"], "agent.control.set_mode")
        client.replay_start()
        self.assertEqual(seen["method"], "agent.replay.start")
        client.replay_stop()
        self.assertEqual(seen["method"], "agent.replay.stop")


class FakeStdioProcess:
    """Minimal Popen stand-in with controllable stdout lines."""

    def __init__(self, lines: list[str], returncode=None):
        self._lines = list(lines)
        self.returncode = returncode
        self.stdin = MagicMock()
        self.stdout = MagicMock()
        self.terminated = False
        self.killed = False
        self.stdout.fileno.return_value = -1  # force reader-thread path
        self.stdout.readline.side_effect = lambda: self._lines.pop(0) if self._lines else ""
        self.stdout.close = MagicMock()

    def poll(self):
        return self.returncode

    def terminate(self):
        self.terminated = True

    def kill(self):
        self.killed = True

    def wait(self, timeout=None):
        self.returncode = self.returncode if self.returncode is not None else 0
        return self.returncode


class StdioClientTests(unittest.TestCase):
    def _client_with(self, lines: list[str], returncode=None, **kwargs) -> tuple[StdioAgentClient, FakeStdioProcess]:
        with patch("bevy_agent_client.subprocess.Popen") as popen:
            process = FakeStdioProcess(lines, returncode=returncode)
            popen.return_value = process
            client = StdioAgentClient(argv=["fake"], **kwargs)
        # __init__ stored the fake process object directly.
        self.assertIs(client.process, process)
        return client, process

    def test_successful_call_returns_result(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"tick": 7}})
        client, _ = self._client_with([body + "\n"])
        self.assertEqual(client.call("agent.info"), {"tick": 7})
        client.close()

    def test_error_envelope_raises_agent_error(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "nope"}})
        client, _ = self._client_with([body + "\n"])
        with self.assertRaises(AgentError):
            client.call("agent.step", {"action": {"type": "Noop"}})
        client.close()

    def test_eof_raises_structured_agent_error(self):
        client, process = self._client_with([])
        with self.assertRaises(AgentError) as ctx:
            client.call("agent.info")
        self.assertIn("EOF", str(ctx.exception))
        self.assertIn("returncode", str(ctx.exception))
        client.close()

    def test_exited_process_raises_structured_error(self):
        client, process = self._client_with([], returncode=1)
        with self.assertRaises(AgentError) as ctx:
            client.call("agent.info")
        self.assertIn("returncode 1", str(ctx.exception))
        client.close()

    def test_read_timeout_raises_agent_error(self):
        client, _ = self._client_with([])
        client.timeout = 0.05
        # Block readline longer than the timeout.
        client.process.stdout.readline.side_effect = lambda: __import__("time").sleep(5) or "x"
        with self.assertRaises(AgentError) as ctx:
            client.call("agent.info")
        self.assertIn("timed out", str(ctx.exception))
        client.close()

    def test_close_terminates_and_waits_then_cleans_pipes(self):
        client, process = self._client_with([])
        client.close()
        self.assertTrue(process.terminated)
        self.assertTrue(process.stdin.close.called)
        self.assertTrue(process.stdout.close.called)
        # Second close is a no-op.
        client.close()

    def test_context_manager_closes(self):
        with patch("bevy_agent_client.subprocess.Popen") as popen:
            process = FakeStdioProcess([])
            popen.return_value = process
            with StdioAgentClient(argv=["fake"]) as client:
                self.assertIs(client.process, process)
        self.assertTrue(process.stdin.close.called)


if __name__ == "__main__":
    unittest.main()

"""Unit tests for bevy_agent_client using mocked transports."""

from __future__ import annotations

import io
import json
import socket
import subprocess
import threading
import sys
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import MagicMock, patch
from urllib.error import HTTPError, URLError
from http.client import IncompleteRead

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from bevy_agent_client import (
    AgentClient,
    AgentError,
    RemoteError,
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

    def test_numeric_lookalike_ids_are_rejected(self):
        for response_id in [True, 1.0, "1"]:
            with self.subTest(response_id=response_id), self.assertRaises(AgentError):
                validate_envelope({"jsonrpc": "2.0", "id": response_id, "result": {}}, 1)

    def test_mixed_result_and_error_is_rejected(self):
        with self.assertRaisesRegex(AgentError, "both result and error"):
            validate_envelope(
                {"jsonrpc": "2.0", "id": 1, "result": {}, "error": None}, 1
            )

    def test_malformed_error_objects_are_rejected(self):
        for error in [None, {}, {"code": True, "message": "bad"}, {"code": -1},
                      {"code": "-1", "message": "bad"}, {"code": -1, "message": 42}]:
            with self.subTest(error=error), self.assertRaisesRegex(AgentError, "malformed error"):
                validate_envelope({"jsonrpc": "2.0", "id": 1, "error": error}, 1)

    def test_valid_remote_error_keeps_code_and_data(self):
        with self.assertRaises(AgentError) as caught:
            validate_envelope(
                {"jsonrpc": "2.0", "id": 1,
                 "error": {"code": -32602, "message": "bad action", "data": {"field": "x"}}}, 1
            )
        self.assertEqual(caught.exception.code, -32602)
        self.assertEqual(caught.exception.data, {"field": "x"})


class AgentClientErrorMappingTests(unittest.TestCase):
    def _client(self) -> AgentClient:
        return AgentClient(url="http://127.0.0.1:4000/rpc")

    def test_json_error_envelope_maps_to_agent_error(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "bad"}}).encode()
        with patch("bevy_agent_client._open_http", return_value=_http_response(body)):
            with self.assertRaises(AgentError) as ctx:
                self._client().call("agent.info")
        self.assertIn("bad", str(ctx.exception))

    def test_malformed_json_maps_to_agent_error(self):
        with patch(
            "bevy_agent_client._open_http",
            return_value=_http_response(b"not json{{"),
        ):
            with self.assertRaises(AgentError):
                self._client().call("agent.info")

    def test_http_error_maps_to_agent_error(self):
        error = HTTPError("http://x/rpc", 500, "boom", {}, io.BytesIO(b"server exploded"))
        with patch("bevy_agent_client._open_http", side_effect=error):
            with self.assertRaises(AgentError) as ctx:
                self._client().call("agent.info")
        self.assertIn("HTTP 500", str(ctx.exception))

    def test_url_error_maps_to_agent_error(self):
        with patch(
            "bevy_agent_client._open_http",
            side_effect=URLError("refused"),
        ):
            with self.assertRaises(AgentError):
                self._client().call("agent.info")

    def test_socket_timeout_maps_to_agent_error(self):
        with patch(
            "bevy_agent_client._open_http",
            side_effect=socket.timeout("timed out"),
        ):
            with self.assertRaises(AgentError):
                self._client().call("agent.info")

    def test_missing_result_key_maps_to_agent_error(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 1}).encode()
        with patch("bevy_agent_client._open_http", return_value=_http_response(body)):
            with self.assertRaises(AgentError):
                self._client().call("agent.info")

    def test_invalid_utf8_maps_to_agent_error(self):
        with patch("bevy_agent_client._open_http", return_value=_http_response(b"\xff")):
            with self.assertRaisesRegex(AgentError, "invalid JSON response"):
                self._client().call("agent.info")

    def test_oversized_http_response_is_bounded(self):
        response = _http_response(b"x" * 65)
        with patch("bevy_agent_client.MAX_RESPONSE_BYTES", 64), patch(
            "bevy_agent_client._open_http", return_value=response
        ):
            with self.assertRaisesRegex(AgentError, "exceeds 64 bytes"):
                self._client().call("agent.info")
        response.read.assert_called_once_with(65)

    def test_incomplete_http_body_maps_to_agent_error(self):
        response = _http_response(b"")
        response.read.side_effect = IncompleteRead(b"partial", 100)
        with patch("bevy_agent_client._open_http", return_value=response):
            with self.assertRaisesRegex(AgentError, "request failed"):
                self._client().call("agent.info")

    def test_nonfinite_json_response_is_rejected(self):
        response = _http_response(b'{"jsonrpc":"2.0","id":1,"result":NaN}')
        with patch("bevy_agent_client._open_http", return_value=response):
            with self.assertRaisesRegex(AgentError, "non-finite"):
                self._client().call("agent.info")

    def test_deep_json_response_maps_to_agent_error(self):
        body = b'{"jsonrpc":"2.0","id":1,"result":' + b"[" * 10000 + b"0" + b"]" * 10000 + b"}"
        with patch("bevy_agent_client._open_http", return_value=_http_response(body)):
            with self.assertRaisesRegex(AgentError, "invalid JSON response"):
                self._client().info()

    def test_deep_request_maps_to_agent_error_before_transport(self):
        value = []
        for _ in range(10000):
            value = [value]
        with patch("bevy_agent_client._open_http") as transport:
            with self.assertRaisesRegex(AgentError, "invalid JSON request"):
                self._client().call("agent.info", {"nested": value})
        transport.assert_not_called()

    def test_nonfinite_request_is_rejected_before_transport(self):
        with patch("bevy_agent_client._open_http") as transport:
            with self.assertRaisesRegex(AgentError, "invalid JSON request"):
                self._client().step({"type": "Move", "x": float("nan"), "y": 0})
        transport.assert_not_called()

    def test_authentication_does_not_mutate_caller_params(self):
        response = _http_response(b'{"jsonrpc":"2.0","id":1,"result":null}')
        params = {"nested": {"keep": True}}
        with patch("bevy_agent_client._open_http", return_value=response) as transport:
            AgentClient(token="secret").call("agent.info", params)
        request = json.loads(transport.call_args.args[0].data)
        self.assertEqual(request["params"]["session_token"], "secret")
        self.assertEqual(params, {"nested": {"keep": True}})

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
        self.stdout.readline.side_effect = lambda *args: self._lines.pop(0) if self._lines else ""
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
        client.process.stdout.readline.side_effect = lambda *args: __import__("time").sleep(0.2) or "x"
        with self.assertRaises(AgentError) as ctx:
            client.call("agent.info")
        self.assertIn("timed out", str(ctx.exception))
        with self.assertRaisesRegex(AgentError, "unusable after previous failure"):
            client.call("agent.info")
        self.assertEqual(client.process.stdin.write.call_count, 1)
        client.close()

    def test_protocol_failure_invalidates_stream(self):
        for line in ["not json\n", '{"jsonrpc":"2.0","id":2,"result":{}}\n']:
            with self.subTest(line=line):
                client, process = self._client_with([line])
                try:
                    with self.assertRaises(AgentError):
                        client.call("agent.info")
                    with self.assertRaisesRegex(AgentError, "unusable"):
                        client.call("agent.info")
                    self.assertEqual(process.stdin.write.call_count, 1)
                finally:
                    client.close()

    def test_deep_json_response_invalidates_stream(self):
        line = '{"jsonrpc":"2.0","id":1,"result":' + "[" * 10000 + "0" + "]" * 10000 + "}\n"
        client, process = self._client_with([line])
        try:
            with self.assertRaisesRegex(AgentError, "invalid JSON response"):
                client.info()
            with self.assertRaisesRegex(AgentError, "unusable"):
                client.info()
            self.assertEqual(process.stdin.write.call_count, 1)
        finally:
            client.close()

    def test_write_and_read_share_one_deadline(self):
        client, process = self._client_with([], timeout=0.15)
        process.stdin.write.side_effect = lambda payload: time.sleep(0.1)
        process.stdout.readline.side_effect = lambda *args: time.sleep(0.1) or (
            '{"jsonrpc":"2.0","id":1,"result":{}}\n'
        )
        try:
            with self.assertRaisesRegex(AgentError, "timed out"):
                client.info()
            with self.assertRaisesRegex(AgentError, "unusable"):
                client.info()
        finally:
            client.close()

    def test_valid_remote_error_does_not_invalidate_stream(self):
        client, _ = self._client_with([
            '{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad"}}\n',
            '{"jsonrpc":"2.0","id":2,"result":{"tick":7}}\n',
        ])
        try:
            with self.assertRaises(AgentError):
                client.call("agent.step")
            self.assertEqual(client.info(), {"tick": 7})
        finally:
            client.close()

    def test_stdio_inherits_schema_and_space_commands(self):
        client, process = self._client_with([
            json.dumps({"jsonrpc": "2.0", "id": request_id, "result": {}}) + "\n"
            for request_id in range(1, 4)
        ])
        try:
            client.schema()
            client.action_space()
            client.observation_space()
            methods = [json.loads(call.args[0])["method"] for call in process.stdin.write.call_args_list]
            self.assertEqual(methods, ["agent.schema", "agent.action_space", "agent.observation_space"])
        finally:
            client.close()

    def test_stdio_authentication_does_not_mutate_params(self):
        client, process = self._client_with([
            '{"jsonrpc":"2.0","id":1,"result":null}\n'
        ], token="secret")
        params = {"value": 3}
        try:
            client.call("agent.info", params)
            request = json.loads(process.stdin.write.call_args.args[0])
            self.assertEqual(request["params"]["session_token"], "secret")
            self.assertEqual(params, {"value": 3})
        finally:
            client.close()

    def test_invalid_caller_input_keeps_stream_usable(self):
        client, process = self._client_with(['{"jsonrpc":"2.0","id":1,"result":{}}\n'])
        try:
            with self.assertRaisesRegex(AgentError, "invalid JSON request"):
                client.step({"x": float("inf")})
            process.stdin.write.assert_not_called()
            self.assertEqual(client.info(), {})
        finally:
            client.close()

    def test_fallback_rejects_partial_eof_and_oversized_unicode(self):
        for line, message in [("{", "before newline"), ("界" * 24 + "\n", "exceeds")]:
            with self.subTest(line=line), patch("bevy_agent_client.MAX_LINE_BYTES", 64):
                client, _ = self._client_with([line])
                try:
                    with self.assertRaisesRegex(AgentError, message):
                        client._readline_with_timeout()
                finally:
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

    def test_close_kills_child_that_ignores_termination(self):
        client, process = self._client_with([])
        with patch.object(process, "wait", side_effect=[subprocess.TimeoutExpired("fake", 2), 0]):
            client.close()
        self.assertTrue(process.terminated)
        self.assertTrue(process.killed)
        self.assertTrue(process.stdout.close.called)

    def test_close_tolerates_child_exit_race(self):
        client, process = self._client_with([])
        with patch.object(process, "terminate", side_effect=ProcessLookupError()):
            client.close()
        self.assertTrue(process.stdin.close.called)
        self.assertTrue(process.stdout.close.called)

    def test_timeout_configuration_is_validated_before_starting_child(self):
        for timeout in [0, -1, float("nan"), float("inf"), False]:
            with self.subTest(timeout=timeout), patch("bevy_agent_client.subprocess.Popen") as start:
                with self.assertRaises(ValueError):
                    StdioAgentClient(argv=["fake"], timeout=timeout)
                start.assert_not_called()
                with self.assertRaises(ValueError):
                    AgentClient(timeout=timeout)

    def test_parallel_calls_preserve_request_response_pairs(self):
        script = (
            "import json,sys\n"
            "for line in sys.stdin:\n"
            " request=json.loads(line)\n"
            " print(json.dumps({'jsonrpc':'2.0','id':request['id'],"
            "'result':request['params']}),flush=True)\n"
        )
        with StdioAgentClient(argv=[sys.executable, "-u", "-c", script], timeout=5) as client:
            with ThreadPoolExecutor(max_workers=4) as pool:
                results = list(pool.map(lambda value: client.call("echo", {"value": value}), range(12)))
        self.assertEqual(results, [{"value": value} for value in range(12)])
        self.assertIsNotNone(client.process.poll())

    def test_nonreading_child_write_times_out_and_is_reaped(self):
        # Exercise both the nonblocking POSIX writer and daemon fallback with
        # real pipes. A full pipe must consume the call deadline before reading.
        for writer_platform in ["posix", "nt"]:
            with self.subTest(writer_platform=writer_platform):
                client = StdioAgentClient(
                    argv=[sys.executable, "-c", "import time; time.sleep(30)"],
                    timeout=0.2,
                )
                started = time.monotonic()
                try:
                    with patch("bevy_agent_client.os.name", writer_platform):
                        with self.assertRaisesRegex(AgentError, "write timed out"):
                            client.call("agent.info", {"large": "x" * (1024 * 1024)})
                    self.assertLess(time.monotonic() - started, 2)
                    with self.assertRaisesRegex(AgentError, "unusable"):
                        client.info()
                finally:
                    client.close()
                self.assertIsNotNone(client.process.poll())
                self.assertTrue(client.process.stdin.closed)
                self.assertTrue(client.process.stdout.closed)
                if client._writer_thread is not None:
                    self.assertFalse(client._writer_thread.is_alive())


class StdioPartialLineTests(unittest.TestCase):
    """Incremental deadline: a partial single-byte write then hang must timeout."""

    def _pipe_client(self, timeout: float):
        import os as _os

        read_fd, write_fd = _os.pipe()
        # Partial line: single "{" byte, writer stays open (no EOF).
        _os.write(write_fd, b"{")
        with patch("bevy_agent_client.subprocess.Popen") as popen:
            process = MagicMock()
            process.stdin = MagicMock()
            # Text-mode read end with a real selectable fileno.
            process.stdout = _os.fdopen(read_fd, "r", buffering=1)
            process.poll.return_value = None
            process.returncode = None
            popen.return_value = process
            client = StdioAgentClient(argv=["fake"], timeout=timeout)
        self.assertIs(client.process, process)
        return client, process, write_fd

    def test_partial_single_byte_then_hang_times_out(self):
        import os as _os

        client, process, write_fd = self._pipe_client(timeout=0.3)
        try:
            with self.assertRaises(AgentError) as ctx:
                client._readline_with_timeout()
            self.assertIn("timed out", str(ctx.exception))
        finally:
            client._closed = True  # avoid terminate on fake MagicMock
            try:
                process.stdout.close()
            except Exception:
                pass
            _os.close(write_fd)

    def test_incremental_chunks_assemble_full_line(self):
        import os as _os
        import threading as _threading
        import time as _time

        read_fd, write_fd = _os.pipe()

        def _writer():
            _time.sleep(0.02)
            _os.write(write_fd, b'{"json')
            _time.sleep(0.02)
            _os.write(write_fd, b'rpc": 2}\n')

        thread = _threading.Thread(target=_writer, daemon=True)
        thread.start()
        with patch("bevy_agent_client.subprocess.Popen") as popen:
            process = MagicMock()
            process.stdin = MagicMock()
            process.stdout = _os.fdopen(read_fd, "r", buffering=1)
            process.poll.return_value = None
            popen.return_value = process
            client = StdioAgentClient(argv=["fake"], timeout=5.0)
        try:
            line = client._readline_with_timeout()
            self.assertEqual(line.strip(), '{"jsonrpc": 2}')
        finally:
            client._closed = True
            try:
                process.stdout.close()
            except Exception:
                pass
            _os.close(write_fd)
        thread.join(timeout=2)

    def test_line_exceeding_max_bytes_raises(self):
        import os as _os

        import bevy_agent_client as _client_mod

        read_fd, write_fd = _os.pipe()
        _os.write(write_fd, b"A" * 64 + b"\n")
        with patch("bevy_agent_client.subprocess.Popen") as popen:
            process = MagicMock()
            process.stdin = MagicMock()
            process.stdout = _os.fdopen(read_fd, "r", buffering=1)
            process.poll.return_value = None
            popen.return_value = process
            client = StdioAgentClient(argv=["fake"], timeout=5.0)
        old_max = _client_mod.MAX_LINE_BYTES
        _client_mod.MAX_LINE_BYTES = 16
        try:
            with self.assertRaises(AgentError) as ctx:
                client._readline_with_timeout()
            self.assertIn("exceeds", str(ctx.exception))
        finally:
            _client_mod.MAX_LINE_BYTES = old_max
            client._closed = True
            try:
                process.stdout.close()
            except Exception:
                pass
            _os.close(write_fd)

    def test_overread_applies_size_limit_to_each_line(self):
        import os as _os

        read_fd, write_fd = _os.pipe()
        line = b'{"jsonrpc":"2.0","id":1,"result":{}}\n'
        _os.write(write_fd, line + line)
        with patch("bevy_agent_client.subprocess.Popen") as popen:
            process = MagicMock()
            process.stdin = MagicMock()
            process.stdout = _os.fdopen(read_fd, "r", encoding="utf-8")
            process.poll.return_value = None
            popen.return_value = process
            client = StdioAgentClient(argv=["fake"])
        try:
            # Both lines arrive in one os.read; their aggregate size exceeds the
            # bound but each individual response remains within the limit.
            with patch("bevy_agent_client.MAX_LINE_BYTES", len(line)):
                self.assertEqual(client._readline_with_timeout(), line.decode())
                self.assertEqual(client._readline_with_timeout(), line.decode())
        finally:
            client._closed = True
            process.stdout.close()
            _os.close(write_fd)

    def test_real_subprocess_invalid_utf8_invalidates_stream(self):
        script = "import sys; sys.stdin.readline(); sys.stdout.buffer.write(b'\\xff\\n'); sys.stdout.flush()"
        with StdioAgentClient(argv=[sys.executable, "-c", script], timeout=5) as client:
            with self.assertRaisesRegex(AgentError, "invalid UTF-8"):
                client.info()
            with self.assertRaisesRegex(AgentError, "unusable"):
                client.info()

    def test_real_subprocess_partial_write_times_out_and_cleans_up(self):
        # Real process writes "{" then sleeps; the client must hit its
        # absolute deadline and terminate the child on close().
        client = StdioAgentClient(
            argv=[
                "python3",
                "-c",
                "import sys,time; sys.stdout.write('{'); sys.stdout.flush(); time.sleep(30)",
            ],
            timeout=0.5,
        )
        try:
            with self.assertRaises(AgentError) as ctx:
                client.call("agent.info")
            self.assertIn("timed out", str(ctx.exception))
            self.assertIsNone(client.process.poll(), "child should still be alive after timeout")
            with self.assertRaisesRegex(AgentError, "unusable"):
                client.info()
        finally:
            client.close()
        self.assertIsNotNone(
            client.process.poll(), "close() must reap/terminate the hung child"
        )


class ValidateEnvelopeExtraTests(unittest.TestCase):
    def test_wrong_version_raises(self):
        with self.assertRaises(AgentError):
            validate_envelope({"jsonrpc": "1.0", "id": 1, "result": {}}, 1)

    def test_missing_version_raises(self):
        with self.assertRaises(AgentError):
            validate_envelope({"id": 1, "result": {}}, 1)

    def test_error_non_dict_raises(self):
        with self.assertRaises(AgentError):
            validate_envelope(
                {"jsonrpc": "2.0", "id": 1, "error": "boom"}, 1
            )

    def test_result_none_passes_through(self):
        self.assertIsNone(
            validate_envelope({"jsonrpc": "2.0", "id": 1, "result": None}, 1)
        )

    def test_id_none_mismatch_raises(self):
        with self.assertRaises(AgentError):
            validate_envelope({"jsonrpc": "2.0", "id": None, "result": {}}, 1)


class OperationStatusTests(unittest.TestCase):
    def test_shared_facade_preserves_the_opaque_operation_id(self):
        for client_type in (AgentClient, StdioAgentClient):
            client = client_type.__new__(client_type)
            client.call = MagicMock(return_value={"state": "running"})
            self.assertEqual(client.operation_status("e0cfac39a-1"), {"state": "running"})
            client.call.assert_called_once_with(
                "agent.operations.status", {"operation_id": "e0cfac39a-1"}
            )

    def test_invalid_operation_id_does_not_send_a_request(self):
        client = AgentClient()
        client.call = MagicMock()
        for value in (None, "", True, 1):
            with self.subTest(value=value):
                with self.assertRaises(AgentError):
                    client.operation_status(value)
        client.call.assert_not_called()


class HttpDeadlineAndRetryTests(unittest.TestCase):
    def test_stalled_dns_lookup_obeys_call_deadline(self):
        release = threading.Event()
        entered = threading.Event()
        finished = threading.Event()
        lookup = socket.getaddrinfo

        def stalled_lookup(*args):
            entered.set()
            release.wait()
            try:
                return lookup(*args)
            finally:
                finished.set()

        # Release even if a regression puts the blocking lookup on the caller.
        fallback = threading.Timer(1, release.set)
        fallback.start()
        try:
            with patch("bevy_agent_client.socket.getaddrinfo", stalled_lookup):
                started = time.monotonic()
                with self.assertRaisesRegex(AgentError, "timed out|timeout"):
                    AgentClient(url="http://127.0.0.1:4000/rpc", timeout=0.05).info()
                self.assertTrue(entered.is_set())
                self.assertLess(time.monotonic() - started, 0.5)
                release.set()
                self.assertTrue(finished.wait(2))
        finally:
            release.set()
            fallback.cancel()
            fallback.join()

    def test_dns_saturation_bounds_lookup_concurrency_and_caller_waits(self):
        release = threading.Event()
        finished = threading.Event()
        lock = threading.Lock()
        lookup = socket.getaddrinfo
        counts = {"active": 0, "peak": 0}

        def stalled_lookup(*args):
            with lock:
                counts["active"] += 1
                counts["peak"] = max(counts["peak"], counts["active"])
            release.wait()
            try:
                return lookup(*args)
            finally:
                with lock:
                    counts["active"] -= 1
                    if counts["active"] == 0:
                        finished.set()

        def call():
            try:
                AgentClient(url="http://127.0.0.1:4000/rpc", timeout=0.15).info()
            except AgentError as error:
                return error
            self.fail("stalled DNS unexpectedly completed the HTTP request")

        fallback = threading.Timer(2, release.set)
        fallback.start()
        try:
            with patch("bevy_agent_client.socket.getaddrinfo", stalled_lookup):
                started = time.monotonic()
                with ThreadPoolExecutor(max_workers=32) as callers:
                    outcomes = list(callers.map(lambda _: call(), range(32)))
                self.assertLess(time.monotonic() - started, 1)
                self.assertEqual(counts["peak"], 4)
                for outcome in outcomes:
                    self.assertRegex(str(outcome), "timed out|timeout")
                release.set()
                self.assertTrue(finished.wait(2))
        finally:
            release.set()
            fallback.cancel()
            fallback.join()

    def test_retry_key_and_status_by_key_are_exposed(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"tick": 1}}).encode()
        with patch("bevy_agent_client._open_http", return_value=_http_response(body)) as transport:
            client = AgentClient()
            client.step({"type": "Noop"}, retry_key="episode-1.tick-1")
            sent = json.loads(transport.call_args.args[0].data)
            self.assertEqual(sent["retry_key"], "episode-1.tick-1")
            self.assertNotIn("retry_key", sent["params"])
        client.call = MagicMock()
        client.operation_status(retry_key="episode-1.tick-1")
        client.call.assert_called_once_with("agent.operations.status", {"retry_key": "episode-1.tick-1"})
        with self.assertRaises(AgentError):
            client.operation_status("ab-1", retry_key="tick-1")

    def test_outbound_byte_limit_and_bad_retry_key_fail_before_network(self):
        with patch("bevy_agent_client._open_http") as transport:
            client = AgentClient()
            with self.assertRaisesRegex(AgentError, "message limit"):
                client.call("agent.step", {"value": "😀" * (2 * 1024 * 1024)})
            with self.assertRaisesRegex(AgentError, "retry_key"):
                client.call("agent.step", retry_key=" ")
            transport.assert_not_called()

    def test_remote_failure_exposes_committed_tick_and_recovery_state(self):
        data = {"tick_before": 1, "tick_after": 2, "tick_committed": True, "recovery_required": True}
        with self.assertRaises(RemoteError) as outcome:
            validate_envelope({"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "failed", "data": data}}, 1)
        self.assertEqual(outcome.exception.data, data)

    def test_trickled_headers_and_bodies_share_a_total_deadline(self):
        for stage in ["headers", "body"]:
            with self.subTest(stage=stage):
                listener = socket.socket()
                listener.bind(("127.0.0.1", 0))
                listener.listen()
                url = f"http://127.0.0.1:{listener.getsockname()[1]}/rpc"
                stopped = threading.Event()
                def serve():
                    connection = None
                    try:
                        connection, _ = listener.accept()
                        connection.settimeout(1)
                        request = bytearray()
                        while b"\r\n\r\n" not in request:
                            request.extend(connection.recv(4096))
                        body = b'{"jsonrpc":"2.0","id":1,"result":{"ok":true}}'
                        headers = f"HTTP/1.1 200 OK\r\nContent-Length: {len(body)}\r\n\r\n".encode()
                        if stage == "body":
                            connection.sendall(headers)
                            trickle = body
                        else:
                            trickle = headers + body
                        for byte in trickle:
                            if stopped.wait(0.01):
                                break
                            connection.sendall(bytes([byte]))
                    except OSError:
                        pass
                    finally:
                        if connection is not None:
                            connection.close()
                worker = threading.Thread(target=serve)
                worker.start()
                started = time.monotonic()
                try:
                    with self.assertRaisesRegex(AgentError, "timed out|timeout"):
                        AgentClient(url=url, timeout=0.12).info()
                    self.assertLess(time.monotonic() - started, 0.6)
                finally:
                    stopped.set()
                    worker.join(timeout=2)
                    listener.close()
                self.assertFalse(worker.is_alive())


if __name__ == "__main__":
    unittest.main()

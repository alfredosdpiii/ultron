from __future__ import annotations

import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import unittest


RUNTIME = Path(__file__).resolve().parents[1] / "agent/extensions/prime-rlm/runtime.py"
PREVIEW_BYTES = 8192
TRUNCATION_MARKER = "\n... [truncated]"


class RuntimeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.process = subprocess.Popen(
            [sys.executable, "-B", "-u", str(RUNTIME)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        )
        self.addCleanup(self.close_runtime)
        self.frames: queue.Queue[str | None] = queue.Queue()
        self.reader = threading.Thread(target=self.read_frames, daemon=True)
        self.reader.start()
        self.counter = 0
        ready = self.next_frame()
        self.assertEqual(ready, {"event": "ready", "protocol": 1, "pid": self.process.pid})

    def read_frames(self) -> None:
        try:
            for line in self.process.stdout:
                self.frames.put(line)
        finally:
            self.frames.put(None)

    def next_frame(self) -> dict:
        try:
            line = self.frames.get(timeout=15)
        except queue.Empty:
            self.fail("Runtime did not emit a protocol frame within 15 seconds")
        self.assertIsNotNone(line, "Runtime closed stdout before completing the request")
        return json.loads(line)

    def close_runtime(self) -> None:
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
        self.reader.join(timeout=5)
        self.process.stdout.close()
        self.process.stderr.close()

    def request(self, request: str, **fields) -> list[dict]:
        self.counter += 1
        request_id = f"test-{self.counter}"
        self.process.stdin.write(json.dumps({"request": request, "id": request_id, **fields}) + "\n")
        self.process.stdin.flush()
        frames = []
        while True:
            frame = self.next_frame()
            self.assertEqual(frame.get("id"), request_id)
            frames.append(frame)
            if frame["event"] == "done":
                return frames

    def execute(self, code: str, status: str = "ok") -> dict[str, dict]:
        frames = self.request("execute", code=code)
        self.assertEqual(
            [frame["event"] for frame in frames],
            ["stdout", "stderr", "result" if status == "ok" else "error", "done"],
        )
        self.assertEqual(frames[-1]["status"], status)
        return {frame["event"]: frame for frame in frames}

    def assert_preview(self, text: str, *, truncated: bool = True) -> None:
        self.assertLessEqual(len(text.encode("utf-8")), PREVIEW_BYTES)
        if truncated:
            self.assertTrue(text.endswith(TRUNCATION_MARKER), repr(text[-80:]))

    def test_process_persistence_and_top_level_await(self) -> None:
        first = self.execute("answer = 40\nprint('ready')\nanswer")
        self.assertEqual(first["stdout"]["text"], "ready\n")
        self.assertEqual(first["stderr"]["text"], "")
        self.assertEqual(first["result"]["result"], "40")
        second = self.execute(
            "import asyncio\n"
            "answer += await asyncio.sleep(0, result=2)\n"
            "answer"
        )
        self.assertEqual(second["result"]["result"], "42")
        self.assertEqual(self.execute("answer += 1")["result"]["result"], "")
        self.assertEqual(self.execute("answer")["result"]["result"], "43")
        self.assertIsNone(self.process.poll())

    def test_host_bridge_protocol_during_top_level_await(self) -> None:
        self.process.stdin.write(json.dumps({
            "request": "execute", "id": "bridge", "code": "await bash('printf hello')",
        }) + "\n")
        self.process.stdin.flush()
        host = self.next_frame()
        self.assertEqual(host["event"], "host_request")
        self.assertEqual(host["type"], "bash")
        self.assertEqual(host["payload"], {"command": "printf hello"})
        self.process.stdin.write(json.dumps({
            "request": "host_reply", "id": host["id"], "payload": {"stdout": "hello"},
        }) + "\n")
        self.process.stdin.flush()
        frames = [self.next_frame() for _ in range(4)]
        self.assertEqual([frame["event"] for frame in frames], ["stdout", "stderr", "result", "done"])
        self.assertTrue(all(frame["id"] == "bridge" for frame in frames))
        self.assertEqual(frames[2]["result"], "{'stdout': 'hello'}")
        self.assertEqual(frames[3]["status"], "ok")

    def test_stdout_stderr_exact_byte_boundaries(self) -> None:
        for text in ("a" * 8192, "🙂" * 2048, "a" * 8191 + "é", "🙂" * 2049):
            with self.subTest(length=len(text), bytes=len(text.encode("utf-8"))):
                frames = self.execute(
                    f"import sys\ntext = {text!r}\n"
                    "counts = (sys.stdout.write(text), sys.stderr.write(text))\ncounts"
                )
                self.assertEqual(frames["result"]["result"], repr((len(text), len(text))))
                for event in ("stdout", "stderr"):
                    preview = frames[event]["text"]
                    if len(text.encode("utf-8")) <= PREVIEW_BYTES:
                        self.assertEqual(preview, text)
                    else:
                        self.assert_preview(preview)
                        self.assertTrue(text.startswith(preview.removesuffix(TRUNCATION_MARKER)))
                        self.assertNotIn("\ufffd", preview)
        after = self.execute("print('fresh')")
        self.assertEqual(after["stdout"]["text"], "fresh\n")
        self.assertEqual(after["stderr"]["text"], "")

    def test_unicode_flood_is_bounded_while_writing(self) -> None:
        frames = self.execute(
            "import sys, tracemalloc\n"
            "chunk = '🙂é漢' * 4096\n"
            "tracemalloc.start()\n"
            "for i in range(512):\n"
            "    written = sys.stdout.write(chunk)\n"
            "    err_written = sys.stderr.write(chunk)\n"
            "    assert written == err_written == len(chunk)\n"
            "peak = tracemalloc.get_traced_memory()[1]\n"
            "tracemalloc.stop()\n"
            "sys.stdout.flush()\n"
            "sys.stderr.flush()\n"
            "peak"
        )
        for event in ("stdout", "stderr"):
            preview = frames[event]["text"]
            self.assert_preview(preview)
            self.assertTrue(("🙂é漢" * 4096).startswith(preview.removesuffix(TRUNCATION_MARKER)))
        # The cell writes 36 MiB. A final-only slice or retained chunks exceeds
        # this allowance; a bounded writer needs only a small fixed buffer.
        self.assertLess(int(frames["result"]["result"]), 1_000_000)

    def test_single_large_write_uses_bounded_temporary_space(self) -> None:
        frames = self.execute(
            "import sys, tracemalloc\n"
            "text = '🙂' * 2_000_000\n"
            "tracemalloc.start()\n"
            "counts = (sys.stdout.write(text), sys.stderr.write(text))\n"
            "peak = tracemalloc.get_traced_memory()[1]\n"
            "tracemalloc.stop()\n"
            "(counts, peak)"
        )
        import ast
        counts, peak = ast.literal_eval(frames["result"]["result"])
        self.assertEqual(counts, (2_000_000, 2_000_000))
        self.assertLess(peak, 1_000_000)
        self.assert_preview(frames["stdout"]["text"])
        self.assert_preview(frames["stderr"]["text"])

    def test_large_builtin_results_leave_full_namespace_intact(self) -> None:
        cases = [
            ("'🙂' * 1_000_000", "len(value)", "1000000"),
            ("b'x' * 1_000_000", "len(value)", "1000000"),
            ("bytearray(b'x' * 1_000_000)", "len(value)", "1000000"),
            ("list(range(100_000))", "(len(value), value[-1])", "(100000, 99999)"),
            ("tuple(range(100_000))", "(len(value), value[-1])", "(100000, 99999)"),
            ("dict.fromkeys(range(100_000), 'kept')", "(len(value), value[99999])", "(100000, 'kept')"),
            ("set(range(100_000))", "(len(value), 99999 in value)", "(100000, True)"),
            ("frozenset(range(100_000))", "(len(value), 99999 in value)", "(100000, True)"),
            ("10 ** 100_000", "value == 10 ** 100_000", "True"),
            ("range(10 ** 100_000)", "value.stop == 10 ** 100_000", "True"),
            ("{'nested': ['🙂' * 1_000_000]}", "len(value['nested'][0])", "1000000"),
        ]
        for expression, check, expected in cases:
            with self.subTest(expression=expression):
                preview = self.execute(f"value = {expression}\nvalue")["result"]["result"]
                self.assert_preview(preview)
                self.assertEqual(self.execute(check)["result"]["result"], expected)

    def test_result_preview_does_not_build_full_builtin_repr(self) -> None:
        self.execute("import tracemalloc\nvalue = ['🙂' * 1_000_000] * 100")
        frames = self.execute("tracemalloc.start()\nvalue")
        self.assert_preview(frames["result"]["result"])
        measurement = self.execute(
            "peak = tracemalloc.get_traced_memory()[1]\ntracemalloc.stop()\npeak"
        )
        self.assertLess(int(measurement["result"]["result"]), 2_000_000)
        self.assertEqual(self.execute("(len(value), len(value[-1]))")["result"]["result"], "(100, 1000000)")

    def test_small_builtin_representations_and_recursive_values(self) -> None:
        cases = [
            "None", "True", "-123", "1.5", "1+2j", "'hello🙂'", "b'bytes'",
            "bytearray(b'abc')", "[]", "[1, 'two']", "()", "(1,)", "(1, 2)",
            "{}", "{'key': [1, 2]}", "set()", "{1}", "frozenset()",
            "frozenset({1})", "range(3)", "range(1, 10, 2)", "slice(1, 2, 3)",
        ]
        for expression in cases:
            with self.subTest(expression=expression):
                self.assertEqual(self.execute(expression)["result"]["result"], repr(eval(expression)))
        self.assertEqual(self.execute("cycle = []\ncycle.append(cycle)\ncycle")["result"]["result"], "[[...]]")
        self.assertEqual(self.execute("cycle = {}\ncycle['self'] = cycle\ncycle")["result"]["result"], "{'self': {...}}")
        frames = self.execute("nested = []\nfor i in range(2000):\n    nested = [nested]\nnested")
        self.assert_preview(frames["result"]["result"])
        self.assertEqual(self.execute("len(nested)")["result"]["result"], "1")

    def test_exception_then_recovery_preserves_state_and_output(self) -> None:
        frames = self.execute(
            "import sys\nkept = 41\nprint('before')\n"
            "print('warning', file=sys.stderr)\nraise ValueError('broken')",
            status="error",
        )
        self.assertEqual(frames["stdout"]["text"], "before\n")
        self.assertEqual(frames["stderr"]["text"], "warning\n")
        error = frames["error"]
        self.assertEqual(error["ename"], "ValueError")
        self.assertEqual(error["evalue"], "broken")
        self.assertIsInstance(error["traceback"], list)
        self.assertIn("<rlm-cell>", "\n".join(error["traceback"]))
        self.assertIn("ValueError: broken", "\n".join(error["traceback"]))
        self.assertEqual(self.execute("kept += 1\nkept")["result"]["result"], "42")
        self.execute("if", status="error")
        self.assertEqual(self.execute("kept")["result"]["result"], "42")

    def test_error_message_and_traceback_byte_bounds(self) -> None:
        for expression in ("ValueError('🙂' * 100_000)", "KeyError('é' * 100_000)", "ValueError(list(range(100_000)))"):
            with self.subTest(expression=expression):
                error = self.execute(f"raise {expression}", status="error")["error"]
                self.assert_preview(error["evalue"])
                self.assert_preview("\n".join(error["traceback"]))
                self.assertEqual(self.execute("6 * 7")["result"]["result"], "42")
        error = self.execute(
            "def fail(n):\n"
            "    if n:\n"
            "        return fail(n - 1)\n"
            "    raise RuntimeError('bottom')\n"
            "fail(400)", status="error",
        )["error"]
        self.assertEqual(error["evalue"], "bottom")
        self.assert_preview("\n".join(error["traceback"]))
        error = self.execute(
            "try:\n"
            "    raise ValueError('é' * 100_000)\n"
            "except ValueError as cause:\n"
            "    raise RuntimeError('outer') from cause", status="error",
        )["error"]
        self.assertEqual(error["ename"], "RuntimeError")
        self.assertEqual(error["evalue"], "outer")
        self.assert_preview("\n".join(error["traceback"]))
        self.assertEqual(self.execute("40 + 2")["result"]["result"], "42")

    def test_broken_exception_string_does_not_break_protocol(self) -> None:
        error = self.execute(
            "class BadError(Exception):\n"
            "    def __str__(self):\n"
            "        raise RuntimeError('bad str')\n"
            "raise BadError()", status="error",
        )["error"]
        self.assertEqual(error["ename"], "BadError")
        self.assert_preview(error["evalue"], truncated=False)
        self.assertEqual(self.execute("42")["result"]["result"], "42")

    def test_other_error_frames_are_bounded(self) -> None:
        error = self.request("🙂" * 100_000)[0]
        self.assertEqual(error["status"], "error")
        self.assert_preview(error["error"])
        error = self.request("snapshot", path="/" + "é" * 100_000)[0]
        self.assertEqual(error["status"], "error")
        self.assert_preview(error["error"])
        # Missing snapshots are a supported no-op, not a restore failure.
        with tempfile.TemporaryDirectory() as directory:
            missing = self.request("restore", path=str(Path(directory) / "missing"))[0]
            self.assertEqual(missing["status"], "ok")
            self.assertTrue(missing["restore"]["missing"])
            invalid = Path(directory) / "invalid"
            invalid.write_text("not a snapshot")
            error = self.request("restore", path=str(invalid))[0]
            self.assertEqual(error["status"], "error")
            self.assert_preview(error["error"], truncated=False)
        self.assertEqual(self.execute("42")["result"]["result"], "42")

    def test_snapshot_round_trip_keeps_full_values_and_protocol(self) -> None:
        self.execute("payload = {'text': '🙂' * 10_000, 'items': list(range(10_000))}")
        preview = self.execute("payload")["result"]["result"]
        self.assert_preview(preview)
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / "snapshot")
            saved = self.request("snapshot", path=path)[0]
            self.assertEqual(saved["status"], "ok")
            self.assertIn("payload", saved["snapshot"]["saved"])
            self.assertIn(saved["snapshot"]["serializer"], ("json", "dill"))
            self.execute("payload = None")
            restored = self.request("restore", path=path)[0]
            self.assertEqual(restored["status"], "ok")
            self.assertIn("payload", restored["restore"]["restored"])
            self.assertFalse(restored["restore"]["missing"])
            self.assertEqual(
                self.execute("(len(payload['text']), len(payload['items']), payload['items'][-1])")["result"]["result"],
                "(10000, 10000, 9999)",
            )
        self.assertIn("payload", self.request("list_names")[0]["names"])


if __name__ == "__main__":
    unittest.main()

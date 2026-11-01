import hashlib
import importlib.util
import json
import os
import select
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import tracemalloc
import unittest


SCRIPT = Path(__file__).with_name("smoke-agent.py")
SPEC = importlib.util.spec_from_file_location("smoke_agent", SCRIPT)
SMOKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)

NORMAL_TIMEOUT = 10
NATIVE_TIMEOUT = 0.6
CLEANUP_TIMEOUT = 1
READY_TIMEOUT = 10
DRIVER_TIMEOUT = 7 * (NORMAL_TIMEOUT + 3 * CLEANUP_TIMEOUT) + 2


class SmokeAgentTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="hls-smoke-test-")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        self.calls = self.root / "hls-calls.jsonl"
        self.ready = self.root / "ready"
        self.hls = self.executable("hls", (
            "import json, sys\n"
            f"with open({str(self.calls)!r}, 'a') as log:\n"
            "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
        ))

    def executable(self, name, source):
        path = self.root / name
        path.write_text(f"#!{sys.executable}\n" + source)
        path.chmod(0o755)
        return path

    def driver(self, native, native_timeout=None):
        ready_read, ready_write = os.pipe()
        wrapper = f"""
import importlib.util, os, select, sys
from unittest.mock import patch
spec = importlib.util.spec_from_file_location('smoke', {str(SCRIPT)!r})
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)
original_run = smoke.run
original_popen = smoke.subprocess.Popen

def ready_popen(*args, **kwargs):
    ready_read, ready_write = os.pipe()
    try:
        kwargs['env'] = {{**kwargs['env'], 'HLS_TEST_READY_FD': str(ready_write)}}
        process = original_popen(*args, **kwargs, pass_fds=(ready_write,))
    finally:
        os.close(ready_write)
    try:
        if not select.select([ready_read], [], [], {READY_TIMEOUT})[0]:
            raise RuntimeError('Native fixture did not become ready')
        if os.read(ready_read, 1) != b'R':
            raise RuntimeError('Native fixture closed its readiness pipe')
        os.write({ready_write}, b'R')
    except BaseException:
        smoke.stop_process(process, {CLEANUP_TIMEOUT})
        raise
    finally:
        os.close(ready_read)
    return process

def run(command, env, cwd):
    if str(command[0]) != {str(native)!r} or {native_timeout!r} is None:
        return original_run(command, env, cwd, timeout={NORMAL_TIMEOUT}, cleanup_timeout={CLEANUP_TIMEOUT})
    with patch.object(smoke.subprocess, 'Popen', ready_popen):
        result = original_run(command, env, cwd, timeout={native_timeout!r}, cleanup_timeout={CLEANUP_TIMEOUT})
    os.write({ready_write}, b'D')
    return result

smoke.run = run
sys.exit(smoke.main())
"""
        try:
            process = subprocess.Popen(
                [sys.executable, "-B", "-c", wrapper, "antigravity", str(native),
                 "--hls", str(self.hls), "--root-parent", str(self.root)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                pass_fds=(ready_write,),
            )
        except BaseException:
            os.close(ready_read)
            raise
        finally:
            os.close(ready_write)
        self.addCleanup(self.stop_driver, process)
        try:
            if native_timeout is not None:
                self.assert_pipe_signal(ready_read, b"R", 3 * NORMAL_TIMEOUT + READY_TIMEOUT + 2)
                self.assert_pipe_signal(ready_read, b"D", native_timeout + 3 * CLEANUP_TIMEOUT + 2)
        finally:
            os.close(ready_read)
        return process

    def assert_pipe_signal(self, descriptor, expected, timeout):
        self.assertTrue(select.select([descriptor], [], [], timeout)[0], f"Timed out waiting for {expected!r}")
        self.assertEqual(os.read(descriptor, 1), expected)

    def stop_driver(self, process):
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
        try:
            process.communicate(timeout=DRIVER_TIMEOUT)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=CLEANUP_TIMEOUT)

    def finish(self, process, expected):
        stdout, stderr = process.communicate(timeout=DRIVER_TIMEOUT)
        self.assertEqual(process.returncode, expected, stdout + stderr)
        report_path = Path(stdout.splitlines()[0])
        self.assertEqual(report_path.name, "result.json")
        report = json.loads(report_path.read_text())
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertEqual(calls[-1], ["uninstall", "--consumer", "native-smoke"])
        self.assertEqual(report["results"][-1]["exit"], 0)
        return report

    def native_result(self, report, native):
        attempts = [result for result in report["results"] if result["command"][0] == str(native)]
        self.assertEqual(len(attempts), 1)
        self.assertEqual(attempts[0]["command"], [str(native), "--version"])
        return attempts[0]

    def test_timeout_with_zero_exit_stops_prompt_and_reports_failure(self):
        native = self.executable("native", (
            "import os, signal, sys, time\n"
            "signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))\n"
            "print('started', flush=True)\n"
            "os.write(int(os.environ['HLS_TEST_READY_FD']), b'R')\n"
            "time.sleep(60)\n"
        ))
        result = self.native_result(self.finish(self.driver(native, native_timeout=NATIVE_TIMEOUT), 1), native)
        self.assertTrue(result["timedOut"])
        self.assertEqual(result["exit"], 0)
        self.assertEqual(result["stdout"], "started\n")
        with self.assertRaises(ProcessLookupError):
            os.kill(result["pid"], 0)

    def test_launch_failure_records_os_error_without_pid(self):
        native = self.root / "native"
        native.write_text("#!/hls-missing-test-interpreter\n")
        native.chmod(0o755)
        result = self.native_result(self.finish(self.driver(native), 1), native)
        self.assertIsNone(result["pid"])
        self.assertIsNone(result["exit"])
        self.assertIn("No such file", result["launchError"])

    def test_invalid_bytes_preserve_command_and_cleanup_results(self):
        native = self.executable("native", "import os\nos.write(2, b'error: \\xff\\n')\nraise SystemExit(7)\n")
        result = self.native_result(self.finish(self.driver(native), 1), native)
        self.assertEqual(result["exit"], 7)
        self.assertEqual(result["stderr"], "error: \ufffd\n")

    def test_detached_pipe_writer_has_bounded_drain_and_retained_output(self):
        native = self.executable("native", (
            "import os, time\n"
            "from pathlib import Path\n"
            "if os.fork() == 0:\n"
            "    os.setsid()\n"
            f"    Path({str(self.ready)!r}).write_text(str(os.getpid()))\n"
            "    os.write(1, b'backend: \\xff\\n')\n"
            "    os.write(int(os.environ['HLS_TEST_READY_FD']), b'R')\n"
            "    time.sleep(60)\n"
            "    os._exit(0)\n"
        ))
        try:
            result = self.native_result(self.finish(self.driver(native, native_timeout=NATIVE_TIMEOUT), 1), native)
            self.assertTrue(result["timedOut"])
            self.assertEqual(result["exit"], 0)
            self.assertEqual(result["stdout"], "backend: \ufffd\n")
            self.assertIn("Capture pipes remained open after process-group cleanup", result["cleanupErrors"])
            os.kill(int(self.ready.read_text()), 0)
        finally:
            if self.ready.exists():
                os.kill(int(self.ready.read_text()), signal.SIGKILL)

    def test_interrupt_reaps_native_and_publishes_report(self):
        native = self.executable("native", (
            "import os, signal, time\n"
            "from pathlib import Path\n"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            f"Path({str(self.ready)!r}).write_text(str(os.getpid()))\n"
            "time.sleep(60)\n"
        ))
        process = self.driver(native)
        try:
            deadline = time.monotonic() + 3 * NORMAL_TIMEOUT + READY_TIMEOUT + 2
            while not self.ready.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(self.ready.exists())
            process.send_signal(signal.SIGINT)
            report = self.finish(process, 130)
            result = self.native_result(report, native)
            self.assertTrue(result["interrupted"])
            self.assertEqual(result["pid"], int(self.ready.read_text()))
            self.assertEqual(result["exit"], -signal.SIGKILL)
            with self.assertRaises(ProcessLookupError):
                os.kill(result["pid"], 0)
            self.assertEqual(report["results"][-2], result)
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=3)
            if self.ready.exists():
                try:
                    os.killpg(int(self.ready.read_text()), signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_success_preserves_hashes_and_runs_prompt(self):
        native = self.executable("native", "print('OK')\n")
        report = self.finish(self.driver(native), 0)
        attempts = [result for result in report["results"] if result["command"][0] == str(native)]
        self.assertEqual(len(attempts), 2)
        self.assertEqual(attempts[1]["command"][-2:], ["-p", "Reply with OK. Do not use tools."])
        self.assertEqual(report["binarySha256"], hashlib.sha256(native.read_bytes()).hexdigest())
        self.assertEqual(report["hlsSha256"], hashlib.sha256(self.hls.read_bytes()).hexdigest())

    def test_hashing_uses_bounded_memory(self):
        path = self.root / "large-binary"
        chunk = b"x" * (1024 * 1024)
        expected = hashlib.sha256()
        with path.open("wb") as destination:
            for _ in range(12):
                destination.write(chunk)
                expected.update(chunk)
        tracemalloc.start()
        try:
            self.assertEqual(SMOKE.sha256(path), expected.hexdigest())
            self.assertLess(tracemalloc.get_traced_memory()[1], 4 * 1024 * 1024)
        finally:
            tracemalloc.stop()


if __name__ == "__main__":
    unittest.main()

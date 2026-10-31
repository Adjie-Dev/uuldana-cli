"""Exercise watchdog teardown against real children in separate Unix sessions."""
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("stress_runner", Path(__file__).resolve().parents[1] / "stress.py")
stress = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stress)


@unittest.skipIf(os.name == "nt", "Windows uses native taskkill tree cleanup")
class WatchdogTest(unittest.TestCase):
    def assert_retired(self, pid):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            result = subprocess.run(["ps", "-p", str(pid), "-o", "stat="], capture_output=True, text=True, check=False)
            if result.returncode != 0 or result.stdout.strip().startswith("Z"):
                return
            time.sleep(0.01)
        self.fail(f"watchdog left fixture {pid} running")

    def test_snapshot_failure_after_stop_still_kills_and_reaps_harness(self):
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
        real_snapshot = stress.unix_process_snapshot
        injected = False

        def failing_snapshot():
            nonlocal injected
            table = real_snapshot()
            if process.pid in table and table[process.pid][1].startswith("T"):
                injected = True
                raise subprocess.TimeoutExpired("ps", 1)
            return table

        try:
            with patch.object(stress, "unix_process_snapshot", side_effect=failing_snapshot):
                issues = stress.terminate_unix_tree(process)
            self.assertTrue(injected, "fault must happen after real SIGSTOP delivery")
            self.assertTrue(issues)
            self.assertEqual(process.returncode, -signal.SIGKILL)
        finally:
            process.kill()
            process.wait(timeout=5)

    def test_cleanup_during_active_detached_child_spawning(self):
        with tempfile.TemporaryDirectory() as folder:
            marker = Path(folder) / "pids"
            code = """
import pathlib,subprocess,sys,time
with pathlib.Path(sys.argv[1]).open('w', buffering=1) as output:
    for _ in range(128):
        child = subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'], start_new_session=True)
        output.write(f'{child.pid}\\n')
        time.sleep(0.002)
    time.sleep(60)
"""
            process = subprocess.Popen([sys.executable, "-c", code, str(marker)], start_new_session=True)
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if marker.exists() and len(marker.read_text().splitlines()) >= 3:
                        break
                    time.sleep(0.005)
                else:
                    self.fail("spawning fixture did not start")
                self.assertEqual(stress.terminate_unix_tree(process), [])
                self.assertEqual(process.returncode, -signal.SIGKILL)
                for pid in marker.read_text().splitlines():
                    self.assert_retired(int(pid))
            finally:
                if process.poll() is None:
                    stress.terminate_unix_tree(process)

    def test_timeout_cleanup_reaches_children_that_create_new_sessions(self):
        with tempfile.TemporaryDirectory() as folder:
            marker = Path(folder) / "pids"
            child_code = """
import os,pathlib,subprocess,sys,time
grandchild = subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'], start_new_session=True)
pathlib.Path(sys.argv[1]).write_text(f'{os.getpid()} {grandchild.pid}')
time.sleep(60)
"""
            parent_code = """
import subprocess,sys,time
child = subprocess.Popen([sys.executable,'-c',sys.argv[1],sys.argv[2]], start_new_session=True)
time.sleep(60)
"""
            process = subprocess.Popen([sys.executable, "-c", parent_code, child_code, str(marker)], start_new_session=True)
            descendants = []
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if marker.exists():
                        descendants = [int(value) for value in marker.read_text().split()]
                        if len(descendants) == 2:
                            break
                    time.sleep(0.01)
                self.assertEqual(len(descendants), 2, "real fixture must start")
                self.assertEqual(stress.terminate_unix_tree(process), [])
                self.assertEqual(process.wait(timeout=5), -signal.SIGKILL)
                for pid in descendants:
                    self.assert_retired(pid)
            finally:
                if process.poll() is None:
                    stress.terminate_unix_tree(process)
                    process.kill()
                    process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()

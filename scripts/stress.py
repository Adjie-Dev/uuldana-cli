#!/usr/bin/env python3
"""Run exact Cargo test artifacts with external deadlines on every CI host."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import datetime
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time


def unix_process_snapshot():
    output = subprocess.check_output(["ps", "-axo", "pid=,ppid=,stat=,lstart="], text=True, timeout=1)
    return {int(fields[0]): (int(fields[1]), fields[2], fields[3])
            for line in output.splitlines() if len(fields := line.strip().split(maxsplit=3)) == 4}


def terminate_unix_tree(process):
    """Bounded best-effort fixture cleanup, including detached descendant sessions."""
    owned = {}
    issues = []
    try:
        table = unix_process_snapshot()
        if process.pid not in table:
            return issues
        owned[process.pid] = table[process.pid][2]
        frozen = set()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            for pid, identity in list(owned.items()):
                if time.monotonic() >= deadline:
                    break
                if pid in frozen:
                    continue
                table = unix_process_snapshot()
                if pid not in table or table[pid][2] != identity:
                    frozen.add(pid)
                    continue
                try:
                    os.kill(pid, signal.SIGSTOP)
                except ProcessLookupError:
                    frozen.add(pid)
                    continue
                # Signal delivery is asynchronous. Confirm stopped state before
                # trusting a parent's final child list.
                while time.monotonic() < deadline:
                    table = unix_process_snapshot()
                    row = table.get(pid)
                    if row is None or row[2] != identity or row[1][0] in "TtZ":
                        frozen.add(pid)
                        break
                    time.sleep(0.005)
            table = unix_process_snapshot()
            additions = {pid: identity for pid, (parent, _, identity) in table.items()
                         if parent in owned and pid not in owned}
            owned.update(additions)
            if not additions and frozen.issuperset(owned):
                break
        else:
            issues.append("Unix watchdog discovery deadline expired")
    except (OSError, subprocess.SubprocessError) as error:
        issues.append(f"Unix watchdog snapshot/stop failed: {error}")
    finally:
        # Even a failed snapshot must never leave a known fixture SIGSTOPped.
        # Frozen parents cannot reap children and allow their PIDs to be reused.
        try:
            final_table = unix_process_snapshot()
        except (OSError, subprocess.SubprocessError):
            final_table = None
        for pid, identity in reversed(list(owned.items())):
            if final_table is not None and (pid not in final_table or final_table[pid][2] != identity):
                continue
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except OSError as error:
                issues.append(f"Unix watchdog kill {pid} failed: {error}")
        process.kill()
        process.wait(timeout=5)
    return issues


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--parallel-runs", type=int, default=12)
    parser.add_argument("--sequential-runs", type=int, default=10)
    parser.add_argument("--deadline", type=int, default=60)
    args = parser.parse_args()
    if min(args.parallel_runs, args.sequential_runs, args.deadline) < 1:
        parser.error("run counts and deadline must be positive")
    root = Path(__file__).resolve().parent.parent
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    logs = root / "target" / "verification" / "stress" / f"{stamp}-{os.getpid()}"
    logs.mkdir(parents=True)
    packages = ["-p", "uuldana-runtime", "-p", "uuldana-transport"]
    with (logs / "build.stderr.log").open("w") as errors:
        subprocess.run(["cargo", "build", *packages, "--examples", "--locked"], cwd=root, stderr=errors, check=True)
        with (logs / "build.jsonl").open("w") as output:
            subprocess.run(["cargo", "test", *packages, "--no-run", "--locked", "--message-format=json"], cwd=root, stdout=output, stderr=errors, check=True)
    specs = [
        ("runtime-lib", "uuldana_runtime", "lib", False),
        ("runtime-lifecycle", "lifecycle", "test", True),
        ("runtime-reliability", "reliability", "test", True),
        ("runtime-probe", "probe", "test", False),
        ("transport-lib", "uuldana_transport", "lib", False),
        ("transport-process", "process", "test", True),
    ]
    artifacts = [json.loads(line) for line in (logs / "build.jsonl").read_text().splitlines()]
    executables = {}
    for name, target, kind, _ in specs:
        matches = {item["executable"] for item in artifacts
                   if item.get("reason") == "compiler-artifact" and item.get("executable")
                   and item["profile"]["test"] and item["target"]["name"] == target
                   and kind in item["target"]["kind"]}
        if len(matches) != 1:
            raise RuntimeError(f"expected one {name} test artifact: {matches}")
        executable = Path(matches.pop()).resolve(strict=True)
        if not executable.is_relative_to((root / "target").resolve()):
            raise RuntimeError(f"test artifact outside workspace target: {executable}")
        executables[name] = executable

    results = []

    def execute(name, stage, number, threads):
        stem = logs / f"{stage}-{name}-{number:03d}"
        start = time.monotonic()
        timed_out = False
        with stem.with_suffix(".stdout.log").open("w") as output, stem.with_suffix(".stderr.log").open("w") as errors:
            process = subprocess.Popen([str(executables[name]), f"--test-threads={threads}", "--nocapture"], cwd=root, stdout=output, stderr=errors, start_new_session=os.name != "nt")
            try:
                code = process.wait(timeout=args.deadline)
            except subprocess.TimeoutExpired:
                timed_out = True
                try:
                    if os.name == "nt":
                        subprocess.run(["taskkill.exe", "/PID", str(process.pid), "/T", "/F"], stdout=errors, stderr=errors, timeout=10, check=False)
                    else:
                        for issue in terminate_unix_tree(process):
                            errors.write(issue + "\n")
                finally:
                    process.kill()
                    code = process.wait(timeout=10)
        summary = re.search(r"test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;", stem.with_suffix(".stdout.log").read_text())
        return {"stage": stage, "target": name, "run": number, "threads": threads,
                "process_id": process.pid, "exit_code": code, "timed_out": timed_out,
                "seconds": round(time.monotonic() - start, 3),
                "passed": not timed_out and code == 0 and bool(summary), "log": str(stem)}

    with (logs / "results.jsonl").open("w") as report:
        def record(batch):
            for result in batch:
                results.append(result)
                report.write(json.dumps(result) + "\n")
                report.flush()
                print(f"{'PASS' if result['passed'] else 'FAIL'} {result['stage']} {result['target']} {result['run']}", flush=True)
            if not all(result["passed"] for result in batch):
                raise RuntimeError(f"stress regression or watchdog failure: {logs}")

        for name, _, _, _ in specs:
            with ThreadPoolExecutor(max_workers=args.parallel_runs) as pool:
                record(list(pool.map(lambda number: execute(name, "parallel", number, 4), range(1, args.parallel_runs + 1))))
        for name, _, _, repeat in specs:
            if repeat:
                for number in range(1, args.sequential_runs + 1):
                    record([execute(name, "sequential", number, 1)])
    assert len(results) == 6 * args.parallel_runs + 3 * args.sequential_runs
    print(f"RESULT stress exit=0 executions={len(results)} logs={logs}")


if __name__ == "__main__":
    main()

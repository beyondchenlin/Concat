#!/usr/bin/env python3
"""Read-only resource evidence for a Nix build; never infer a failure cause."""

import datetime
import json
import os
from pathlib import Path
import subprocess
import sys


def read(path):
    try:
        return {"text": Path(path).read_text(encoding="utf-8", errors="replace")}
    except OSError as error:
        return {"error": str(error)}


def command(arguments):
    try:
        result = subprocess.run(
            arguments, capture_output=True, text=True, errors="replace", timeout=15,
            check=False,
        )
        return {
            "command": arguments, "exit_code": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr,
        }
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"command": arguments, "error": str(error)}


def cgroups():
    # Include the visible hierarchy, not just this observer's cgroup: daemon
    # builds can run in a different service and limits can live at ancestors.
    # Missing files are not zero-valued events. v1 interfaces are retained on
    # hosts which expose them; Ubuntu's v2 counters keep their original names.
    names = {
        "cgroup.events", "memory.current", "memory.peak", "memory.high", "memory.max",
        "memory.swap.current", "memory.swap.max", "memory.events", "memory.events.local",
        "memory.pressure", "cpu.max", "cpu.stat", "cpuset.cpus.effective",
        "pids.current", "pids.max", "pids.events", "memory.usage_in_bytes",
        "memory.max_usage_in_bytes", "memory.limit_in_bytes", "memory.failcnt",
        "memory.oom_control", "cpu.cfs_quota_us", "cpu.cfs_period_us",
    }
    groups = {}
    errors = []
    for directory, _, files in os.walk(
        "/sys/fs/cgroup", onerror=lambda error: errors.append(str(error))
    ):
        present = names.intersection(files)
        if present:
            groups[directory] = {name: read(Path(directory) / name) for name in sorted(present)}
    return {"root": "/sys/fs/cgroup", "groups": groups, "scan_errors": errors}


def process_membership():
    # Record names and membership only, never command lines or environment
    # variables. Nix service processes can outlive the foreground client.
    processes = {}
    for path in Path("/proc").glob("[0-9]*"):
        identity = read(path / "comm")
        name = identity.get("text", "").strip()
        if int(path.name) in {os.getpid(), os.getppid()} or name in {
            "nix", "nix-daemon", "cargo", "rustc"
        } or name.startswith("determinate"):
            processes[path.name] = {"name": identity, "cgroup": read(path / "cgroup")}
    return processes


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: nix-resources.py OUTPUT.json")
    output = Path(sys.argv[1])
    output.parent.mkdir(parents=True, exist_ok=True)
    report = {
        "observed_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "run_id": os.environ.get("GITHUB_RUN_ID"),
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "event_commit": os.environ.get("GITHUB_SHA"),
        "checkout": command(["git", "rev-parse", "HEAD"]),
        "kernel": list(os.uname()),
        "logical_cpus": os.cpu_count(),
        "memory": read("/proc/meminfo"),
        "virtual_memory_counters": read("/proc/vmstat"),
        "memory_pressure": read("/proc/pressure/memory"),
        "disk_blocks": command(["df", "-Pk", ".", "/nix", os.environ.get("RUNNER_TEMP", "/tmp")]),
        "disk_inodes": command(["df", "-Pi", ".", "/nix", os.environ.get("RUNNER_TEMP", "/tmp")]),
        "nix_version": command(["nix", "--version"]),
        # Read only these settings: a complete config dump can carry tokens.
        "requested_max_jobs": command(["nix", "config", "show", "max-jobs", "--max-jobs", "1", "--cores", "1"]),
        "requested_cores": command(["nix", "config", "show", "cores", "--max-jobs", "1", "--cores", "1"]),
        "process_membership": process_membership(),
        "cgroups": cgroups(),
        # A permission error is retained as unavailable evidence. Do not turn
        # missing kernel logs into a claim that no process was killed.
        "kernel_messages": command(["dmesg", "--ctime"]),
        "recorded_build_exit_status": read(output.parent / "exit-status.txt"),
    }
    output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Resource evidence written to {output}")


if __name__ == "__main__":
    main()

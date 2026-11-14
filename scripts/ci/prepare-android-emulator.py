#!/usr/bin/env python3
"""Remove only the observed launcher ANR from the disposable CI emulator.

This runs before production app startup. It never clears app data or logs,
changes WebView, dismisses arbitrary dialogs, or handles the app under test.
The original ownership and production-startup scripts remain authoritative.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time


LAUNCHER = "com.google.android.apps.nexuslauncher"
TOTAL_SECONDS = 30
COMMAND_SECONDS = 5
SETTLE_SECONDS = 10
POLL_SECONDS = 0.5
ANR = "Application Not Responding:"
DISPLAY_HEADER = "WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)"
WINDOW_HEADER = "WINDOW MANAGER WINDOWS (dumpsys window windows)"


class PreparationFailed(Exception):
    pass


def window_condition(displays, windows):
    if displays.splitlines().count(DISPLAY_HEADER) != 1 or windows.splitlines().count(WINDOW_HEADER) != 1:
        raise PreparationFailed("current-window-sources-are-not-recognized")
    focus = [line.strip().removeprefix("mCurrentFocus=") for line in displays.splitlines()
             if line.strip().startswith("mCurrentFocus=")]
    if len(focus) != 1:
        raise PreparationFailed("focus-source-is-not-unique")
    value = focus[0]
    if value != "null" and not re.fullmatch(r"Window\{[0-9a-fA-F]+ u0 [^{}\r\n]+\}", value):
        raise PreparationFailed("focus-source-is-not-recognized")
    # Parse current Window # entries, not surface names, historical focus, or
    # the default dump's saved last-ANR snapshot. Keep both raw sources on disk.
    active = []
    anrs = []
    for line in windows.splitlines():
        if not line.strip().startswith("Window #"):
            continue
        match = re.fullmatch(r"Window #[0-9]+ (Window\{[0-9a-fA-F]+ u([0-9]+) ([^{}\r\n]+)\}):", line.strip())
        if match is None:
            raise PreparationFailed("active-window-is-not-recognized")
        identity, user, title = match.groups()
        if identity in active:
            raise PreparationFailed("active-window-is-not-unique")
        active.append(identity)
        if "Application Error:" in title:
            raise PreparationFailed("application-error-dialog-is-not-approved-for-cleanup")
        if ANR in title:
            if user != "0" or title != ANR + " " + LAUNCHER:
                raise PreparationFailed("anr-is-not-the-approved-launcher")
            anrs.append(identity)
    if not active or (value != "null" and value not in active):
        raise PreparationFailed("focus-does-not-match-the-active-window-list")
    if anrs:
        if len(anrs) != 1 or anrs[0] != value:
            raise PreparationFailed("launcher-anr-is-not-the-current-focus")
        return "launcher-anr"
    return "clear"


class Preparation:
    def __init__(self, out):
        self.out = out
        self.deadline = time.monotonic() + TOTAL_SECONDS
        self.serial = None
        self.report = {"schema": 1, "cleanup_attempted": False, "cleanup_confirmed": False,
                       "launcher": LAUNCHER, "commands": [], "outcome": "failed"}

    def command(self, label, arguments, *, pinned=True, suffix="txt", deadline=None):
        effective_deadline = min(self.deadline, deadline if deadline is not None else self.deadline)
        remaining = effective_deadline - time.monotonic()
        if remaining <= 0:
            raise PreparationFailed("preparation-deadline-exceeded")
        command = ["adb"]
        if pinned:
            if self.serial is None:
                raise PreparationFailed("device-has-not-been-verified")
            command += ["-s", self.serial]
        command += arguments
        entry = {"label": label, "command": command}
        self.report["commands"].append(entry)
        try:
            result = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True,
                                    timeout=min(COMMAND_SECONDS, remaining), check=False)
        except subprocess.TimeoutExpired as error:
            (self.out / (label + "." + suffix)).write_bytes(error.stdout or b"")
            (self.out / (label + ".stderr.txt")).write_bytes(error.stderr or b"")
            entry["result"] = "timeout"
            raise PreparationFailed("adb-command-timed-out") from error
        except OSError as error:
            entry["result"] = "unavailable"
            raise PreparationFailed("adb-command-unavailable") from error
        (self.out / (label + "." + suffix)).write_bytes(result.stdout)
        (self.out / (label + ".stderr.txt")).write_bytes(result.stderr)
        entry["exit_code"] = result.returncode
        if result.returncode != 0:
            raise PreparationFailed("adb-command-failed:" + label)
        if time.monotonic() >= effective_deadline:
            entry["result"] = "deadline-exceeded"
            raise PreparationFailed("adb-command-deadline-exceeded")
        return result.stdout.decode("utf-8", errors="replace")

    def unique_device(self, label):
        listing = self.command(label + "-devices", ["devices"], pinned=False)
        lines = [line.strip() for line in listing.splitlines() if line.strip()]
        if len(lines) != 2 or lines[0] != "List of devices attached":
            raise PreparationFailed("device-list-is-not-unique")
        row = lines[1].split()
        if len(row) != 2 or row[1] != "device" or not re.fullmatch(r"emulator-[0-9]+", row[0]):
            raise PreparationFailed("device-is-not-an-online-emulator")
        selected = self.command(label + "-selected", ["get-serialno"], pinned=False).strip()
        if selected != row[0] or (self.serial is not None and selected != self.serial):
            raise PreparationFailed("selected-device-does-not-match-the-only-emulator")
        self.serial = selected
        self.report["serial"] = selected

    def window_snapshot(self, label, *, deadline=None):
        displays = self.command(label + "-displays", ["shell", "dumpsys", "window", "displays"], deadline=deadline)
        windows = self.command(label + "-windows", ["shell", "dumpsys", "window", "windows"], deadline=deadline)
        return displays, windows

    def capture(self, label):
        snapshot = self.window_snapshot(label)
        self.command(label + "-activity", ["shell", "dumpsys", "activity", "activities"])
        self.command(label + "-screen", ["exec-out", "screencap", "-p"], suffix="png")
        self.command(label + "-logcat", ["logcat", "-d", "-v", "threadtime"])
        return snapshot

    def run(self):
        if os.environ.get("GITHUB_ACTIONS") != "true":
            raise PreparationFailed("cleanup-is-limited-to-the-disposable-ci-emulator")
        self.unique_device("initial")
        observed = {}
        for name, prop in [("qemu", "ro.kernel.qemu"), ("abi", "ro.product.cpu.abi"),
                           ("api", "ro.build.version.sdk")]:
            observed[name] = self.command(name, ["shell", "getprop", prop]).strip()
        observed["user"] = self.command("current-user", ["shell", "am", "get-current-user"]).strip()
        self.report["device"] = observed
        if observed["qemu"] != "1" or observed["abi"] != "x86_64" or observed["api"] not in {"30", "35"} or observed["user"] != "0":
            raise PreparationFailed("device-is-outside-the-existing-ci-matrix")
        initial = window_condition(*self.capture("before"))
        self.report["initial_condition"] = initial
        if initial == "clear":
            final = window_condition(*self.capture("after"))
            if final != "clear":
                raise PreparationFailed("environment-changed-before-app-startup")
            self.report["final_condition"] = final
            self.report["outcome"] = "clear-without-cleanup"
            return

        # Recheck both the unique device and the exact focused window immediately
        # before the only mutation. A different/ambiguous state is never handled.
        self.unique_device("before-cleanup")
        current = self.window_snapshot("before-cleanup")
        if window_condition(*current) != "launcher-anr":
            raise PreparationFailed("launcher-anr-changed-before-cleanup")
        failure = None
        self.report["cleanup_attempted"] = True
        try:
            self.command("launcher-force-stop", ["shell", "am", "force-stop", "--user", "0", LAUNCHER])
            settle_deadline = min(self.deadline, time.monotonic() + SETTLE_SECONDS)
            poll = 0
            while True:
                poll += 1
                current = self.window_snapshot(f"settle-{poll:02d}", deadline=settle_deadline)
                if time.monotonic() >= settle_deadline:
                    raise PreparationFailed("launcher-anr-settle-deadline-exceeded")
                if window_condition(*current) == "clear":
                    break
                remaining = settle_deadline - time.monotonic()
                if remaining <= 0:
                    raise PreparationFailed("launcher-anr-did-not-disappear")
                time.sleep(min(POLL_SECONDS, remaining))
        except PreparationFailed as error:
            failure = error
        # Preserve the post-action state even when force-stop or confirmation
        # failed. Diagnostic failure never changes an earlier failure to success.
        try:
            final = window_condition(*self.capture("after"))
            self.report["final_condition"] = final
            if final != "clear":
                raise PreparationFailed("launcher-anr-did-not-disappear")
        except PreparationFailed as error:
            if failure is None:
                failure = error
        if failure is not None:
            raise failure
        self.report["cleanup_confirmed"] = True
        self.report["outcome"] = "approved-launcher-anr-removed"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("apk", type=Path)
    args = parser.parse_args()
    # Keep missing/ambiguous APKs ahead of any emulator side effect. The original
    # smoke script independently retains its exact argument and install checks.
    if not args.apk.is_file():
        parser.error("exactly one existing APK is required before emulator preparation")
    try:
        args.out.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        parser.error("use a fresh preparation output directory")
    preparation = Preparation(args.out)
    result = 1
    try:
        preparation.run()
        result = 0
    except PreparationFailed as error:
        preparation.report["failure"] = str(error)
        print("emulator-preparation: " + str(error), file=sys.stderr)
    finally:
        preparation.report["exit_code"] = result
        (args.out / "report.json").write_text(json.dumps(preparation.report, indent=2) + "\n", encoding="utf-8")
        (args.out / "runner-exit.txt").write_text(str(result) + "\n", encoding="utf-8")
    if result == 0:
        print("emulator-preparation: " + preparation.report["outcome"])
    return result


if __name__ == "__main__":
    raise SystemExit(main())

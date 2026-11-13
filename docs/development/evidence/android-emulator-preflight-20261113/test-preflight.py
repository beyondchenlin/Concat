#!/usr/bin/env python3
"""Exercise real preflight and wrapper processes against an explicit fake adb.

These are command-contract tests, not emulator or production-startup results.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


REPO = Path(__file__).resolve().parents[4]
LAUNCHER = "com.google.android.apps.nexuslauncher"
DISPLAY_HEADER = "WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)"
WINDOW_HEADER = "WINDOW MANAGER WINDOWS (dumpsys window windows)"
CLEAR_ID = "Window{a1b2 u0 com.google.android.apps.nexuslauncher/com.google.android.apps.nexuslauncher.NexusLauncherActivity}"
ANR_ID = "Window{e5830d0 u0 Application Not Responding: " + LAUNCHER + "}"


def snapshot(focus, active=None, decoration=""):
    if active is None:
        active = [focus]
    return {"displays": DISPLAY_HEADER + "\n  Display: mDisplayId=0\n  mCurrentFocus=" + focus + "\n",
            "windows": WINDOW_HEADER + "\n" + "".join(
                f"  Window #{i} {identity}:\n    mHasSurface=true\n" for i, identity in enumerate(active)) + decoration}


CLEAR = snapshot(CLEAR_ID)
ANR = snapshot(ANR_ID)
OWN_ANR = snapshot(ANR_ID.replace(LAUNCHER, "app.concat.editor"))
OTHER_ANR = snapshot(ANR_ID.replace(LAUNCHER, "com.example.other"))
DEVICE = "List of devices attached\nemulator-5554\tdevice\n"
FAKE_ADB = r'''#!/usr/bin/env python3
import json, os, pathlib, sys, time
config = json.loads(pathlib.Path(os.environ["FAKE_ADB_CONFIG"]).read_text())
state_path = pathlib.Path(os.environ["FAKE_ADB_STATE"])
state = json.loads(state_path.read_text()) if state_path.exists() else {}
raw = sys.argv[1:]
with open(os.environ["ADB_TRACE"], "a") as output:
    output.write(json.dumps(raw) + "\n")
args = raw
if args[:1] == ["-s"]:
    if args[1] != "emulator-5554": sys.exit(91)
    args = args[2:]
elif args not in [["devices"], ["get-serialno"]]:
    sys.exit(92)
if config.get("timeout") == args:
    time.sleep(6)
if config.get("fail") == args:
    print("injected command failure", file=sys.stderr)
    sys.exit(7)
def sequence(key, default):
    values = config.get(key, [default])
    index = state.get(key, 0)
    state[key] = index + 1
    state_path.write_text(json.dumps(state))
    return values[min(index, len(values) - 1)]
if args == ["devices"]:
    print(sequence("devices", "List of devices attached\nemulator-5554\tdevice\n"), end="")
elif args == ["get-serialno"]:
    print(config.get("selected", "emulator-5554"))
elif args[:2] == ["shell", "getprop"]:
    print(config.get("properties", {}).get(args[2], {
        "ro.kernel.qemu": "1", "ro.product.cpu.abi": "x86_64", "ro.build.version.sdk": "35"
    }.get(args[2], "unknown")))
elif args == ["shell", "am", "get-current-user"]:
    print(config.get("user", "0"))
elif args[:3] == ["shell", "dumpsys", "window"] and args[3:] in [["displays"], ["windows"]]:
    index = state.get("snapshot_index", 0)
    snapshots = config["snapshots"]
    time.sleep(config.get("window_delays", {}).get(str(index) + ":" + args[3], 0))
    print(snapshots[min(index, len(snapshots) - 1)][args[3]], end="")
    if args[3] == "windows":
        state["snapshot_index"] = index + 1
        state_path.write_text(json.dumps(state))
elif args == ["shell", "dumpsys", "activity", "activities"]:
    print("fixture activity; not an actual Android observation")
elif args == ["exec-out", "screencap", "-p"]:
    sys.stdout.buffer.write(b"fixture screenshot; not a real image")
elif args == ["logcat", "-d", "-v", "threadtime"]:
    print("fixture device log; no native recovery is asserted")
elif args == ["shell", "am", "force-stop", "--user", "0", "com.google.android.apps.nexuslauncher"]:
    sys.exit(config.get("force_exit", 0))
else:
    print("unreviewed fake command: " + repr(raw), file=sys.stderr)
    sys.exit(99)
'''
STAGE = r'''#!/usr/bin/env bash
set -euo pipefail
python3 - "STAGE_NAME" "$@" <<'PY'
import json, os, sys
with open(os.environ["STAGE_TRACE"], "a") as f:
    f.write(json.dumps({"stage":sys.argv[1], "args":sys.argv[2:],
        "source":os.environ.get("BUILD_SOURCE_COMMIT"), "serial":os.environ.get("ANDROID_SERIAL"),
        "sdk":os.environ.get("ANDROID_SDK_ROOT"), "ndk":os.environ.get("ANDROID_NDK_HOME")}) + "\n")
PY
exit STAGE_EXIT
'''


class Contracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bk-cat-emulator-preflight-")
        self.root = Path(self.temp.name) / "source with spaces"
        self.ci = self.root / "scripts/ci"
        self.ci.mkdir(parents=True)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        adb = self.bin / "adb"
        adb.write_text(FAKE_ADB)
        adb.chmod(0o755)
        self.apk = self.root / "apk/Concat-proof.apk"
        self.apk.parent.mkdir()
        self.apk.write_bytes(b"explicit fixture; no APK is installed")
        self.config = self.root / "fake.json"
        self.state = self.root / "fake-state.json"
        self.trace = self.root / "adb-trace.jsonl"
        self.stage_trace = self.root / "stages.jsonl"
        self.sequence = 0
        for name in ["prepare-android-emulator.py", "verify-agent-android.sh"]:
            shutil.copyfile(REPO / "scripts/ci" / name, self.ci / name)
        self.env = os.environ | {"PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
            "GITHUB_ACTIONS":"true", "FAKE_ADB_CONFIG":str(self.config), "FAKE_ADB_STATE":str(self.state),
            "ADB_TRACE":str(self.trace), "STAGE_TRACE":str(self.stage_trace),
            "BUILD_SOURCE_COMMIT":"exact-source", "ANDROID_SERIAL":"emulator-5554",
            "ANDROID_SDK_ROOT":"/fixed/sdk", "ANDROID_NDK_HOME":"/fixed/ndk"}

    def tearDown(self):
        self.temp.cleanup()

    def prepare(self, config=None):
        self.config.write_text(json.dumps({"snapshots":[CLEAR]} | (config or {})))
        for path in [self.state, self.trace, self.stage_trace]:
            path.unlink(missing_ok=True)

    def helper(self, config=None, *, env=None, apks=None):
        self.prepare(config)
        self.sequence += 1
        self.out = self.root / f"result-{self.sequence}"
        return subprocess.run([sys.executable, str(self.ci / "prepare-android-emulator.py"),
            "--out", str(self.out), *(apks if apks is not None else [str(self.apk)])],
            env=env or self.env, capture_output=True, text=True, timeout=35)

    def calls(self):
        return [json.loads(line) for line in self.trace.read_text().splitlines()] if self.trace.exists() else []

    def mutations(self):
        return [call for call in self.calls() if "force-stop" in call]

    def report(self):
        return json.loads((self.out / "report.json").read_text())

    def stage(self, filename, name, result):
        (self.ci / filename).write_text(STAGE.replace("STAGE_NAME", name).replace("STAGE_EXIT", str(result)))

    def wrapper(self, *, owner=0, smoke=0, config=None):
        self.prepare(config)
        self.stage("test-agent-android.sh", "ownership", owner)
        self.stage("smoke-android.sh", "smoke", smoke)
        return subprocess.run(["/bin/sh", "-c", "bash scripts/ci/verify-agent-android.sh"],
            cwd=self.root, env=self.env, capture_output=True, text=True, timeout=35)

    def stages(self):
        return [json.loads(line) for line in self.stage_trace.read_text().splitlines()] if self.stage_trace.exists() else []

    def test_clear_environment_has_no_mutation_and_retains_both_snapshots(self):
        result = self.helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.report()["outcome"], "clear-without-cleanup")
        for label in ["before", "after"]:
            for suffix in ["displays.txt", "windows.txt", "activity.txt", "screen.png", "logcat.txt"]:
                self.assertTrue((self.out / (label + "-" + suffix)).is_file())

    def test_exact_launcher_anr_gets_one_fixed_cleanup_and_verified_clear(self):
        result = self.helper({"snapshots":[ANR, ANR, CLEAR, CLEAR]})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.mutations(), [["-s", "emulator-5554", "shell", "am", "force-stop", "--user", "0", LAUNCHER]])
        self.assertTrue(self.report()["cleanup_confirmed"])
        self.assertIn("Application Not Responding", (self.out / "before-windows.txt").read_text())
        self.assertEqual((self.out / "after-windows.txt").read_text(), CLEAR["windows"])

    def test_current_window_entries_accept_surface_decoration_but_ignore_stale_references(self):
        # Official window dumps include decorated surface names and stale window
        # references. Neither is an additional currently registered Window #.
        decoration = ("    mSurface=Surface(name=Application Not Responding: " + LAUNCHER + ")/@0x1234\n"
                      "    mWinAnimator=WindowStateAnimator{cafe Application Not Responding: " + LAUNCHER + "}\n")
        decorated = snapshot(ANR_ID, decoration=decoration)
        after = snapshot(CLEAR_ID, decoration="  mLastWakeLockHoldingWindow=" + ANR_ID + "\n" + decoration)
        result = self.helper({"snapshots":[decorated, decorated, after, after]})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.mutations()), 1)
        self.assertTrue(self.report()["cleanup_confirmed"])
        self.assertIn("Surface(name=", (self.out / "before-windows.txt").read_text())

    def test_missing_or_ambiguous_active_window_sources_fail_without_cleanup(self):
        for state in [ANR | {"windows":""}, ANR | {"windows":ANR["windows"] + WINDOW_HEADER + "\n"},
                      snapshot(ANR_ID, []), snapshot(ANR_ID, [CLEAR_ID]),
                      snapshot(ANR_ID, [ANR_ID, ANR_ID]),
                      snapshot(ANR_ID, [ANR_ID, ANR_ID.replace("e5830d0", "deadbeef")]),
                      ANR | {"windows":WINDOW_HEADER + "\n  Window #unrecognized\n"},
                      snapshot(ANR_ID.replace(LAUNCHER, LAUNCHER + ".unknown"))]:
            with self.subTest(state=state):
                result = self.helper({"snapshots":[state]})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.mutations(), [])

    def test_app_and_unknown_anrs_or_crash_dialogs_fail_without_cleanup(self):
        for window in [OWN_ANR, OTHER_ANR,
                       snapshot(ANR_ID, [ANR_ID, "Window{a11 u0 Application Not Responding: app.concat.editor}"]),
                       snapshot(CLEAR_ID, [CLEAR_ID, "Window{a11 u0 Application Error: app.concat.editor}"])]:
            with self.subTest(window=window):
                result = self.helper({"snapshots":[window]})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.mutations(), [])

    def test_known_launcher_anr_away_from_current_focus_is_not_cleaned(self):
        result = self.helper({"snapshots":[snapshot(CLEAR_ID, [CLEAR_ID, ANR_ID])]})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.mutations(), [])

    def test_missing_multiple_malformed_or_nondefault_user_focus_fail_closed(self):
        for window in [CLEAR | {"displays": DISPLAY_HEADER + "\n"},
                       CLEAR | {"displays": CLEAR["displays"] + "  mCurrentFocus=" + CLEAR_ID + "\n"},
                       snapshot("unparsed"), snapshot(ANR_ID.replace("u0", "u10"))]:
            with self.subTest(window=window):
                result = self.helper({"snapshots":[window]})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.mutations(), [])

    def test_physical_absent_multiple_or_offline_devices_never_receive_cleanup(self):
        for devices in ["List of devices attached\nphysical-phone\tdevice\n", "List of devices attached\n",
                        DEVICE + "emulator-5556\tdevice\n", "List of devices attached\nemulator-5554\toffline\n"]:
            with self.subTest(devices=devices):
                result = self.helper({"devices":[devices], "snapshots":[ANR]})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.calls(), [["devices"]])

    def test_selector_properties_and_user_must_match_existing_matrix(self):
        for config in [{"selected":"emulator-5556"}, {"properties":{"ro.kernel.qemu":"0"}},
                       {"properties":{"ro.product.cpu.abi":"arm64-v8a"}},
                       {"properties":{"ro.build.version.sdk":"34"}}, {"user":"10"}]:
            with self.subTest(config=config):
                result = self.helper(config | {"snapshots":[ANR]})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.mutations(), [])

    def test_non_ci_invocation_has_no_device_calls(self):
        result = self.helper({"snapshots":[ANR]}, env=self.env | {"GITHUB_ACTIONS":"false"})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_device_or_focus_change_before_cleanup_prevents_mutation(self):
        for config in [{"devices":[DEVICE, DEVICE + "emulator-5556\tdevice\n"], "snapshots":[ANR]},
                       {"snapshots":[ANR, OTHER_ANR]}, {"snapshots":[ANR, CLEAR]}]:
            with self.subTest(config=config):
                result = self.helper(config)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.mutations(), [])

    def test_failed_cleanup_keeps_failure_and_post_action_evidence(self):
        result = self.helper({"snapshots":[ANR], "force_exit":7})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.mutations()), 1)
        self.assertFalse(self.report()["cleanup_confirmed"])
        self.assertTrue((self.out / "after-windows.txt").is_file())

    def test_persistent_launcher_anr_is_bounded_and_not_repeatedly_stopped(self):
        start = time.monotonic()
        result = self.helper({"snapshots":[ANR]})
        self.assertLess(time.monotonic() - start, 30)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.mutations()), 1)
        self.assertFalse(self.report()["cleanup_confirmed"])

    def test_late_clear_result_cannot_exceed_the_fixed_settle_deadline(self):
        # The second poll would report clear only after the fixed ten seconds.
        # Real subprocess timeouts must reject it, even if post-failure evidence
        # later observes a clear environment.
        result = self.helper({"snapshots":[ANR, ANR, ANR, CLEAR],
                              "window_delays":{"2:displays":3.5, "2:windows":4.5, "3:displays":2.0}})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.mutations()), 1)
        report = self.report()
        self.assertFalse(report["cleanup_confirmed"])
        self.assertTrue(any(command["label"].startswith("settle-") and command.get("result") == "timeout"
                            for command in report["commands"]), report)
        self.assertEqual(report["final_condition"], "clear")

    def test_unknown_anr_after_cleanup_is_not_hidden_or_cleaned(self):
        result = self.helper({"snapshots":[ANR, ANR, OTHER_ANR, OTHER_ANR]})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.mutations()), 1)
        self.assertFalse(self.report()["cleanup_confirmed"])

    def test_read_failure_or_timeout_prevents_cleanup(self):
        for key in ["fail", "timeout"]:
            with self.subTest(kind=key):
                result = self.helper({key:["shell", "dumpsys", "window", "displays"], "snapshots":[ANR]})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.mutations(), [])

    def test_missing_or_multiple_apks_are_rejected_before_device_access(self):
        for apks in [[str(self.root / "missing.apk")], [str(self.apk), str(self.apk)]]:
            with self.subTest(apks=apks):
                result = self.helper(apks=apks)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(self.calls(), [])

    def test_wrapper_preserves_arguments_environment_and_real_stage_order(self):
        result = self.wrapper(config={"snapshots":[ANR, ANR, CLEAR, CLEAR]})
        self.assertEqual(result.returncode, 0, result.stderr)
        records = self.stages()
        self.assertEqual([x["stage"] for x in records], ["ownership", "smoke"])
        self.assertEqual(records[0]["args"], ["run", "--artifact", "agent-ownership-x86_64", "--out", "smoke-android/ownership"])
        self.assertEqual(records[1]["args"], ["--out", "smoke-android", "--wait", "60", "--require-agent", "apk/Concat-proof.apk"])
        for record in records:
            self.assertEqual((record["source"], record["serial"], record["sdk"], record["ndk"]),
                             ("exact-source", "emulator-5554", "/fixed/sdk", "/fixed/ndk"))
        self.assertTrue(json.loads((self.root / "smoke-android/environment/report.json").read_text())["cleanup_confirmed"])

    def test_wrapper_ownership_failure_prevents_environment_and_app_start(self):
        result = self.wrapper(owner=37, config={"snapshots":[ANR]})
        self.assertEqual(result.returncode, 37)
        self.assertEqual(self.calls(), [])
        self.assertEqual([x["stage"] for x in self.stages()], ["ownership"])

    def test_wrapper_environment_failure_prevents_app_start(self):
        result = self.wrapper(config={"snapshots":[OWN_ANR]})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.mutations(), [])
        self.assertEqual([x["stage"] for x in self.stages()], ["ownership"])

    def test_wrapper_production_failure_is_propagated(self):
        result = self.wrapper(smoke=42)
        self.assertEqual(result.returncode, 42)

    def test_original_ownership_manifest_rejection_still_precedes_environment(self):
        self.prepare({"snapshots":[ANR]})
        shutil.copyfile(REPO / "scripts/ci/test-agent-android.sh", self.ci / "test-agent-android.sh")
        self.stage("smoke-android.sh", "smoke", 0)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True, capture_output=True)
        subprocess.run(["git", "-C", str(self.root), "-c", "user.name=Fixture", "-c",
                        "user.email=fixture@example.invalid", "commit", "--allow-empty", "-qm", "fixture"],
                       check=True, capture_output=True)
        artifact = self.root / "agent-ownership-x86_64"
        artifact.mkdir()
        (artifact / "ownership-tests").write_bytes(b"not executed")
        (artifact / "manifest.json").write_text(json.dumps({"schema":1,"source_sha":"0" * 40,
            "target":"x86_64-linux-android","ndk_api":26,"binary":"ownership-tests"}))
        result = subprocess.run(["/bin/sh", "-c", "bash scripts/ci/verify-agent-android.sh"],
            cwd=self.root, env=self.env, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifest does not match this source and Android target", result.stderr)
        self.assertEqual(self.calls(), [])
        self.assertEqual(self.stages(), [])


if __name__ == "__main__":
    print("Command fixtures only: no real emulator, APK installation or native recovery is asserted.", flush=True)
    unittest.main(verbosity=2)

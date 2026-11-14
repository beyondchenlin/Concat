#!/usr/bin/env python3
"""Exercise the actual workflow command through POSIX sh, with bounded fixtures."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[3] / "scripts" / "ci"
ENTRY = "bash scripts/ci/verify-agent-android.sh"
EXPECTED_SHA = "1462a55367091ee49d17541cae2131a67a22fa1237d4bcaa39bc4d50951ac944"
OWNER_ARGS = ["run", "--artifact", "agent-ownership-x86_64", "--out", "smoke-android/ownership"]
SMOKE_ARGS = ["--out", "smoke-android", "--wait", "60", "--require-agent", "apk/Concat-proof.apk"]
STUB = """#!/usr/bin/env bash
set -euo pipefail
python3 - "$STAGE" "$@" <<'PY'
import json, os, sys
with open(os.environ["TRACE"], "a", encoding="utf-8") as f:
    f.write(json.dumps({"stage": sys.argv[1], "args": sys.argv[2:],
        "source": os.environ.get("BUILD_SOURCE_COMMIT"),
        "serial": os.environ.get("ANDROID_SERIAL"),
        "sdk": os.environ.get("ANDROID_SDK_ROOT"),
        "ndk": os.environ.get("ANDROID_NDK_HOME")}) + "\\n")
PY
exit "$STAGE_EXIT"
"""

class EntryContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="bk-cat-entry-test-")
        self.root = Path(self.tmp.name) / "source with spaces"
        self.ci = self.root / "scripts/ci"
        self.ci.mkdir(parents=True)
        (self.root / "apk").mkdir()
        (self.root / "apk/Concat-proof.apk").touch()
        self.trace = self.root / "trace.jsonl"
        shutil.copyfile(SOURCE / "verify-agent-android.sh", self.ci / "verify-agent-android.sh")
        self.env = os.environ | {
            "TRACE": str(self.trace), "BUILD_SOURCE_COMMIT": "exact-source-commit",
            "ANDROID_SERIAL": "emulator-5554", "ANDROID_SDK_ROOT": "/fixed/sdk",
            "ANDROID_NDK_HOME": "/fixed/ndk"
        }

    def tearDown(self):
        self.tmp.cleanup()

    def stub(self, name, stage, result):
        source = STUB.replace('python3 - "$STAGE"', 'python3 - "' + stage + '"')
        source = source.replace('exit "$STAGE_EXIT"', "exit " + str(result))
        (self.ci / name).write_text(source, encoding="utf-8")

    def run_entry(self, command=ENTRY, cwd=None):
        return subprocess.run(["/bin/sh", "-c", command],
            cwd=cwd or self.root, env=self.env, capture_output=True, text=True, timeout=10)

    def records(self):
        return [json.loads(x) for x in self.trace.read_text().splitlines()] if self.trace.exists() else []

    def test_source_matches_reviewed_remote_bytes(self):
        self.assertEqual(hashlib.sha256((SOURCE / "verify-agent-android.sh").read_bytes()).hexdigest(), EXPECTED_SHA)

    def test_single_sh_entry_runs_stages_in_order_with_exact_arguments_and_environment(self):
        self.stub("test-agent-android.sh", "ownership", 0)
        self.stub("smoke-android.sh", "smoke", 0)
        r = self.run_entry()
        self.assertEqual(r.returncode, 0, r.stderr)
        records = self.records()
        self.assertEqual([x["stage"] for x in records], ["ownership", "smoke"])
        self.assertEqual(records[0]["args"], OWNER_ARGS)
        self.assertEqual(records[1]["args"], SMOKE_ARGS)
        for x in records:
            self.assertEqual((x["source"], x["serial"], x["sdk"], x["ndk"]),
                ("exact-source-commit", "emulator-5554", "/fixed/sdk", "/fixed/ndk"))

    def test_ownership_failure_prevents_startup_and_preserves_exit(self):
        self.stub("test-agent-android.sh", "ownership", 37)
        self.stub("smoke-android.sh", "smoke", 0)
        r = self.run_entry()
        self.assertEqual(r.returncode, 37, r.stderr)
        self.assertEqual([x["stage"] for x in self.records()], ["ownership"])

    def test_startup_failure_is_not_masked(self):
        self.stub("test-agent-android.sh", "ownership", 0)
        self.stub("smoke-android.sh", "smoke", 42)
        r = self.run_entry()
        self.assertEqual(r.returncode, 42, r.stderr)

    def test_real_startup_script_rejects_missing_apk_before_adb(self):
        self.stub("test-agent-android.sh", "ownership", 0)
        shutil.copyfile(SOURCE / "smoke-android.sh", self.ci / "smoke-android.sh")
        (self.root / "apk/Concat-proof.apk").unlink()
        r = self.run_entry()
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertIn("smoke: no APK", r.stderr)

    def test_real_startup_script_rejects_ambiguous_apks_before_adb(self):
        self.stub("test-agent-android.sh", "ownership", 0)
        shutil.copyfile(SOURCE / "smoke-android.sh", self.ci / "smoke-android.sh")
        (self.root / "apk/Concat-second.apk").touch()
        r = self.run_entry()
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertIn("usage:", r.stderr)

    def test_real_ownership_script_rejects_wrong_source_before_startup(self):
        shutil.copyfile(SOURCE / "test-agent-android.sh", self.ci / "test-agent-android.sh")
        self.stub("smoke-android.sh", "smoke", 0)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True, capture_output=True)
        subprocess.run(["git", "-C", str(self.root), "-c", "user.name=Entry Test",
            "-c", "user.email=entry-test@example.invalid", "commit", "--allow-empty",
            "-qm", "isolated entry test"], check=True, capture_output=True)
        artifact = self.root / "agent-ownership-x86_64"
        artifact.mkdir()
        (artifact / "ownership-tests").write_bytes(b"non-executed fixture")
        (artifact / "manifest.json").write_text(json.dumps({
            "schema": 1, "source_sha": "0" * 40, "target": "x86_64-linux-android",
            "ndk_api": 26, "binary": "ownership-tests"}), encoding="utf-8")
        r = self.run_entry()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("manifest does not match this source and Android target", r.stderr)
        self.assertEqual(self.records(), [])
        self.assertEqual((self.root / "smoke-android/ownership/runner-exit.txt").read_text().strip(), str(r.returncode))

if __name__ == "__main__":
    print("This verifies shell orchestration and early fail-closed checks; no emulator or model is invoked.", flush=True)
    unittest.main(verbosity=2)

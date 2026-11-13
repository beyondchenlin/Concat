#!/usr/bin/env bash
# Compile the existing ownership integration tests for Android and execute
# that exact binary on the CI emulator. No mock lock or host-only substitute.
#
# From the repository root:
#   bash scripts/ci/test-agent-android.sh build --out agent-ownership-x86_64
#   bash scripts/ci/test-agent-android.sh run --artifact agent-ownership-x86_64 \
#     --out smoke-android/ownership
#
# Run accepts only an x86_64 emulator at API 30/35. It creates a fresh device
# directory, leaves it for inspection, and never removes any device directory.
set -euo pipefail

usage() {
  echo "usage: $0 build --out DIR | run --artifact DIR --out DIR" >&2
  exit 2
}
fail() { echo "agent-android: $*" >&2; exit 1; }
[ "$#" -gt 0 ] || usage
mode=$1
shift
out=""
artifact=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --out) [ "$#" -ge 2 ] || usage; out=$2; shift 2 ;;
    --artifact) [ "$#" -ge 2 ] || usage; artifact=$2; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$out" ] || usage
case "$mode" in
  build) [ -z "$artifact" ] || usage ;;
  run) [ -n "$artifact" ] || usage ;;
  *) usage ;;
esac
repo=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
trap 'status=$?; printf "%s\n" "$status" > "$out/runner-exit.txt"' EXIT
target=x86_64-linux-android

if [ "$mode" = build ]; then
  [ "$(uname -s)" = Linux ] || fail "build requires the existing Linux Android CI runner"
  # Reuse the exact SDK/NDK and target flags already used by the APK build.
  eval "$("$repo/src/scripts/mobile-env.sh" "$target")"
  ndk_bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
  cc="$ndk_bin/x86_64-linux-android26-clang"
  [ -x "$cc" ] || fail "missing NDK API 26 compiler: $cc"
  [ -x "$ndk_bin/llvm-ar" ] || fail "missing NDK archiver"
  [ -x "$ndk_bin/llvm-readelf" ] || fail "missing NDK ELF inspector"
  # cargo-apk sets these for its own child only. Ordinary cargo test needs
  # explicit target tools; host build scripts must keep their host compiler.
  export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$cc"
  export CC_x86_64_linux_android="$cc"
  export CXX_x86_64_linux_android="$ndk_bin/x86_64-linux-android26-clang++"
  export AR_x86_64_linux_android="$ndk_bin/llvm-ar"
  (
    cd "$repo/src"
    cargo test --locked --release -p concat-agent --target "$target" \
      --test ownership --no-run --message-format=json-render-diagnostics
  ) 2>&1 | tee "$out/build.log"

  python3 - "$repo" "$out" <<'PY'
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

repo, out = map(Path, sys.argv[1:])
executables = set()
for line in (out / "build.log").read_text(encoding="utf-8").splitlines():
    try:
        item = json.loads(line)
    except json.JSONDecodeError:
        continue
    if (item.get("reason") == "compiler-artifact"
            and item.get("target", {}).get("name") == "ownership"
            and "test" in item.get("target", {}).get("kind", [])
            and item.get("profile", {}).get("test") is True
            and item.get("executable")):
        executables.add(Path(item["executable"]).resolve())
if len(executables) != 1:
    raise SystemExit(f"expected one actual Android ownership test binary, got {executables}")
binary = executables.pop()
payload = binary.read_bytes()
if payload[:6] != b"\x7fELF\x02\x01" or int.from_bytes(payload[18:20], "little") != 62:
    raise SystemExit("ownership test is not a 64-bit little-endian x86 ELF")
destination = out / "ownership-tests"
shutil.copy2(binary, destination)
destination.chmod(0o755)
manifest = {
    "schema": 1,
    "source_sha": subprocess.check_output(
        ["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip(),
    "target": "x86_64-linux-android",
    "ndk_api": 26,
    "test_source": "src/crates/concat-agent/tests/ownership.rs",
    "binary": "ownership-tests",
    "sha256": hashlib.sha256(payload).hexdigest(),
    "rustc": subprocess.check_output(
        ["rustc", "--version"], cwd=repo / "src", text=True).strip(),
}
(out / "manifest.json").write_text(
    json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
(out / "SHA256SUMS.txt").write_text(
    manifest["sha256"] + "  ownership-tests\n", encoding="utf-8")
print(json.dumps(manifest, indent=2))
PY
  "$ndk_bin/llvm-readelf" --program-headers "$out/ownership-tests" > "$out/elf-program-headers.txt"
  grep -F -q '/system/bin/linker64' "$out/elf-program-headers.txt" \
    || fail "test executable does not use the Android dynamic linker"
  echo "ANDROID_OWNERSHIP_TEST_BINARY_BUILT; execution is still pending"
  exit 0
fi

[ -f "$artifact/ownership-tests" ] || fail "missing ownership-tests artifact"
artifact=$(cd "$artifact" && pwd)
# A stale test artifact from another source commit cannot validate this run.
python3 - "$repo" "$artifact" "$out" <<'PY'
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

repo, artifact, out = map(Path, sys.argv[1:])
manifest = json.loads((artifact / "manifest.json").read_text(encoding="utf-8"))
source = subprocess.check_output(
    ["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
if (manifest.get("schema") != 1 or manifest.get("source_sha") != source
        or manifest.get("target") != "x86_64-linux-android"
        or manifest.get("ndk_api") != 26 or manifest.get("binary") != "ownership-tests"):
    raise SystemExit("ownership test manifest does not match this source and Android target")
payload = (artifact / "ownership-tests").read_bytes()
if hashlib.sha256(payload).hexdigest() != manifest.get("sha256"):
    raise SystemExit("ownership test artifact hash mismatch")
shutil.copy2(artifact / "manifest.json", out / "manifest.json")
print("Verified Android ownership binary for source " + source)
PY

timeout 60s adb wait-for-device
serial=$(adb get-serialno | tr -d '\r')
qemu=$(adb shell getprop ro.kernel.qemu | tr -d '\r')
abi=$(adb shell getprop ro.product.cpu.abi | tr -d '\r')
api=$(adb shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$serial" == emulator-* && "$qemu" == 1 && "$abi" == x86_64 ]] \
  || fail "refusing any device other than the existing x86_64 Android emulator"
case "$api" in 30|35) ;; *) fail "expected the existing API 30/35 emulator";; esac
device_root=$(adb shell mktemp -d /data/local/tmp/bk-cat-owner-ci.XXXXXXXX | tr -d '\r')
[[ "$device_root" =~ ^/data/local/tmp/bk-cat-owner-ci\.[A-Za-z0-9]+$ ]] \
  || fail "invalid fresh emulator test directory"
printf 'serial=%s\napi=%s\nabi=%s\ndevice_directory=%s\n' \
  "$serial" "$api" "$abi" "$device_root" | tee "$out/device.txt"
adb shell -T "mkdir '$device_root/tmp'"
adb push "$artifact/ownership-tests" "$device_root/ownership-tests"
adb shell -T "chmod 700 '$device_root/ownership-tests'"
# TMPDIR applies only to this test process and its subprocesses. Every test
# creates its own further directory under it; production app data is untouched.
timeout 180s adb shell -T \
  "cd '$device_root' && TMPDIR='$device_root/tmp' ./ownership-tests --test-threads=1 --nocapture" \
  2>&1 | tee "$out/ownership.log"

python3 - "$out/ownership.log" <<'PY'
from pathlib import Path
import re
import sys

text = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
required = [
    "same_store_and_canonical_path_alias_share_one_real_lock",
    "concurrent_first_acquisition_has_one_owner_and_does_not_strand_an_empty_journal",
    "desktop_second_process_is_refused_by_the_operating_system_lock",
    "killed_owner_can_be_taken_over_but_never_automatically_resumes",
    "dropping_the_host_revokes_but_keeps_the_kernel_lock_until_callbacks_drain",
    "a_timed_out_handoff_refuses_new_owners_and_late_callbacks",
    "persistent_generation_increases_without_removing_the_lock_file",
    "mobile_runtime_replacement_overlap_is_fenced_by_native_generation",
    "fast_stop_and_status_do_not_wait_for_an_admitted_write_or_release_ownership",
]
for name in required:
    prefix = r"^test " + re.escape(name) + r" \.\.\."
    if not re.search(prefix, text, re.MULTILINE):
        raise SystemExit("required production ownership test did not run: " + name)
    # Ignored tests print a heading too. Do not count that as execution; a
    # real parent test may instead interleave output with its child process.
    if re.search(prefix + r"[ \t]+ignored\b", text, re.MULTILINE):
        raise SystemExit("required production ownership test was ignored: " + name)
summaries = re.findall(
    r"test result: ok\. (\d+) passed; 0 failed; \d+ ignored; 0 measured; 0 filtered out;",
    text)
if not summaries or int(summaries[-1]) < len(required):
    raise SystemExit("missing complete successful Android ownership test result")
print("ANDROID_OWNERSHIP_TESTS_PASS: " + summaries[-1] + " real tests")
print("Ignored child helper is explicitly executed by the parent-process tests.")
PY

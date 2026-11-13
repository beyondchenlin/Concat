#!/usr/bin/env bash
# The emulator action invokes each input line through /bin/sh. Keep Bash
# options and the required stages behind one explicit Bash entry point.
# Existing source, target, artifact, device and startup checks stay in their
# original scripts; ownership or environment failure prevents the app launch.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo"

bash scripts/ci/test-agent-android.sh run --artifact agent-ownership-x86_64 \
  --out smoke-android/ownership
python3 scripts/ci/prepare-android-emulator.py \
  --out smoke-android/environment apk/Concat-*.apk
exec bash scripts/ci/smoke-android.sh --out smoke-android --wait 60 \
  --require-agent apk/Concat-*.apk

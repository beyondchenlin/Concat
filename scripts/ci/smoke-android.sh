#!/usr/bin/env bash
# Installs the built APK on an Android device or emulator, starts it the
# way the launcher does, and fails if it does not stay up: nothing else in
# the phone build ever runs what it packages.
#
#   smoke-android.sh [--out DIR] [--wait SECS] [--ready REGEX] [--no-install]
#                    [--package NAME] [--export CLIP] [--require-agent] [--activate-agent-ui] APK
#
# On whatever device adb talks to (ANDROID_SERIAL picks one of several):
# waits for it to finish booting; checks the device can run one of the
# ABIs the APK carries (an x86_64 emulator image runs an arm64-only APK
# through its ARM translation, and says so in ro.product.cpu.abilist);
# installs the APK; starts its launcher activity with `am start -W`; and
# then watches for WAIT seconds (45 by default). It fails as soon as
#
#   - the process is gone,
#   - logcat has, from the app, a Java crash (FATAL EXCEPTION), a native
#     one (Fatal signal), a Rust panic (panicked at), an ANR (ANR in), or
#     one of the app's own start-up failures (could not start, an event
#     loop that ended in an error, or a preview that failed: the window's
#     "Preview failed" notice, which the log carries as `preview: ...`,
#     #312),
#
# --require-agent also requires this launch's current process to report
# the production embedded runtime in native recovery state. Any ownership,
# host startup or retirement failure during this foreground observation fails;
# the full wait is retained even when --ready is also supplied.
# --activate-agent-ui explicitly creates a uniquely named empty project and
# opens the real floating assistant using keyboard input. It requires an
# emulator, --require-agent, an installation and at least 60 seconds of final
# observation. Without --export, ordinary smoke runs and --no-install
# never create a project.
#
# and passes if none of that has happened by the end of the wait, with the
# app's own first line ("Concat <version> starting") in the log, so a pass
# means its native code ran. With --ready, it also requires a line matching
# REGEX (an extended regular expression) by the end of the wait, and stops
# waiting once it appears.
#
# One crash is not counted, and only on a device that runs the APK through
# its ARM translation: a SIGSEGV whose crash dump puts the app's library at
# cpuinfo_arm_linux_init. ONNX Runtime's static initialisers detect the
# CPU as the library loads, and under the translation /proc/cpuinfo
# describes the x86_64 host, not an ARM core, which the detection does not
# survive. It happens before any of the app's code runs, so such a device
# cannot test the APK at all: the script says so as a warning, in the job
# summary, and exits 0 without claiming a pass. Any other crash, or this
# one on a device running the APK natively, fails as before. The
# translation before Android 12 leaves the app's frames out of the crash
# dump, and there the crash is recognised by its address and its timing
# instead (see untestable_here).
#
# Whatever the outcome it leaves in DIR (smoke-android by default): the
# whole logcat from before the launch to the end, the app's lines alone,
# a screenshot of the screen at the end, the window focus, and the
# device's properties. CI uploads that folder. The package is
# app.concat.editor unless --package names another.
#
# --export CLIP needs an APK built with concat-android's `smoke` feature
# (never the one that ships). The clip is pushed into the app's files
# folder as smoke-export.<extension>, where that build finds it at start, makes a
# project of it and exports it through the window's own messages
# (concat/src/smoke_export.rs). From the window's first frame on, a key the
# app does nothing with is pressed every two seconds: a key waits on the window's
# thread, so a thread that has stopped answering is an ANR within ten
# seconds, which fails the run as above. It passes on the app's
# "export finished" line, after which the app exits on its own, and fails
# on its "the export run failed" one; WAIT is 300 by default here.
#
# --no-install starts what is already installed, which leaves a device
# somebody is using as it was apart from the restart of the app. Runs in
# CI (mobile.yml) and locally against any device adb can see. Needs adb,
# unzip and perl.
set -euo pipefail

out=smoke-android
wait_secs=45
ready=""
require_agent=0
activate_agent_ui=0
install=1
package=app.concat.editor
export_clip=""
wait_given=""

usage() {
  echo "usage: $0 [--out DIR] [--wait SECS] [--ready REGEX] [--no-install] [--package NAME] [--export CLIP] [--require-agent] [--activate-agent-ui] APK" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --out) out=${2:?}; shift 2 ;;
    --wait) wait_secs=${2:?}; wait_given=1; shift 2 ;;
    --ready) ready=${2:?}; shift 2 ;;
    --no-install) install=0; shift ;;
    --require-agent) require_agent=1; shift ;;
    --activate-agent-ui) activate_agent_ui=1; shift ;;
    --package) package=${2:?}; shift 2 ;;
    --export) export_clip=${2:?}; shift 2 ;;
    -h|--help) usage ;;
    --*) usage ;;
    *) break ;;
  esac
done
[ $# -eq 1 ] || usage
apk=$1
[ -f "$apk" ] || { echo "smoke: no APK at $apk" >&2; exit 2; }
if [ -n "$export_clip" ] && { [ "$require_agent" = 1 ] || [ "$activate_agent_ui" = 1 ]; }; then
  echo "smoke: --export and native agent activation are separate verification runs" >&2
  exit 2
fi
if [ "$activate_agent_ui" = 1 ]; then
  if [ "$require_agent" != 1 ] || [ "$install" != 1 ] || [ "$package" != app.concat.editor ]; then
    echo "smoke: --activate-agent-ui requires --require-agent, installation and the production package; --no-install never creates a project" >&2
    exit 2
  fi
  [[ "$wait_secs" =~ ^[0-9]+$ ]] && [ "$wait_secs" -ge 60 ] \
    || { echo "smoke: UI activation requires at least 60 seconds of final observation" >&2; exit 2; }
  command -v python3 > /dev/null || { echo "smoke: UI activation requires Python 3" >&2; exit 2; }
  [ ! -e "$out/ui-activation" ] \
    || { echo "smoke: use a fresh UI activation evidence directory" >&2; exit 2; }
fi
if [ -n "$export_clip" ]; then
  [ -f "$export_clip" ] || { echo "smoke: no clip at $export_clip" >&2; exit 2; }
  [ -n "$wait_given" ] || wait_secs=300
  # The app's own lines for the export's end, either way.
  ready=' concat +: ([A-Za-z0-9_:]+: )?smoke: export finished'
fi

say() { printf '\n== %s\n' "$*"; }
note() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::warning::$*"; else echo "smoke: $*"; fi
}

mkdir -p "$out"
logcat_file=$out/logcat.txt
logcat_pid=""
collected=""
activation_pid=""
startup_pid=""

# Everything the folder should hold, gathered on the way out whatever the
# outcome: a failure is exactly when the screenshot and the log are wanted.
collect() {
  collected=1
  set +e
  if [ -n "$logcat_pid" ]; then
    kill "$logcat_pid" 2>/dev/null
    wait "$logcat_pid" 2>/dev/null
    logcat_pid=""
  fi
  adb exec-out screencap -p > "$out/screen.png" 2>/dev/null || rm -f "$out/screen.png"
  {
    adb shell dumpsys activity activities 2>/dev/null | grep -i 'ResumedActivity'
    adb shell dumpsys window 2>/dev/null | grep -E 'mCurrentFocus|mFocusedApp'
  } | tr -d '\r' > "$out/focus.txt"
  adb shell getprop > "$out/getprop.txt" 2>/dev/null
  # Tombstones for a native crash, where the device lets them be read.
  adb logcat -d -b crash -v threadtime > "$out/logcat-crash.txt" 2>/dev/null
  if [ -f "$logcat_file" ]; then
    grep -E ' concat +:' "$logcat_file" > "$out/concat.txt" || true
  fi
  set -e
}
# And on a way out nobody wrote a `fail` for - a command that `set -e`
# ends the script on - so the background logcat is stopped and the folder
# still holds what the failure left.
trap '[ -n "$collected" ] || collect' EXIT

fail() {
  collect
  if [ -s "$out/concat.txt" ]; then
    echo "-- the app's last lines --"
    tail -n 60 "$out/concat.txt"
  fi
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*"; else echo "smoke: $*" >&2; fi
  exit 1
}

# A time limit without coreutils, so this runs on a Mac too: perl's alarm,
# which the exec keeps. A command that overstays it dies of SIGALRM, 142.
limit() {
  local secs=$1
  shift
  perl -e 'alarm shift; exec @ARGV or die "exec: $!"' "$secs" "$@"
}

say "the device"
limit 300 adb wait-for-device || fail "no device came up for adb within 300s"
booted=""
for _ in $(seq 1 150); do
  booted=$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r' || true)
  [ "$booted" = 1 ] && break
  sleep 2
done
[ "$booted" = 1 ] || fail "the device did not finish booting within 300s"
sdk=$(adb shell getprop ro.build.version.sdk | tr -d '\r')
release=$(adb shell getprop ro.build.version.release | tr -d '\r')
abilist=$(adb shell getprop ro.product.cpu.abilist | tr -d '\r')
model=$(adb shell getprop ro.product.model | tr -d '\r')
echo "$model, Android $release (API $sdk), ABIs $abilist"
if [ "$activate_agent_ui" = 1 ]; then
  serial=$(adb get-serialno | tr -d '\r')
  qemu=$(adb shell getprop ro.kernel.qemu | tr -d '\r')
  boot_qemu=$(adb shell getprop ro.boot.qemu | tr -d '\r')
  [[ "$serial" =~ ^emulator-[0-9]+$ ]] && { [ "$qemu" = 1 ] || [ "$boot_qemu" = 1 ]; } \
    || fail "explicit UI activation is limited to an emulator; no project was created"
fi

# The ABIs the APK carries native code for, from its lib/ folders. One the
# device lists is one it can load, natively or through its translation.
apk_abis=$(unzip -Z1 "$apk" | sed -n 's#^lib/\([^/]*\)/.*#\1#p' | sort -u | tr '\n' ' ')
echo "the APK carries: ${apk_abis:-no native code}"
runnable=""
for abi in $apk_abis; do
  case ",$abilist," in *",$abi,"*) runnable=$abi; break ;; esac
done
[ -n "$runnable" ] || [ -z "$apk_abis" ] || fail "the device runs $abilist and the APK carries only $apk_abis: an x86_64 emulator image needs ARM translation (API 30+ google_apis or default images have it, ATD images do not)"
primary=${abilist%%,*}
if [ -n "$runnable" ] && [ "$runnable" != "$primary" ]; then
  echo "the app runs as $runnable through the device's translation for $primary"
fi

if [ "$install" = 1 ]; then
  say "install"
  # -g grants every runtime permission the manifest asks for, so no
  # permission prompt stands between the launch and the window.
  limit 300 adb install -r -g "$apk" < /dev/null \
    || fail "adb install failed (an installed build signed with another key has to be uninstalled first)"
fi
adb shell pm path "$package" > /dev/null 2>&1 || fail "$package is not installed"

# The activity the launcher would start, asked of the package manager
# rather than written down: cargo-apk names it, not this script.
activity=$(adb shell cmd package resolve-activity --brief -c android.intent.category.LAUNCHER "$package" 2>/dev/null \
  | tr -d '\r' | tail -n 1)
case "$activity" in
  "$package"/*) ;;
  *) fail "no launcher activity for $package (resolve-activity said: $activity)" ;;
esac

# Startup evidence must come from a new process and a fresh device log.
# An empty pidof response cannot distinguish "absent" from an adb failure,
# so require a successful process listing with the expected header instead.
prepare_agent_start() {
  limit 30 adb shell am force-stop "$package" \
    || fail "could not stop the previous app before the agent startup check"
  local processes remaining
  processes=$(limit 30 adb shell ps -A -o PID,NAME | tr -d '\r') \
    || fail "could not verify that the previous app process exited"
  awk 'NR == 1 { valid = ($1 == "PID" && $2 == "NAME") } END { exit !valid }' <<< "$processes" \
    || fail "the process listing cannot prove that the previous app exited"
  remaining=$(awk -v package="$package" 'NR > 1 && ($2 == package || index($2, package ":") == 1) { print $1 }' <<< "$processes")
  [ -z "$remaining" ] \
    || fail "the previous app process is still present after force-stop"
  limit 30 adb logcat -c \
    || fail "could not clear the previous device log before the agent startup check"
}

# The clip where the smoke build looks for it: the app's own files folder,
# as smoke-export.<extension>. A folder of adb's shell's making is one the
# app may not list, so the folder is the app's, made by starting it once
# when it is not there yet; the file in it, world-readable, it may read.
# Android 11 lets the shell not even look into Android/data; there the
# clip goes into Android/media/<app>, where a file the shell writes
# belongs to the app, and the smoke build looks there too.
if [ -n "$export_clip" ]; then
  say "the clip to export"
  files_dir=/sdcard/Android/data/$package/files
  media_dir=/sdcard/Android/media/$package
  adb shell "rm -f $media_dir/smoke-export.*" < /dev/null > /dev/null 2>&1 || true
  if adb shell "ls /sdcard/Android/data" < /dev/null > /dev/null 2>&1; then
    if ! adb shell "[ -d $files_dir ]" < /dev/null; then
      limit 180 adb shell am start -W -n "$activity" < /dev/null > /dev/null 2>&1 || true
      for _ in $(seq 1 30); do
        adb shell "[ -d $files_dir ]" < /dev/null && break
        sleep 1
      done
      adb shell am force-stop "$package" < /dev/null || true
    fi
    adb shell "[ -d $files_dir ]" < /dev/null || fail "the app never made its files folder, $files_dir"
    adb shell "rm -rf $files_dir/smoke $files_dir/smoke-export.*" < /dev/null
    clip_dir=$files_dir
  else
    echo "the shell may not look into /sdcard/Android/data; the clip goes into $media_dir"
    adb shell "mkdir -p $media_dir" < /dev/null || fail "could not make $media_dir"
    clip_dir=$media_dir
  fi
  ext=${export_clip##*.}
  limit 120 adb push "$export_clip" "$clip_dir/smoke-export.$ext" < /dev/null \
    || fail "could not push $export_clip to $clip_dir"
  adb shell chmod 644 "$clip_dir/smoke-export.$ext" < /dev/null || true
  adb shell ls -l "$clip_dir/smoke-export.$ext" < /dev/null
fi

say "start $activity"
# A cold start: the app stopped, the log emptied and made big enough to
# hold the whole run, and read into a file from before the launch on, so
# the app's first line cannot have scrolled out of the device's buffer by
# the time it is looked for.
adb logcat -G 16M > /dev/null 2>&1 || true
prepare_agent_start
adb logcat -v threadtime > "$logcat_file" 2>&1 &
logcat_pid=$!
started=$(limit 180 adb shell am start -W -n "$activity" 2>&1 | tr -d '\r') \
  || fail "am start did not return within 180s"
printf '%s\n' "$started"

# The lines that mean the start-up failed, wherever they come from: the
# runtime's crash report, the C library's on a fatal signal, Rust's on a
# panic (the app's hook writes the standard message into its own line),
# the system's on an ANR, and the app's own on a start-up it gave up on or
# a window whose event loop ended in an error.
# Only the app's count: a line written by one of its processes, or one
# that names it (the system's ANR report does). An emulator's own services
# crash now and then while it settles, and that is not this app failing.
# The app's lines read "concat  : <module>: <message>".
fatal='FATAL EXCEPTION|Fatal signal [0-9]+|panicked at|ANR in |Preview failed'
fatal+='| concat +: ([A-Za-z0-9_:]+: )?(preview: |could not start|close: event loop exited [(]ok=false[)]|smoke: the export run failed)'
if [ "$require_agent" = 1 ]; then
  fatal+='| concat +: ([A-Za-z0-9_:]+: )?(embedded agent unavailable:|could not install the embedded Android runtime:|bk-cat-agent: state=(failed|stopping|closed))'
fi

# Every process the app has had since the launch: the ones the activity
# manager reports starting, which a process that dies at once still has,
# and the one running now.
# Sets `running` to the process there is now, empty when there is none,
# and adds it to `pids`. Called in this shell, not in $( ): it sets both.
pids=""
running=""
app_pids() {
  # A caller can bound this query with the same deadline as its other queries.
  local adb_command=${1:-adb}
  running=$("$adb_command" shell pidof "$package" 2>/dev/null | tr -d '\r' || true)
  pids=$(
    {
      sed -n "s/.*Start proc \([0-9][0-9]*\):$package\/.*/\1/p" "$logcat_file"
      printf '%s %s\n' "$running" "$pids" | tr ' ' '\n'
    } | grep -E '^[0-9]+$' | sort -u | tr '\n' ' '
  )
}

# The fatal lines that are the app's, from the log so far. threadtime puts
# the writing process's id in the third column.
fatal_lines() {
  awk -v re="$fatal" -v pids=" $pids " -v pkg="$package" '
    $0 ~ re && (index(pids, " " $3 " ") || index($0, pkg)) { print; if (++n == 5) exit }
  ' "$logcat_file"
}

# Only the process currently alive can prove runtime readiness. A marker from
# an earlier process or another application's log cannot satisfy this check.
agent_ready_lines() {
  awk -v pids=" $running " '
    / concat +: ([A-Za-z0-9_:]+: )?bk-cat-agent: state=recovery generation=[1-9][0-9]*[[:space:]]*$/ &&
    index(pids, " " $3 " ") { print }
  ' "$logcat_file"
}

# Ends the script without a verdict when the fatal lines in `found` are the
# translation's crash in ONNX Runtime's CPU detection (see the top of this
# file), and returns otherwise. The crash dump reaches the crash buffer a
# few seconds after the signal, so it is waited for.
untestable_here() {
  [ -n "$runnable" ] && [ "$runnable" != "$primary" ] || return 0
  [ -n "$found" ] || return 0
  ! grep -v -q -E 'Fatal signal 11 ' <<< "$found" || return 0
  local frame="#00 pc [0-9a-f]+ +/data/app/[^ ]*$package-[^ ]*/lib/[^ ]+[.]so [(]cpuinfo_arm_linux_init[+]"
  # The translation before Android 12 writes no frames of the app's own
  # into the crash dump, only the translator's code, which sits in
  # anonymous memory. There the crash is known by everything else it has:
  # every fatal line the same SIGSEGV at address 0x8, the top frame in the
  # translator, and not one line from the app, whose first comes at the
  # start of android_main, after the library has loaded.
  local blind="#00 pc [0-9a-f]+ +<anonymous:[0-9a-f]+>"
  local hit="" crashes=""
  for _ in $(seq 1 10); do
    crashes=$(adb logcat -d -b crash -v threadtime 2>/dev/null || true)
    hit=$(grep -E -m 1 -- "$frame" <<< "$crashes" || true)
    [ -n "$hit" ] && break
    sleep 2
  done
  local seen="its crash dump puts the fault in ONNX Runtime's CPU detection"
  if [ -z "$hit" ]; then
    ! grep -v -q -E 'Fatal signal 11 .*fault addr 0x8 ' <<< "$found" || return 0
    ! grep -E -q ' concat +:' "$logcat_file" || return 0
    hit=$(grep -E -m 1 -- "$blind" <<< "$crashes" || true)
    [ -n "$hit" ] || return 0
    seen="its crash dump names no frame of the app's (this translation writes none), and the crash matches the one the newer translation names: a SIGSEGV at 0x8 as the library loads, before the app's first line"
  fi
  collect
  printf '%s\n' "$hit"
  local why="NOT TESTED on API $sdk: the APK runs here through ARM translation ($runnable on $primary), where ONNX Runtime's CPU detection crashes as the library loads, before any of the app's code; $seen. Start it on a real $runnable device."
  note "$why"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    printf '### Android smoke, API %s: not tested\n\n%s\n' "$sdk" "$why" >> "$GITHUB_STEP_SUMMARY"
  fi
  say "not tested"
  if [ "$require_agent" = 1 ]; then
    fail "required native agent could not be tested on this device; no startup acceptance"
  fi
  exit 0
}

# `am start -W` has its own shorter activity-launch timeout. That is not
# readiness: allow at most 30 additional seconds for this same cold process
# to enter native code and own the foreground. Every query shares one deadline.
startup_adb() {
  local remaining=$((startup_deadline - SECONDS))
  [ "$remaining" -gt 0 ] || return 142
  limit "$remaining" adb "$@"
}

check_startup_process() {
  app_pids startup_adb
  found=$(fatal_lines)
  [ -z "$found" ] || fail "the app failed during native startup: $(head -n 1 <<< "$found")"
  [[ "$running" =~ ^[1-9][0-9]*$ ]] \
    || fail "native startup requires exactly one live app process"
  [ -n "$startup_pid" ] || startup_pid=$running
  [ "$running" = "$startup_pid" ] && [ "$pids" = "$startup_pid " ] \
    || fail "the app process changed during native startup"
}

wait_native_start() {
  local startup_deadline=$((SECONDS + 30))
  say "launch timed out; verify native foreground readiness within 30s"
  while [ "$SECONDS" -lt "$startup_deadline" ]; do
    check_startup_process
    awk -v pid="$startup_pid" '
      $3 == pid && / concat +: ([A-Za-z0-9_:]+: )?Concat .+ starting[[:space:]]*$/ { print }
    ' "$logcat_file" > "$out/startup-native.txt"
    if [ -s "$out/startup-native.txt" ]; then
      startup_adb shell dumpsys window displays | tr -d '\r' > "$out/startup-window.txt" \
        || fail "could not query the foreground window within the native startup deadline"
      startup_adb shell dumpsys activity activities | tr -d '\r' > "$out/startup-activity.txt" \
        || fail "could not query the resumed activity within the native startup deadline"
      if awk -v pkg="$package" '
          /^[[:space:]]*mCurrentFocus=/ {
            n++
            if ($0 ~ /^[[:space:]]*mCurrentFocus=Window\{[^{}[:space:]]+ u[0-9]+ [^{}[:space:]]+\}$/ &&
                index($0, " " pkg "/")) own++
          }
          END { exit !(n == 1 && own == 1) }
        ' "$out/startup-window.txt" && awk -v pkg="$package" '
          /ResumedActivity/ { n++; if (!index($0, " " pkg "/")) other++ }
          END { exit !(n > 0 && other == 0) }
        ' "$out/startup-activity.txt"; then
        # Queries can race a crash/restart. Recheck without renewing the deadline.
        check_startup_process
        [ "$SECONDS" -lt "$startup_deadline" ] \
          || fail "native foreground readiness was not established within 30s"
        printf '%s\n' "$startup_pid" > "$out/startup-pid.txt"
        echo "the same new process entered native code and owns the resumed foreground window"
        return
      fi
    fi
    sleep 1
  done
  fail "native foreground readiness was not established within 30s"
}

# Reject missing/duplicate statuses and explicit launch errors. Only the exact
# timeout status receives extra observation; it never counts as success alone.
if grep -E -q '^Error([[:space:]:]|$)|^Exception' <<< "$started"; then
  fail "am start reported a launch error"
fi
launch_status=$(grep '^Status:' <<< "$started" || true)
case "$launch_status" in
  'Status: ok') ;;
  'Status: timeout') wait_native_start ;;
  *) fail "am start did not report exactly one supported launch status" ;;
esac

if [ "$activate_agent_ui" = 1 ]; then
  say "create a new project and open the production assistant"
  app_pids
  found=$(fatal_lines)
  [ -z "$found" ] || fail "the app failed before UI activation: $(head -n 1 <<< "$found")"
  [[ "$running" =~ ^[1-9][0-9]*$ ]] \
    || fail "UI activation requires exactly one current app process"
  if [ -n "$startup_pid" ] && [ "$running" != "$startup_pid" ]; then
    fail "the app process changed before production UI activation"
  fi
  activation_pid=$running
  python3 "$(dirname "${BASH_SOURCE[0]}")/activate-agent-android.py" \
    --out "$out/ui-activation" --package "$package" --pid "$activation_pid" \
    || fail "production UI activation did not establish a real project and native recovery"
fi

# The full observation starts after explicit activation, never before it.
say "watch for ${wait_secs}s"
seen_ready=""
doing="on start"
[ -z "$export_clip" ] || doing="exporting"
deadline=$((SECONDS + wait_secs))
while [ "$SECONDS" -lt "$deadline" ]; do
  sleep 2
  app_pids
  found=$(fatal_lines)
  if [ -n "$found" ]; then
    printf '%s\n' "$found"
    untestable_here
    fail "the app failed $doing: $(head -n 1 <<< "$found")"
  fi
  # The export's end is the run's: the app exits once it has said so.
  if [ -n "$export_clip" ] && grep -E -q -- "$ready" "$logcat_file"; then
    seen_ready=1
    echo "the export finished: $(grep -E -m 1 -- "$ready" "$logcat_file")"
    break
  fi
  # Left shift, which the window takes and does nothing with: delivered
  # to the window's thread, and waited on by the system for an answer.
  # From the moment the window is up, not from the export's start: a
  # window that freezes as the export starts never says it started.
  if [ -n "$export_clip" ] && [ -n "$running" ] \
    && grep -E -q ' concat +: ([A-Za-z0-9_:]+: )?(start: done|smoke: exporting)' "$logcat_file"; then
    adb shell input keyevent KEYCODE_SHIFT_LEFT < /dev/null > /dev/null 2>&1 || true
  fi
  if [ -z "$running" ]; then
    # The log can trail the death by a moment; the reason is worth the wait.
    gone=$((wait_secs - deadline + SECONDS))
    sleep 3
    found=$(fatal_lines)
    [ -z "$found" ] || printf '%s\n' "$found"
    if [ -z "$found" ] && [ -n "$export_clip" ] && grep -E -q -- "$ready" "$logcat_file"; then
      seen_ready=1
      echo "the export finished: $(grep -E -m 1 -- "$ready" "$logcat_file")"
      break
    fi
    untestable_here
    fail "the app's process is gone ${gone}s after the launch${found:+: $(head -n 1 <<< "$found")}"
  fi
  if [ -n "$activation_pid" ] && [ "$running" != "$activation_pid" ]; then
    fail "the app process changed after production UI activation"
  fi
  if [ -n "$startup_pid" ] && [ "$running" != "$startup_pid" ]; then
    fail "the app process changed after native foreground readiness"
  fi
  if [ -n "$ready" ] && [ -z "$seen_ready" ] && grep -E -q -- "$ready" "$logcat_file"; then
    seen_ready=1
    echo "the ready line is in the log: $(grep -E -m 1 -- "$ready" "$logcat_file")"
    if [ "$require_agent" = 0 ]; then break; fi
  fi
done

say "the outcome"
collect
cat "$out/focus.txt" || true
if [ -n "$export_clip" ]; then
  [ -n "$seen_ready" ] || fail "the export did not finish within ${wait_secs}s"
  echo "-- the export --"
  grep -E ' concat +: ([A-Za-z0-9_:]+: )?(smoke|export): ' "$out/concat.txt" | tail -n 30 || true
  echo
  echo "the app made a project of $(basename "$export_clip") and exported it, answering keys throughout; see $out/"
  say "ok"
  exit 0
fi
# A process can outlive its window: an android_main that returns, on an
# error it logged as anything else, finishes the activity and leaves the
# process cached. The activity in front is the app's, or it did not start.
grep -i 'ResumedActivity' "$out/focus.txt" | grep -q "$package/" \
  || fail "the app's activity is not the one in front after ${wait_secs}s"
# The app's own first line, from android_main: proof its native code ran,
# and not only that a process by its name exists.
grep -E -q ' concat +: ([A-Za-z0-9_:]+: )?Concat .* starting' "$logcat_file" \
  || fail "the app's first line (\"Concat <version> starting\") never reached logcat"
if [ "$require_agent" = 1 ]; then
  # Collection can capture lines after the final polling interval. Recheck
  # failures and the live PID against that complete foreground log.
  app_pids
  [ -n "$running" ] || fail "the app has no current process at the agent startup check"
  if [ -n "$startup_pid" ] && [ "$running" != "$startup_pid" ]; then
    fail "the app process changed before the final native recovery check"
  fi
  if [ -n "$activation_pid" ] && [ "$running" != "$activation_pid" ]; then
    fail "the app process changed before the final native recovery check"
  fi
  found=$(fatal_lines)
  [ -z "$found" ] || fail "native agent startup failed: $(head -n 1 <<< "$found")"
  agent_ready=$(agent_ready_lines)
  [ -n "$agent_ready" ] || fail "the current app process never completed native agent recovery within the observation window"
  printf '%s\n' "$agent_ready" > "$out/agent-recovery.txt"
  echo "the current process completed production native agent recovery; no model call or edit is asserted"
fi
if [ -n "$ready" ] && [ -z "$seen_ready" ]; then
  fail "no line matching '$ready' within ${wait_secs}s"
fi
if ! grep -E 'mCurrentFocus' "$out/focus.txt" | grep -q "$package"; then
  note "the window in focus is not the app's: $(grep -E 'mCurrentFocus' "$out/focus.txt" | tr -s ' ')"
fi
echo "-- the app's lines --"
head -n 40 "$out/concat.txt" || true
echo
echo "the app is up after ${wait_secs}s with nothing fatal in the log; see $out/"
say "ok"

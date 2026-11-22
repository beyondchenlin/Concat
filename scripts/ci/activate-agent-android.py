#!/usr/bin/env python3
"""Open the production Android assistant through its existing native UI.

This opt-in smoke step is for an emulator, including an isolated local one.
It creates a uniquely named empty project through the application's form;
it never writes app files, configuration, credentials or recovery markers.
The caller retains the complete observation period after this step succeeds.
"""

import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import struct
import subprocess
import sys
import time
import uuid
import zlib


TOTAL_SECONDS = 100
COMMAND_SECONDS = 8
POLL_SECONDS = 0.5
LOG_LINE = re.compile(
    r"^\d\d-\d\d\s+\d\d:\d\d:\d\d\.\d+\s+(\d+)\s+\d+\s+[VDIWEF]\s+concat\s*:\s*(.*)$"
)
RECOVERY = re.compile(
    r"^(?:[A-Za-z0-9_:]+:\s+)?bk-cat-agent: state=recovery generation=[1-9][0-9]*\s*$"
)
COMPOSE = re.compile(r"^(?:[A-Za-z0-9_:]+:\s+)?start:\s+Compose\s*$")
FAILURE = re.compile(
    r"panicked at|could not start|Preview failed|preview: |"
    r"embedded agent unavailable:|could not install the embedded Android runtime:|"
    r"bk-cat-agent: state=(?:failed|stopping|closed)|close: event loop exited"
)


class ActivationFailed(Exception):
    pass


def check_deadline(deadline):
    if deadline is not None and time.monotonic() >= deadline:
        raise ActivationFailed("activation-deadline-exceeded")


class Screenshot:
    def __init__(self, width, height, channels, pixels):
        self.width, self.height, self.channels = width, height, channels
        self.pixels = pixels

    def pixel(self, x, y):
        offset = (y * self.width + x) * self.channels
        value = self.pixels[offset:offset + self.channels]
        return (*value[:3], value[3] if self.channels == 4 else 255)


def decode_png(payload, deadline=None):
    """Bounded, non-interlaced 8-bit RGB/RGBA screencap reader; no image library.

    Validate CRCs, ordering and the exact decompressed size before allocating
    pixels. Unknown critical chunks, transparency tables and trailing data are
    rejected rather than interpreted as an opaque screenshot.
    """
    check_deadline(deadline)
    if (len(payload) > 32 * 1024 * 1024
            or not payload.startswith(b"\x89PNG\r\n\x1a\n")):
        raise ActivationFailed("screenshot-invalid-png-size-or-signature")
    offset, header, phase, ended = 8, None, 0, False
    compressed = bytearray()
    while offset < len(payload):
        if offset + 12 > len(payload):
            raise ActivationFailed("screenshot-truncated-png-chunk")
        length = struct.unpack_from(">I", payload, offset)[0]
        kind = payload[offset + 4:offset + 8]
        end = offset + 12 + length
        if end > len(payload):
            raise ActivationFailed("screenshot-truncated-png-chunk")
        if not re.fullmatch(rb"[A-Za-z]{4}", kind) or kind[2] & 32:
            raise ActivationFailed("screenshot-invalid-png-chunk-type")
        data = memoryview(payload)[offset + 8:end - 4]
        crc = struct.unpack_from(">I", payload, end - 4)[0]
        if zlib.crc32(data, zlib.crc32(kind)) != crc:
            raise ActivationFailed("screenshot-png-crc-mismatch")
        if header is None and kind != b"IHDR":
            raise ActivationFailed("screenshot-png-header-not-first")
        if kind == b"IHDR":
            if header is not None or length != 13:
                raise ActivationFailed("screenshot-invalid-png-header")
            width, height, depth, color, compression, filtering, interlace = struct.unpack(
                ">IIBBBBB", data)
            if (not 1 <= width <= 4096 or not 1 <= height <= 8192
                    or width * height > 12_000_000):
                raise ActivationFailed("screenshot-png-dimensions-exceed-limit")
            if depth != 8 or color not in (2, 6) or compression or filtering or interlace:
                raise ActivationFailed("screenshot-unsupported-png-format")
            header = (width, height, 3 if color == 2 else 4)
        elif kind == b"IDAT":
            if phase == 2:
                raise ActivationFailed("screenshot-noncontiguous-png-data")
            phase = 1
            compressed.extend(data)
        elif kind == b"IEND":
            if length or not compressed or end != len(payload):
                raise ActivationFailed("screenshot-invalid-png-end")
            ended = True
            break
        else:
            if kind == b"tRNS" or (not kind[0] & 32 and kind != b"PLTE"):
                raise ActivationFailed("screenshot-unsupported-png-chunk")
            if kind == b"PLTE" and (phase or not 0 < length <= 768 or length % 3):
                raise ActivationFailed("screenshot-invalid-png-palette")
            if phase == 1:
                phase = 2
        offset = end
    if not ended or header is None:
        raise ActivationFailed("screenshot-incomplete-png")
    width, height, channels = header
    stride = width * channels
    expected = (stride + 1) * height
    try:
        inflater = zlib.decompressobj()
        raw = inflater.decompress(compressed, expected + 1)
    except zlib.error as error:
        raise ActivationFailed("screenshot-invalid-png-compression") from error
    if (len(raw) != expected or not inflater.eof or inflater.unused_data
            or inflater.unconsumed_tail):
        raise ActivationFailed("screenshot-png-decompressed-size-mismatch")
    pixels = bytearray(stride * height)
    for y in range(height):
        if y % 32 == 0:
            check_deadline(deadline)
        start = y * (stride + 1)
        filtering = raw[start]
        if filtering > 4:
            raise ActivationFailed("screenshot-unsupported-png-row-filter")
        row = bytearray(raw[start + 1:start + 1 + stride])
        prior = pixels[(y - 1) * stride:y * stride] if y else bytes(stride)
        if filtering:
            for x in range(stride):
                left = row[x - channels] if x >= channels else 0
                up = prior[x]
                corner = prior[x - channels] if x >= channels else 0
                if filtering == 1:
                    prediction = left
                elif filtering == 2:
                    prediction = up
                elif filtering == 3:
                    prediction = (left + up) // 2
                else:
                    base = left + up - corner
                    a, b, c = abs(base - left), abs(base - up), abs(base - corner)
                    prediction = left if a <= b and a <= c else up if b <= c else corner
                row[x] = (row[x] + prediction) & 255
        pixels[y * stride:(y + 1) * stride] = row
    check_deadline(deadline)
    return Screenshot(width, height, channels, pixels)


def color_regions(image, bounds, predicate, bridge, deadline):
    """Small sampled masks, with a bounded gap between the launcher letters."""
    step = max(1, image.width // 360)
    left, top, right, bottom = bounds
    xs, ys = range(left, right, step), range(top, bottom, step)
    columns = len(xs)
    mask = bytearray(len(xs) * len(ys))
    for row, y in enumerate(ys):
        if row % 32 == 0:
            check_deadline(deadline)
        for column, x in enumerate(xs):
            r, g, b, alpha = image.pixel(x, y)
            mask[row * columns + column] = alpha == 255 and predicate(r, g, b)
    distance = max(1, round(bridge / step))
    regions = []
    for index, present in enumerate(mask):
        if not present:
            continue
        pending, count = [index], 0
        mask[index] = 0
        x0 = x1 = index % columns
        y0 = y1 = index // columns
        while pending:
            cell = pending.pop()
            x, y = cell % columns, cell // columns
            x0, x1, y0, y1 = min(x0, x), max(x1, x), min(y0, y), max(y1, y)
            count += 1
            if count % 1024 == 0:
                check_deadline(deadline)
            for ny in range(max(0, y - distance), min(len(ys), y + distance + 1)):
                for nx in range(max(0, x - distance), min(columns, x + distance + 1)):
                    neighbour = ny * columns + nx
                    if mask[neighbour]:
                        mask[neighbour] = 0
                        pending.append(neighbour)
        box = (left + x0 * step, top + y0 * step,
               min(right, left + (x1 + 1) * step), min(bottom, top + (y1 + 1) * step))
        regions.append((box, count * step * step))
    check_deadline(deadline)
    return regions


def locate_target(image, target, scale=None, deadline=None):
    """Only the production portrait, dark/lime smoke UI is accepted.

    Coordinates come from this image. The white 72dp CreateTile supplies the
    scale used for the 52dp launcher and its default 12dp left inset. No fallback tap
    or assumption about keyboard focus is permitted.
    """
    width, height = image.width, image.height
    if width < 320 or not 1.6 <= height / width <= 2.6:
        raise ActivationFailed("screenshot-is-not-supported-portrait-phone")
    matches = []
    if target == "create":
        regions = color_regions(image, (0, int(height * .05), width, int(height * .45)),
            lambda r, g, b: min(r, g, b) >= 225 and max(r, g, b) - min(r, g, b) <= 12,
            1, deadline)
        for (left, top, right, bottom), area in regions:
            w, h = right - left, bottom - top
            candidate_scale = h / 72
            if (.8 * width <= w <= .97 * width and .10 * width <= h <= .25 * width
                    and area / (w * h) >= .72
                    and abs(left - 16 * candidate_scale) <= 4 * candidate_scale
                    and abs(width - right - 16 * candidate_scale) <= 4 * candidate_scale):
                matches.append(((left, top, right, bottom), candidate_scale))
    elif target == "assistant" and scale is not None and scale > 0:
        regions = color_regions(image, (0, int(.25 * height),
            int(.25 * width), int(.62 * height)),
            lambda r, g, b: g >= 145 and g - r >= 15 and g - b >= 70 and b <= 130,
            7 * scale, deadline)
        for (left, top, right, bottom), area in regions:
            w, h = right - left, bottom - top
            cx, cy = (left + right) / 2, (top + bottom) / 2
            if (not 15 * scale <= w <= 28 * scale or not 12 * scale <= h <= 20 * scale
                    or not .1 <= area / (w * h) <= .65
                    or abs(cx - 38 * scale) > 5 * scale):
                continue
            # An outlined dark 52dp button must enclose the sparse AI label.
            # Export, play and the filled timeline plus cannot supply this rim.
            rim = 0
            for angle in range(0, 360, 45):
                theta = math.radians(angle)
                levels = []
                for radius in (23, 24, 25, 26, 27, 28):
                    x, y = round(cx + radius * scale * math.cos(theta)), round(cy + radius * scale * math.sin(theta))
                    if not 0 <= x < width or not 0 <= y < height:
                        break
                    r, g, b, alpha = image.pixel(x, y)
                    if alpha != 255 or max(r, g, b) - min(r, g, b) > 12:
                        break
                    levels.append((r + g + b) / 3)
                if len(levels) == 6 and max(levels) >= 50 and min(levels) <= 45 and max(levels) <= 110:
                    rim += 1
            if rim >= 6:
                matches.append(((left, top, right, bottom), scale))
    else:
        raise ActivationFailed("invalid-screenshot-target-or-scale")
    if len(matches) != 1:
        raise ActivationFailed(f"screenshot-{target}-target-not-unique:{len(matches)}")
    box, scale = matches[0]
    check_deadline(deadline)
    return {"bounds": list(box), "point": [(box[0] + box[2]) // 2, (box[1] + box[3]) // 2],
            "scale": scale, "image_size": [width, height]}


def current_messages(text, pid):
    """A stale process or another tag cannot supply native evidence."""
    messages = []
    for line in text.splitlines():
        match = LOG_LINE.fullmatch(line)
        if match and match[1] == pid:
            messages.append(match[2])
    return messages


def validate_manifest(payload, name):
    try:
        document = json.loads(payload)
    except (ValueError, TypeError) as error:
        raise ActivationFailed("new-project-manifest-is-not-json") from error
    if not isinstance(document, dict):
        raise ActivationFailed("new-project-manifest-is-not-an-object")
    video = document.get("video")
    if (document.get("name") != name or not document.get("concat")
            or not isinstance(document.get("bkCatProjectId"), str)
            or not document["bkCatProjectId"] or not isinstance(video, dict)
            or not all(isinstance(video.get(key), int) and video[key] > 0
                       for key in ("width", "height", "rateNum", "rateDen"))):
        raise ActivationFailed("new-project-manifest-does-not-match-the-created-project")
    return document


class Activation:
    def __init__(self, out, package, pid):
        self.out, self.package, self.pid = out, package, pid
        self.deadline = time.monotonic() + TOTAL_SECONDS
        self.serial = None
        # ASCII lower case also works with Android's physical key mapping.
        self.name = "bkcatci" + uuid.uuid4().hex[:16]
        self.project = f"/sdcard/Android/data/{package}/files/Concat/{self.name}"
        self.manifest = self.project + "/concat.json"
        self.report = {"schema": 1, "outcome": "failed", "package": package,
                       "pid": pid, "project_name": self.name,
                       "project_path": self.project, "commands": []}
        self.sequence = 0

    def command(self, label, arguments, *, pinned=True, binary=False):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise ActivationFailed("activation-deadline-exceeded")
        if pinned and self.serial is None:
            raise ActivationFailed("device-has-not-been-verified")
        command = ["adb"] + (["-s", self.serial] if pinned else []) + arguments
        self.sequence += 1
        stem = f"{self.sequence:03d}-{label}"
        entry = {"label": label, "command": command, "evidence": stem}
        self.report["commands"].append(entry)
        try:
            result = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True,
                                    timeout=min(COMMAND_SECONDS, remaining), check=False)
        except (OSError, subprocess.TimeoutExpired) as error:
            entry["result"] = type(error).__name__
            raise ActivationFailed("adb-command-unavailable-or-timed-out:" + label) from error
        (self.out / (stem + (".png" if binary else ".txt"))).write_bytes(result.stdout)
        (self.out / (stem + ".stderr.txt")).write_bytes(result.stderr)
        entry["exit_code"] = result.returncode
        if result.returncode != 0:
            raise ActivationFailed("adb-command-failed:" + label)
        return result.stdout if binary else result.stdout.decode("utf-8", errors="replace").strip()

    def verify_device(self):
        serial = self.command("selected-device", ["get-serialno"], pinned=False)
        if not re.fullmatch(r"emulator-[0-9]+", serial):
            raise ActivationFailed("activation-requires-an-emulator")
        self.serial = serial
        self.report["serial"] = serial
        qemu = [self.command("qemu-" + prop.replace(".", "-"), ["shell", "getprop", prop])
                for prop in ("ro.kernel.qemu", "ro.boot.qemu")]
        if "1" not in qemu:
            raise ActivationFailed("device-does-not-report-an-emulator")
        if self.command("current-user", ["shell", "am", "get-current-user"]) != "0":
            raise ActivationFailed("activation-requires-the-emulator-owner-user")

    def foreground(self):
        pid = self.command("current-process", ["shell", "pidof", self.package])
        if pid != self.pid:
            raise ActivationFailed("app-process-is-missing-ambiguous-or-changed")
        display = self.command("foreground-window", ["shell", "dumpsys", "window", "displays"])
        focus = [line.strip() for line in display.splitlines()
                 if line.strip().startswith("mCurrentFocus=")]
        if len(focus) != 1 or not re.fullmatch(
                r"mCurrentFocus=Window\{[0-9a-fA-F]+ u0 " + re.escape(self.package) + r"/[^{}\s]+\}",
                focus[0]):
            raise ActivationFailed("app-is-not-the-unique-focused-window")
        activities = self.command("foreground-activity", ["shell", "dumpsys", "activity", "activities"])
        resumed = [line for line in activities.splitlines() if "ResumedActivity" in line]
        if not resumed or any(self.package + "/" not in line for line in resumed):
            raise ActivationFailed("app-is-not-the-resumed-activity")

    def messages(self, *, before_activation):
        text = self.command("native-log", ["logcat", "-d", "-v", "threadtime", "-s", "concat:I", "*:S"])
        messages = current_messages(text, self.pid)
        if not any(re.search(r"(?:^|: )Concat .+ starting$", line) for line in messages):
            raise ActivationFailed("current-process-native-startup-is-missing")
        if any(FAILURE.search(line) for line in messages):
            raise ActivationFailed("current-process-reports-native-failure")
        if before_activation and any(RECOVERY.fullmatch(line) for line in messages):
            raise ActivationFailed("agent-recovered-before-explicit-ui-activation")
        return messages

    def screenshot(self, label):
        payload = self.command(label, ["exec-out", "screencap", "-p"], binary=True)
        return decode_png(payload, self.deadline)

    def tap_target(self, label, target, scale=None):
        total_deadline = self.deadline
        if target == "create":
            # A focused NativeActivity can still have a black first frame.
            # Scope every query and pixel operation to this shorter absolute
            # deadline; restoring the original deadline never extends it.
            self.deadline = min(total_deadline, time.monotonic() + 20)
        compose_at_start = None
        try:
            while True:
                check_deadline(self.deadline)
                self.foreground()
                before_capture = self.messages(before_activation=True)
                compose_at_capture = sum(bool(COMPOSE.fullmatch(line)) for line in before_capture)
                if target == "create":
                    if any("project: opening " in line for line in before_capture):
                        raise ActivationFailed("project-opened-before-new-project-form")
                    if compose_at_start is None:
                        compose_at_start = compose_at_capture
                    elif compose_at_capture != compose_at_start:
                        raise ActivationFailed("new-project-state-changed-before-tap")
                image = self.screenshot(label)
                if target == "assistant" and [image.width, image.height] != self.report["create_target"]["image_size"]:
                    raise ActivationFailed("screenshot-size-changed-before-assistant-activation")
                try:
                    match = locate_target(image, target, scale, self.deadline)
                except ActivationFailed as error:
                    if target != "create" or str(error) != "screenshot-create-target-not-unique:0":
                        raise
                    match = None
                # Revalidate even an empty frame before considering another
                # observation; process, window or native-state changes are fatal.
                self.foreground()
                before = self.messages(before_activation=True)
                if target == "create":
                    self.compose_before = sum(bool(COMPOSE.fullmatch(line)) for line in before)
                    if self.compose_before != compose_at_start:
                        raise ActivationFailed("new-project-state-changed-before-tap")
                    if any("project: opening " in line for line in before):
                        raise ActivationFailed("project-opened-before-new-project-form")
                if match is not None:
                    self.report[target + "_target"] = match
                    self.command("tap-" + target, ["shell", "input", "tap", *map(str, match["point"])])
                    return match
                check_deadline(self.deadline)
                # Poll only after an observed zero-candidate frame. No delay
                # can authorize input; the next attempt must capture a new PNG.
                time.sleep(min(POLL_SECONDS, max(0, self.deadline - time.monotonic())))
        except ActivationFailed as error:
            if target == "create" and str(error) == "activation-deadline-exceeded":
                raise ActivationFailed("first-native-create-target-was-not-observed") from error
            raise
        finally:
            self.deadline = total_deadline

    def compose_evidence(self):
        self.foreground()
        messages = self.messages(before_activation=True)
        if any("project: opening " in line for line in messages):
            raise ActivationFailed("project-opened-before-new-project-form")
        composed = [line for line in messages if COMPOSE.fullmatch(line)]
        if len(composed) <= self.compose_before:
            return False
        self.report["native_compose"] = composed[-1]
        return True

    def window_ready(self):
        # am start returns before native rendering has finished. Wait only for
        # observed startup and foreground evidence, never merely for a delay.
        try:
            self.foreground()
            self.messages(before_activation=True)
            return True
        except ActivationFailed as error:
            if str(error) in {"app-is-not-the-unique-focused-window",
                              "app-is-not-the-resumed-activity",
                              "current-process-native-startup-is-missing"}:
                return False
            raise

    def key(self, label, keycode):
        self.foreground()
        self.messages(before_activation=True)
        self.command(label, ["shell", "input", "keyevent", str(keycode)])

    def wait_for(self, label, seconds, condition):
        deadline = min(self.deadline, time.monotonic() + seconds)
        while time.monotonic() < deadline:
            if condition():
                check_deadline(self.deadline)
                return
            time.sleep(min(POLL_SECONDS, max(0, deadline - time.monotonic())))
        raise ActivationFailed(label + "-was-not-observed")

    def project_evidence(self):
        self.foreground()
        messages = self.messages(before_activation=True)
        # A missing file is an expected polling result; a failed adb command is
        # not. All interpolated names are locally generated ASCII identifiers.
        payload = self.command("new-project-manifest", ["shell",
            f"if [ -f '{self.manifest}' ]; then cat '{self.manifest}'; else echo BKCAT_MANIFEST_ABSENT; fi"])
        if payload == "BKCAT_MANIFEST_ABSENT":
            return False
        validate_manifest(payload, self.name)
        expected = f"/Android/data/{self.package}/files/Concat/{self.name}"
        opened = [line for line in messages if "project: opening " in line]
        if not opened:
            return False
        if any(not line.endswith(expected) for line in opened):
            raise ActivationFailed("native-project-open-does-not-match-the-new-manifest")
        (self.out / "created-project.json").write_text(payload + "\n", encoding="utf-8")
        self.report["manifest_sha256"] = hashlib.sha256(payload.encode()).hexdigest()
        self.report["native_project_open"] = opened[-1]
        return True

    def recovered(self):
        self.foreground()
        recovery = [line for line in self.messages(before_activation=False) if RECOVERY.fullmatch(line)]
        if not recovery:
            return False
        self.report["recovery"] = recovery
        return True

    def run(self):
        self.verify_device()
        self.wait_for("native-startup-in-foreground", 20, self.window_ready)
        initial = self.messages(before_activation=True)
        if any("project: opening " in line for line in initial):
            raise ActivationFailed("app-already-opened-a-project-before-the-ui-check")
        absent = self.command("unique-project-absent", ["shell",
            f"if [ -e '{self.project}' ]; then echo BKCAT_PROJECT_EXISTS; else echo BKCAT_PROJECT_ABSENT; fi"])
        if absent != "BKCAT_PROJECT_ABSENT":
            raise ActivationFailed("generated-project-directory-already-exists")
        create = self.tap_target("launch-screen", "create")
        self.wait_for("current-process-new-project-form", 12, self.compose_evidence)
        self.screenshot("new-project-form")
        self.foreground()
        if not self.compose_evidence():
            raise ActivationFailed("new-project-form-evidence-was-lost-before-input")
        self.command("name-new-project", ["shell", "input", "text", self.name])
        self.key("create-new-project", 66)
        self.wait_for("new-native-project", 25, self.project_evidence)
        self.tap_target("manual-editor", "assistant", create["scale"])
        self.wait_for("current-process-native-recovery", 45, self.recovered)
        self.screenshot("opened-assistant")
        self.foreground()
        if not self.recovered():
            raise ActivationFailed("native-recovery-evidence-was-lost-before-completion")
        check_deadline(self.deadline)
        self.report["outcome"] = "native-ui-activation-and-recovery-observed"
        self.report["model_call_asserted"] = False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--package", required=True)
    parser.add_argument("--pid", required=True)
    args = parser.parse_args()
    if args.package != "app.concat.editor" or not re.fullmatch(r"[1-9][0-9]*", args.pid):
        parser.error("use the production app package and exactly one live app PID")
    try:
        args.out.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        parser.error("use a fresh activation evidence directory")
    activation = Activation(args.out, args.package, args.pid)
    result = 1
    try:
        activation.run()
        result = 0
    except ActivationFailed as error:
        activation.report["failure"] = str(error)
        print("agent-ui-activation: " + str(error), file=sys.stderr)
    finally:
        activation.report["exit_code"] = result
        (args.out / "report.json").write_text(json.dumps(activation.report, indent=2) + "\n", encoding="utf-8")
        (args.out / "runner-exit.txt").write_text(str(result) + "\n", encoding="utf-8")
    if result == 0:
        print("agent-ui-activation: real project and native recovery observed; full smoke observation is still required")
    return result


if __name__ == "__main__":
    raise SystemExit(main())

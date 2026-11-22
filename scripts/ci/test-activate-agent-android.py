#!/usr/bin/env python3
"""Failure-path checks for the UI smoke driver, not Android UI acceptance.

The raw adb fixtures below intentionally represent an absent process, a stale
recovery, a wrong window/project, or a runtime that never recovers. Synthetic
screens exercise only recognition and rejection, not Android UI acceptance.
"""

from functools import lru_cache
import importlib.util
import itertools
import json
import math
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zlib


HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("activation", HERE / "activate-agent-android.py")
activation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(activation)


def log(message, pid="415", tag="concat"):
    return f"11-21 22:00:00.123 {pid} 416 I {tag} : {message}\n"


def chunk(kind, payload):
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))


def png(width, height, pixels, channels=3, filtering=0):
    stride = width * channels
    raw = bytearray()
    for y in range(height):
        row = pixels[y * stride:(y + 1) * stride]
        prior = pixels[(y - 1) * stride:y * stride] if y else bytes(stride)
        raw.append(filtering)
        for x, value in enumerate(row):
            left = row[x - channels] if x >= channels else 0
            up = prior[x]
            corner = prior[x - channels] if x >= channels else 0
            prediction = 0
            if filtering == 1:
                prediction = left
            elif filtering == 2:
                prediction = up
            elif filtering == 3:
                prediction = (left + up) // 2
            elif filtering == 4:
                base = left + up - corner
                distances = [abs(base - item) for item in (left, up, corner)]
                prediction = (left, up, corner)[distances.index(min(distances))]
            raw.append((value - prediction) & 255)
    return raw_png(width, height, bytes(raw), channels)


def raw_png(width, height, raw, channels=3, *, interlace=0, trailing=b""):
    header = struct.pack(">IIBBBBB", width, height, 8, 2 if channels == 3 else 6, 0, 0, interlace)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(raw) + trailing) + chunk(b"IEND", b""))


@lru_cache(maxsize=16)
def screen(kind="start", width=720):
    """Source-proportioned dark/lime fixtures, not recorded device evidence."""
    height = width * 13 // 6
    pixels = bytearray(bytes((14, 14, 16)) * width * height)
    factor = width / 1080

    def rectangle(bounds, color):
        x0, y0, x1, y1 = (round(value * factor) for value in bounds)
        for y in range(max(0, y0), min(height, y1)):
            pixels[(y * width + x0) * 3:(y * width + x1) * 3] = bytes(color) * (x1 - x0)

    def circle(cx, cy, radius, color):
        cx, cy, radius = cx * factor, cy * factor, radius * factor
        for y in range(max(0, math.floor(cy - radius)), min(height, math.ceil(cy + radius + 1))):
            for x in range(max(0, math.floor(cx - radius)), min(width, math.ceil(cx + radius + 1))):
                if (x - cx) ** 2 + (y - cy) ** 2 <= radius ** 2:
                    index = (y * width + x) * 3
                    pixels[index:index + 3] = bytes(color)

    def stroke(a, b, color):
        ax, ay, bx, by = [value * factor for value in (*a, *b)]
        radius = max(.8, 2 * factor)
        for y in range(math.floor(min(ay, by) - radius), math.ceil(max(ay, by) + radius + 1)):
            for x in range(math.floor(min(ax, bx) - radius), math.ceil(max(ax, bx) + radius + 1)):
                t = min(1, max(0, ((x - ax) * (bx - ax) + (y - ay) * (by - ay))
                               / ((bx - ax) ** 2 + (by - ay) ** 2)))
                if math.hypot(x - ax - t * (bx - ax), y - ay - t * (by - ay)) <= radius:
                    index = (y * width + x) * 3
                    pixels[index:index + 3] = bytes(color)

    def launcher(cy, rim=True, filled=False, cx=100):
        lime = (203, 245, 63)
        if rim:
            circle(cx, cy, 68, (76, 76, 78))
            circle(cx, cy, 65, (33, 33, 35))
        if filled:
            rectangle((cx - 24, cy - 24, cx + 24, cy + 24), lime)
            return
        # Synthetic letter strokes exercise recognition bounds, not typography.
        stroke((cx - 24, cy + 20), (cx - 9, cy - 20), lime)
        stroke((cx - 9, cy - 20), (cx + 6, cy + 20), lime)
        stroke((cx - 17, cy + 2), (cx - 1, cy + 2), lime)
        stroke((cx + 22, cy - 20), (cx + 22, cy + 20), lime)

    if kind in ("start", "double_create", "shifted_create"):
        bounds = (42, 333, 1038, 522) if kind != "shifted_create" else (0, 333, 996, 522)
        rectangle(bounds, (245, 245, 247))
        rectangle((240, 390, 700, 420), (14, 14, 16))
        if kind == "double_create":
            rectangle((42, 700, 1038, 889), (245, 245, 247))
    elif kind in ("editor", "double_assistant", "no_rim", "filled_assistant", "controls_only", "right_assistant"):
        lime = (203, 245, 63)
        rectangle((833, 79, 1059, 174), lime)  # Export is above the search region.
        circle(540, 1340, 53, lime)  # Play is central, not at the left inset.
        rectangle((923, 1659, 1038, 1774), lime)  # Timeline plus is below it.
        if kind != "controls_only":
            launcher(1161, rim=kind != "no_rim", filled=kind == "filled_assistant",
                     cx=980 if kind == "right_assistant" else 100)
        if kind == "double_assistant":
            launcher(900)
    # Filter zero is valid screencap data and keeps fixture generation cheap.
    stride = width * 3
    raw = b"".join(b"\0" + pixels[y * stride:(y + 1) * stride] for y in range(height))
    return raw_png(width, height, raw)


class ScreenshotChecks(unittest.TestCase):
    def test_all_five_filters_and_both_truecolor_formats(self):
        for channels in (3, 4):
            pixels = bytes((index * 47 + 19) % 256 for index in range(5 * 4 * channels))
            for filtering in range(5):
                with self.subTest(channels=channels, filtering=filtering):
                    image = activation.decode_png(png(5, 4, pixels, channels, filtering))
                    self.assertEqual(image.pixels, pixels)

    def test_crc_truncation_trailing_data_and_interlace_are_rejected(self):
        valid = png(2, 2, bytes(12))
        corrupt = valid[:40] + bytes([valid[40] ^ 1]) + valid[41:]
        for payload in (corrupt, valid[:-1], valid + b"extra", raw_png(2, 2, bytes(14), interlace=1)):
            with self.subTest(payload=payload[:16]):
                with self.assertRaises(activation.ActivationFailed):
                    activation.decode_png(payload)

    def test_decompression_size_trailing_stream_and_filter_bounds_are_rejected(self):
        for raw, trailing in ((bytes(13), b""), (bytes(15), b""),
                              (bytes(500_000), b""), (bytes(14), b"extra"),
                              (bytes([5]) + bytes(6) + bytes(7), b"")):
            with self.subTest(length=len(raw), trailing=trailing):
                with self.assertRaises(activation.ActivationFailed):
                    activation.decode_png(raw_png(2, 2, raw, trailing=trailing))

    def test_chunk_order_and_unsupported_critical_chunks_are_rejected(self):
        signature = b"\x89PNG\r\n\x1a\n"
        header = chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 2, 0, 0, 0))
        data = chunk(b"IDAT", zlib.compress(bytes(14)))
        end = chunk(b"IEND", b"")
        for middle in (header + header + data, data + header,
                       header + chunk(b"ABCD", b"") + data,
                       header + data + chunk(b"PLTE", bytes(3)),
                       header + data + chunk(b"tEXt", b"note") + data,
                       header + chunk(b"tRNS", bytes(6)) + data):
            with self.assertRaises(activation.ActivationFailed):
                activation.decode_png(signature + middle + end)

    def test_dimensions_and_deadline_are_checked_before_pixel_work(self):
        for width, height in ((0, 1), (4097, 1), (4096, 4096), (1, 8193)):
            with self.assertRaisesRegex(activation.ActivationFailed, "dimensions"):
                activation.decode_png(raw_png(width, height, b""))
        with patch.object(activation.time, "monotonic", return_value=101):
            with self.assertRaisesRegex(activation.ActivationFailed, "deadline"):
                activation.decode_png(screen(), 100)

    def test_targets_follow_scale_without_fixed_physical_coordinates(self):
        for width in (360, 720, 1080):
            with self.subTest(width=width):
                create = activation.locate_target(activation.decode_png(screen("start", width)), "create")
                assistant = activation.locate_target(activation.decode_png(screen("editor", width)),
                                                     "assistant", create["scale"])
                self.assertAlmostEqual(create["point"][0], width / 2, delta=2)
                self.assertAlmostEqual(assistant["point"][0], 100 * width / 1080, delta=3)
                self.assertAlmostEqual(assistant["point"][1], 1161 * width / 1080, delta=3)

    def test_multiple_cards_or_launchers_are_rejected(self):
        for kind, target, scale in (("double_create", "create", None),
                                    ("double_assistant", "assistant", 1.75)):
            with self.assertRaisesRegex(activation.ActivationFailed, "not-unique:2"):
                activation.locate_target(activation.decode_png(screen(kind)), target, scale)

    def test_wrong_screen_color_shape_and_missing_rim_do_not_supply_a_target(self):
        for kind, target, scale in (("editor", "create", None), ("start", "assistant", 1.75),
                ("controls_only", "assistant", 1.75), ("no_rim", "assistant", 1.75),
                ("filled_assistant", "assistant", 1.75), ("right_assistant", "assistant", 1.75),
                ("shifted_create", "create", None)):
            with self.subTest(kind=kind, target=target):
                with self.assertRaisesRegex(activation.ActivationFailed, "not-unique:0"):
                    activation.locate_target(activation.decode_png(screen(kind)), target, scale)


class BrokenDevice(activation.Activation):
    """Raw, bounded fixture responses; production guards remain unmodified."""

    def __init__(self, out, failure):
        super().__init__(out, "app.concat.editor", "415")
        self.failure = failure
        self.calls = []
        self.named = False
        self.created = False
        self.activated = False
        self.composed = False
        self.captured = False
        self.capture_count = 0

    def command(self, label, arguments, **kwargs):
        self.calls.append(arguments)
        if arguments == ["get-serialno"]:
            return "device-real-phone" if self.failure == "physical" else "emulator-5564"
        if arguments[:2] == ["shell", "getprop"]:
            return "1"
        if arguments == ["shell", "am", "get-current-user"]:
            return "0"
        if arguments == ["shell", "pidof", self.package]:
            changed = self.failure == "changed_pid" or (self.failure in (
                "changed_after_screenshot", "blank_changed_pid") and self.captured)
            return "999" if changed else "415"
        if arguments == ["shell", "dumpsys", "window", "displays"]:
            wrong = self.failure == "wrong_window" or (self.failure == "blank_other_window" and self.captured)
            package = "com.other.app" if wrong else self.package
            return f"mCurrentFocus=Window{{abc u0 {package}/android.app.NativeActivity}}"
        if arguments == ["shell", "dumpsys", "activity", "activities"]:
            return f"mResumedActivity: ActivityRecord{{aaa u0 {self.package}/android.app.NativeActivity t4}}"
        if arguments[0] == "logcat":
            result = log("concat_android: Concat 0.2.10 starting")
            if self.failure == "premature":
                result += log("concat::embedded: bk-cat-agent: state=recovery generation=1")
            if self.failure == "stale_compose":
                result += log("concat::state: start: Compose")
            elif self.failure in ("compose_during_screenshot", "blank_compose") and self.captured:
                result += log("concat::state: start: Compose")
            elif self.composed:
                result += log("concat::state: start: Compose", pid="414" if self.failure == "wrong_compose_pid" else "415")
            if self.created:
                project = self.project if self.failure != "wrong_open" else self.project + "wrong"
                result += log("concat::studio::project_files: project: opening " + project.replace("/sdcard", "/storage/emulated/0"))
            # Even after tapping, this fixture deliberately never recovers.
            result += log("bk-cat-agent: state=recovery generation=2", pid="414")
            return result
        if arguments[:2] == ["exec-out", "screencap"]:
            self.captured = True
            self.capture_count += 1
            kind = ("editor" if self.created else "start")
            if (self.failure in ("blank_forever", "blank_changed_pid", "blank_other_window", "blank_compose")
                    or self.failure == "first_frame_blank" and self.capture_count == 1):
                kind = "blank"
            if self.failure == "bad_frame":
                return b"\x89PNG\r\n\x1a\ntruncated"
            if self.failure == "wrong_editor" and self.created:
                kind = "controls_only"
            if self.failure == "ambiguous_create" and not self.created:
                kind = "double_create"
            return screen(kind, 360 if self.failure == "resized_editor" and self.created else 720)
        if arguments[:3] == ["shell", "input", "tap"]:
            if self.created:
                self.activated = True
            elif self.failure != "no_compose":
                self.composed = True
            return ""
        if arguments[:3] == ["shell", "input", "text"]:
            self.named = True
            return ""
        if arguments[:3] == ["shell", "input", "keyevent"]:
            if arguments[3] == "66" and self.named:
                self.created = True
            return ""
        if arguments[0] == "shell" and "BKCAT_PROJECT_ABSENT" in arguments[1]:
            return "BKCAT_PROJECT_ABSENT"
        if arguments[0] == "shell" and "BKCAT_MANIFEST_ABSENT" in arguments[1]:
            if self.failure == "missing_manifest":
                return "BKCAT_MANIFEST_ABSENT"
            return json.dumps({"concat": "0.2.10", "bkCatProjectId": "project-id",
                "name": "someone-elses-project" if self.failure == "wrong_manifest" else self.name,
                "video": {"width": 1920, "height": 1080, "rateNum": 30, "rateDen": 1}})
        raise AssertionError("unexpected adb fixture call: " + repr(arguments))


class DriverFailures(unittest.TestCase):
    def exercise(self, failure, expected, may_activate=False):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(activation.time, "monotonic", side_effect=itertools.count(0, .05)), \
                    patch.object(activation.time, "sleep"):
                driver = BrokenDevice(Path(directory), failure)
                with self.assertRaisesRegex(activation.ActivationFailed, expected):
                    driver.run()
                self.assertEqual(driver.report["outcome"], "failed")
                self.assertEqual(driver.activated, may_activate)
                self.assertEqual(driver.deadline, 100)
                return driver.calls

    def test_blank_first_frame_requires_a_new_target_frame_before_input(self):
        calls = self.exercise("first_frame_blank", "current-process-native-recovery-was-not-observed", True)
        screenshots = [i for i, call in enumerate(calls) if call[:2] == ["exec-out", "screencap"]]
        inputs = [i for i, call in enumerate(calls) if call[:2] == ["shell", "input"]]
        self.assertGreaterEqual(len(screenshots), 4)
        self.assertGreater(inputs[0], screenshots[1])
        self.assertIn(["shell", "pidof", "app.concat.editor"], calls[screenshots[0] + 1:screenshots[1]])
        self.assertTrue(any(call[0] == "logcat" for call in calls[screenshots[0] + 1:screenshots[1]]))

    def test_permanent_blank_frames_expire_without_input(self):
        calls = self.exercise("blank_forever", "first-native-create-target-was-not-observed")
        self.assertGreater(sum(call[:2] == ["exec-out", "screencap"] for call in calls), 1)
        self.assertFalse(any(call[:2] == ["shell", "input"] for call in calls))

    def test_ambiguous_bad_and_changed_frames_fail_after_the_first_observation(self):
        cases = (("ambiguous_create", "create-target-not-unique:2"),
                 ("bad_frame", "truncated-png-chunk"),
                 ("blank_changed_pid", "process-is-missing-ambiguous-or-changed"),
                 ("blank_other_window", "app-is-not-the-unique-focused-window"),
                 ("blank_compose", "new-project-state-changed-before-tap"))
        for failure, expected in cases:
            with self.subTest(failure=failure):
                calls = self.exercise(failure, expected)
                self.assertEqual(sum(call[:2] == ["exec-out", "screencap"] for call in calls), 1)
                self.assertFalse(any(call[:2] == ["shell", "input"] for call in calls))

    def test_real_phone_is_refused_before_any_input(self):
        calls = self.exercise("physical", "requires-an-emulator")
        self.assertFalse(any(call[:2] == ["shell", "input"] for call in calls))

    def test_restarted_process_cannot_inherit_the_original_run(self):
        calls = self.exercise("changed_pid", "process-is-missing-ambiguous-or-changed")
        self.assertFalse(any(call[:2] == ["shell", "input"] for call in calls))

    def test_other_front_window_receives_no_input(self):
        calls = self.exercise("wrong_window", "native-startup-in-foreground-was-not-observed")
        self.assertFalse(any(call[:2] == ["shell", "input"] for call in calls))

    def test_automatic_recovery_fails_before_new_project_input(self):
        calls = self.exercise("premature", "recovered-before-explicit-ui-activation")
        self.assertFalse(any(call[:2] == ["shell", "input"] for call in calls))

    def test_project_open_log_without_file_never_activates_assistant(self):
        self.exercise("missing_manifest", "new-native-project-was-not-observed")

    def test_another_project_manifest_never_activates_assistant(self):
        self.exercise("wrong_manifest", "manifest-does-not-match")

    def test_another_project_open_log_never_activates_assistant(self):
        self.exercise("wrong_open", "native-project-open-does-not-match")

    def test_form_must_emit_new_current_process_compose_before_name_input(self):
        for failure in ("no_compose", "stale_compose", "wrong_compose_pid"):
            with self.subTest(failure=failure):
                calls = self.exercise(failure, "current-process-new-project-form-was-not-observed")
                self.assertFalse(any(call[:3] == ["shell", "input", "text"] for call in calls))

    def test_form_state_changing_during_screenshot_rejects_the_stale_target(self):
        calls = self.exercise("compose_during_screenshot", "new-project-state-changed-before-tap")
        self.assertFalse(any(call[:2] == ["shell", "input"] for call in calls))

    def test_process_is_checked_again_after_decoding_and_before_tapping(self):
        calls = self.exercise("changed_after_screenshot", "process-is-missing-ambiguous-or-changed")
        self.assertFalse(any(call[:2] == ["shell", "input"] for call in calls))

    def test_wrong_or_resized_editor_and_ambiguous_start_never_activate(self):
        for failure, expected in (("wrong_editor", "assistant-target-not-unique:0"),
                ("resized_editor", "size-changed-before-assistant"),
                ("ambiguous_create", "create-target-not-unique:2")):
            with self.subTest(failure=failure):
                self.exercise(failure, expected)

    def test_previous_process_recovery_never_satisfies_current_recovery(self):
        calls = self.exercise("missing_recovery", "current-process-native-recovery-was-not-observed", True)
        self.assertEqual(sum(call[:3] == ["shell", "input", "tap"] for call in calls), 2)
        self.assertEqual([call[3] for call in calls if call[:3] == ["shell", "input", "keyevent"]], ["66"])

    def test_each_query_uses_only_the_remaining_global_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = activation.Activation(Path(directory), "app.concat.editor", "415")
            driver.serial, driver.deadline = "emulator-5564", 100
            result = subprocess.CompletedProcess([], 0, b"415\n", b"")
            with patch.object(activation.time, "monotonic", return_value=98.5), \
                    patch.object(activation.subprocess, "run", return_value=result) as run:
                self.assertEqual(driver.command("budget", ["shell", "pidof", driver.package]), "415")
                self.assertEqual(run.call_args.kwargs["timeout"], 1.5)

    def test_unrelated_tag_and_stale_pid_cannot_supply_native_evidence(self):
        text = (log("bk-cat-agent: state=recovery generation=1", "414")
                + log("bk-cat-agent: state=recovery generation=2", "415", "not_concat")
                + log("Concat 0.2.10 starting"))
        self.assertEqual(activation.current_messages(text, "415"), ["Concat 0.2.10 starting"])

    def test_no_install_and_unpaired_activation_are_rejected_before_device_work(self):
        # Resolve the selected shell before CreateProcess searches System32:
        # Windows also ships a bash.exe WSL launcher, even without a distro.
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "the smoke argument checks require Bash")
        with tempfile.TemporaryDirectory() as directory:
            apk = Path(directory) / "placeholder.apk"
            apk.write_bytes(b"only argument validation runs")
            out = Path(directory) / "no-device-work"
            for arguments in (["--require-agent", "--no-install", "--wait", "60"],
                              ["--wait", "60"], ["--require-agent", "--wait", "1"]):
                result = subprocess.run([bash, str(HERE / "smoke-android.sh"),
                    "--out", str(out), "--activate-agent-ui", *arguments, str(apk)],
                    capture_output=True, text=True, timeout=3, check=False)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()

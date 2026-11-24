#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Drive actual feature-gated BK CUT windows through five process crashes.

This is a deterministic loopback model fixture, never a real provider test.
The application retains its production Pi, native approval, guarded edit,
SQLite journal and save code. No user project, setting or real key is read.
Run in the existing desktop build environment so the executable's DLLs resolve.
"""

from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
import hashlib
import http.server
import json
import os
import re
from pathlib import Path
import secrets
import shutil
import sqlite3
import struct
import subprocess
import sys
import threading
import time
import traceback
import zlib


STAGES = ("intent", "applied", "receipt", "saved", "save-first")
DECISIONS = ("continue", "discard")
MODEL = "bkcut-controlled-crash-fixture"
SUMMARIZATION_SYSTEM = ("You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified."
    "\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.")


def atomic_json(path: Path, value) -> None:
    temporary = path.with_name(path.name + f".pending-{os.getpid()}-{threading.get_ident()}")
    with temporary.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    for attempt in range(30):
        try:
            os.replace(temporary, path)
            return
        except PermissionError:
            if attempt == 29:
                raise
            time.sleep(0.01)


def read_json(path: Path):
    with path.open(encoding="utf-8") as stream:
        return json.load(stream)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


class Fixture:
    def __init__(self, root: Path, stage: str, decision: str, run_id: str, deep_continuation: bool = False):
        self.root, self.stage, self.decision, self.run_id = root, stage, decision, run_id
        self.deep_continuation = deep_continuation
        self.phase = "interrupt"
        self.requests = 0
        self.recovery_requests = 0
        self.recovery_inference_requests = 0
        self.compaction_requests = 0
        self.summary_texts = []
        self.compaction_used_by_followup = False
        self.failures = []
        self.lock = threading.Lock()
        self.publish()

    def publish(self):
        atomic_json(self.root / "fixture-stats.json", {
            "requests": self.requests,
            "recoveryRequests": self.recovery_requests,
            "recoveryInferenceRequests": self.recovery_inference_requests,
            "compactionRequests": self.compaction_requests,
            "compactionUsedByFollowup": self.compaction_used_by_followup,
            "deepContinuationRequested": self.deep_continuation,
            "phase": self.phase,
            "failures": self.failures,
            "realProviderInference": False,
            "origin": "127.0.0.1",
        })

    def recover(self):
        with self.lock:
            self.phase = "recover"
            self.publish()

    def answer(self, path: str, body: dict, authenticated: bool):
        messages = body.get("messages", [])
        compaction = (not body.get("tools") and isinstance(messages, list) and bool(messages)
            and messages[0].get("role") == "system" and messages[0].get("content") == SUMMARIZATION_SYSTEM)
        with self.lock:
            self.requests += 1
            phase = self.phase
            if phase == "recover":
                self.recovery_requests += 1
                if compaction:
                    self.compaction_requests += 1
                else:
                    self.recovery_inference_requests += 1
                    self.compaction_used_by_followup |= "<summary>" in json.dumps(messages, ensure_ascii=False)
            sequence = self.recovery_inference_requests if phase == "recover" else self.requests
            # The bearer value is intentionally never read into any log/report.
            with (self.root / "fixture-requests.jsonl").open("a", encoding="utf-8") as stream:
                stream.write(json.dumps({"sequence": self.requests, "phase": phase,
                    "requestKind": "original-compaction" if compaction else "task-inference",
                    "timeNs": time.time_ns(), "path": path, "body": body}, ensure_ascii=False) + "\n")
                stream.flush()
                os.fsync(stream.fileno())
            self.publish()
        try:
            assert path == f"/{self.run_id}/v1/chat/completions", "unexpected fixture route"
            assert authenticated, "actual native adapter did not provide the generated test credential"
            assert body.get("model") == MODEL and body.get("stream") is True, "request did not use the isolated streaming model"
            assert isinstance(body.get("messages"), list), "original model request has no messages"
            if phase == "interrupt":
                assert sequence == 1, "unexpected automatic extra request before process termination"
                return self.tool(body, "edit_apply_guarded", "original-crash-tool", read_json(self.root / "fixture-plan.json"))
            assert self.decision == "continue", "discard must never start a model request"
            if compaction:
                plan = read_json(self.root / "fixture-plan.json")
                summary = ("## Goal\n在当前原生工程中核对原任务，并按用户要求处理一条文字剪辑。\n"
                    "## Constraints & Preferences\n- 普通回复与文案均保留原简体中文语言快照。\n"
                    "- 原生工程、原生权限和当前锁轨是唯一执行依据。旧中断写入不得自动重放。\n"
                    "## Progress\n### Done\n- [x] 已接收用户明确继续的决定。\n"
                    "### In Progress\n- [ ] 重新读取当前工程与操作事实；任何新修改必须单独获得当前原生批准。\n"
                    "### Blocked\n- 未核实的原操作不能宣称成功。\n"
                    "## Key Decisions\n- 先读取；已保存内容保留，缺失内容仅按新的批准计划处理。\n"
                    "## Next Steps\n1. 使用 project_get 读取当前工程。\n2. 遵循原生拒绝和新的批准结果。\n"
                    f"## Critical Context\n当前工程：{plan['path']}\n")
                with self.lock:
                    self.summary_texts.append(summary)
                    self.publish()
                return self.final(summary)
            redo = self.stage in ("intent", "applied", "receipt")
            if sequence == 1:
                return self.tool(body, "project_get", "recovery-read-current", {"path": read_json(self.root / "fixture-plan.json")["path"]})
            if not redo:
                assert sequence == 2, "saved operation unexpectedly repeated model work"
                return self.final("已核对当前落盘工程，保留已有剪辑，不重复执行旧操作。")
            if sequence == 2:
                return self.tool(body, "edit_apply_guarded", "recovery-new-plan-locked", read_json(self.root / "fixture-plan.json"))
            if sequence == 3:
                deadline = time.monotonic() + 15
                while not (self.root / "lock-rechecked.json").exists():
                    assert time.monotonic() < deadline, "native lock refusal was not observed before retry planning"
                    time.sleep(0.02)
                return self.tool(body, "project_get", "recovery-read-after-unlock", {"path": read_json(self.root / "fixture-plan.json")["path"]})
            if sequence == 4:
                return self.tool(body, "edit_apply_guarded", "recovery-new-plan-unlocked", read_json(self.root / "fixture-plan.json"))
            if self.deep_continuation and 5 <= sequence <= 7:
                return self.tool(body, "project_get", f"recovery-extra-read-{sequence - 4}",
                    {"path": read_json(self.root / "fixture-plan.json")["path"]})
            assert sequence == (8 if self.deep_continuation else 5), "unexpected repeated original or new plan"
            return self.final("重新读取当前工程并完成本次新批准的修改；旧操作没有自动重放。")
        except Exception as error:
            with self.lock:
                self.failures.append(str(error))
                self.publish()
            raise

    @staticmethod
    def tool(body, suffix, call, arguments):
        names = [item.get("function", {}).get("name", "") for item in body.get("tools", [])]
        matches = [name for name in names if name == suffix or name.endswith("_" + suffix)]
        assert len(matches) == 1, f"actual original adapter did not expose exactly one {suffix} tool"
        return [{"delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": call,
            "type": "function", "function": {"name": matches[0], "arguments": json.dumps(arguments, ensure_ascii=False)}}]}},
            {"finish": "tool_calls"}]

    @staticmethod
    def final(text):
        return [{"delta": {"role": "assistant", "content": text}}, {"finish": "stop"}]


def make_handler(fixture: Fixture):
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_POST(self):
            try:
                length = int(self.headers.get("Content-Length", "0"))
                assert 0 < length <= 16 * 1024 * 1024, "invalid request body size"
                body = json.loads(self.rfile.read(length))
                authorization = self.headers.get("Authorization", "")
                expected_digest = read_json(fixture.root / "whole-process-case.json")["credential_digest"]
                authenticated = authorization.startswith("Bearer ") and secrets.compare_digest(
                    hashlib.sha256(authorization[7:].encode()).hexdigest(), expected_digest)
                del authorization
                parts = fixture.answer(self.path, body, authenticated)
                chunks = []
                for part in parts:
                    chunk = {"id": "bkcut-local-fixture", "object": "chat.completion.chunk", "created": 1,
                        "model": MODEL, "choices": [{"index": 0, "delta": part.get("delta", {}),
                        "finish_reason": part.get("finish")}]}
                    chunks.append("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n")
                data = ("".join(chunks) + "data: [DONE]\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream; charset=utf-8")
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(data)
                self.wfile.flush()
            except Exception as error:
                data = json.dumps({"error": {"message": str(error), "type": "isolated_acceptance_failure"}}).encode()
                self.send_response(500)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(data)
            self.close_connection = True
    return Handler


def screenshot_window(pid: int, output: Path):
    """Capture the bound real application window without copying other windows."""
    user32, gdi32 = ctypes.windll.user32, ctypes.windll.gdi32
    user32.SetProcessDPIAware()
    user32.IsWindowVisible.argtypes = [wintypes.HWND]
    user32.IsWindow.argtypes = [wintypes.HWND]
    user32.IsIconic.argtypes = [wintypes.HWND]
    user32.SetForegroundWindow.argtypes = [wintypes.HWND]
    user32.GetDC.argtypes = [wintypes.HWND]
    user32.GetForegroundWindow.restype = wintypes.HWND
    user32.GetClientRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
    user32.ClientToScreen.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.POINT)]
    user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
    user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
    user32.GetDC.restype = wintypes.HDC
    user32.PrintWindow.argtypes = [wintypes.HWND, wintypes.HDC, wintypes.UINT]
    user32.PrintWindow.restype = wintypes.BOOL
    gdi32.CreateCompatibleDC.argtypes = [wintypes.HDC]
    gdi32.CreateCompatibleDC.restype = wintypes.HDC
    gdi32.CreateCompatibleBitmap.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int]
    gdi32.CreateCompatibleBitmap.restype = wintypes.HBITMAP
    gdi32.SelectObject.argtypes = [wintypes.HDC, wintypes.HGDIOBJ]
    gdi32.SelectObject.restype = wintypes.HGDIOBJ
    gdi32.BitBlt.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
        wintypes.HDC, ctypes.c_int, ctypes.c_int, wintypes.DWORD]
    gdi32.GetDIBits.argtypes = [wintypes.HDC, wintypes.HBITMAP, wintypes.UINT, wintypes.UINT,
        ctypes.c_void_p, ctypes.c_void_p, wintypes.UINT]
    gdi32.DeleteObject.argtypes = [wintypes.HGDIOBJ]
    gdi32.DeleteDC.argtypes = [wintypes.HDC]
    user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
    found = []
    callback_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    user32.EnumWindows.argtypes = [callback_type, wintypes.LPARAM]

    @callback_type
    def enumerate_window(hwnd, _):
        process = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(process))
        if process.value == pid and user32.IsWindowVisible(hwnd) and not user32.IsIconic(hwnd):
            rectangle = wintypes.RECT()
            if user32.GetWindowRect(hwnd, ctypes.byref(rectangle)):
                area = (rectangle.right - rectangle.left) * (rectangle.bottom - rectangle.top)
                if area > 10000:
                    found.append((area, hwnd, rectangle))
        return True

    user32.EnumWindows(enumerate_window, 0)
    if not found:
        raise RuntimeError("no actual visible application window for the checkpoint process")
    _, hwnd, _ = max(found, key=lambda item: item[0])
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.2)
    foreground_before = user32.GetForegroundWindow()

    def verify_bound_window():
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value != pid or not user32.IsWindow(hwnd) or not user32.IsWindowVisible(hwnd) or user32.IsIconic(hwnd):
            raise RuntimeError("the bound capture window changed identity or is no longer visibly available")

    verify_bound_window()
    client = wintypes.RECT()
    origin = wintypes.POINT(0, 0)
    if not user32.GetClientRect(hwnd, ctypes.byref(client)) or not user32.ClientToScreen(hwnd, ctypes.byref(origin)):
        raise ctypes.WinError()
    rect = wintypes.RECT(origin.x, origin.y, origin.x + client.right, origin.y + client.bottom)
    covering = []

    @callback_type
    def check_cover(candidate, _):
        if candidate == hwnd:
            return False
        if user32.IsWindowVisible(candidate):
            process = wintypes.DWORD()
            user32.GetWindowThreadProcessId(candidate, ctypes.byref(process))
            other = wintypes.RECT()
            if process.value != pid and user32.GetWindowRect(candidate, ctypes.byref(other)):
                if max(rect.left, other.left) < min(rect.right, other.right) and max(rect.top, other.top) < min(rect.bottom, other.bottom):
                    covering.append(int(candidate))
        return True

    user32.EnumWindows(check_cover, 0)
    # A persistent GPU overlay can intersect every window even when transparent.
    # PrintWindow captures only this exact HWND, so foreign windows are never
    # copied. Record intersections and retain a separate manual image gate.
    width, height = rect.right - rect.left, rect.bottom - rect.top
    if width <= 0 or height <= 0 or width > 16384 or height > 16384:
        raise RuntimeError("unexpected acceptance window geometry")
    screen = user32.GetDC(hwnd)
    dc = gdi32.CreateCompatibleDC(screen)
    bitmap = gdi32.CreateCompatibleBitmap(screen, width, height)
    previous = gdi32.SelectObject(dc, bitmap)
    try:
        if not user32.PrintWindow(hwnd, dc, 0x1 | 0x2):
            raise RuntimeError("the exact application window did not provide complete capture pixels")
        header = struct.pack("<IiiHHIIiiII", 40, width, -height, 1, 32, 0, width * height * 4, 0, 0, 0, 0)
        info = ctypes.create_string_buffer(header)
        pixels = ctypes.create_string_buffer(width * height * 4)
        gdi32.SelectObject(dc, previous)
        if gdi32.GetDIBits(dc, bitmap, 0, height, pixels, info, 0) != height:
            raise ctypes.WinError()
        verify_bound_window()
        foreground_after = user32.GetForegroundWindow()
        bgra = pixels.raw
        rgb = bytearray(width * height * 3)
        rgb[0::3], rgb[1::3], rgb[2::3] = bgra[2::4], bgra[1::4], bgra[0::4]
        samples = {bytes(rgb[(y * width + x) * 3:(y * width + x) * 3 + 3])
            for y in range(0, height, max(1, height // 64))
            for x in range(0, width, max(1, width // 64))}
        if len(samples) < 8 or max(rgb) - min(rgb) < 24:
            raise RuntimeError("native window capture is blank or lacks reviewable application content")
        rows = b"".join(b"\x00" + rgb[y * width * 3:(y + 1) * width * 3] for y in range(height))

        def chunk(name, data):
            return struct.pack(">I", len(data)) + name + data + struct.pack(">I", zlib.crc32(name + data))
        png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        png += chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
        output.write_bytes(png)
        return {"pid": pid, "windowHandle": int(hwnd), "rect": [rect.left, rect.top, rect.right, rect.bottom],
            "sha256": sha256(output), "realWindowPixels": True,
            "windowIdentityVerifiedBeforeAndAfter": True, "visibleAndNotMinimized": True,
            "foregroundBefore": int(foreground_before or 0), "foregroundAfter": int(foreground_after or 0),
            "foregroundVerified": foreground_before == hwnd and foreground_after == hwnd,
            "method": "PrintWindow(HWND, PW_CLIENTONLY | PW_RENDERFULLCONTENT)",
            "foreignIntersectingWindows": covering, "foreignWindowPixelsIncluded": False,
            "sampledDistinctColors": len(samples), "manualVisualReviewRequired": True,
            "scope": "exact acceptance application client window only"}
    finally:
        gdi32.SelectObject(dc, previous)
        gdi32.DeleteObject(bitmap)
        gdi32.DeleteDC(dc)
        user32.ReleaseDC(hwnd, screen)


def loaded_runtime_modules(pid: int, root: Path, runtime_files: list):
    """Inspect actual loaded modules of the exact isolated process, read-only."""
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    psapi.EnumProcessModulesEx.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.HMODULE),
        wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), wintypes.DWORD]
    psapi.EnumProcessModulesEx.restype = wintypes.BOOL
    psapi.GetModuleFileNameExW.argtypes = [wintypes.HANDLE, wintypes.HMODULE, wintypes.LPWSTR, wintypes.DWORD]
    handle = kernel.OpenProcess(0x0400 | 0x0010, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        modules = (wintypes.HMODULE * 4096)()
        needed = wintypes.DWORD()
        if not psapi.EnumProcessModulesEx(handle, modules, ctypes.sizeof(modules), ctypes.byref(needed), 0x03):
            raise ctypes.WinError(ctypes.get_last_error())
        if needed.value > ctypes.sizeof(modules):
            raise RuntimeError("actual process module inventory exceeded its inspection buffer")
        paths = {}
        for module in modules[:needed.value // ctypes.sizeof(wintypes.HMODULE)]:
            text = ctypes.create_unicode_buffer(32768)
            if not psapi.GetModuleFileNameExW(handle, module, text, len(text)):
                raise ctypes.WinError(ctypes.get_last_error())
            path = Path(text.value)
            paths.setdefault(path.name.lower(), []).append(path)
        result = []
        for dependency in runtime_files:
            actual_paths = paths.get(dependency["name"].lower(), [])
            record = {"name": dependency["name"], "loaded": bool(actual_paths), "instances": []}
            for actual in actual_paths:
                if actual.resolve() != (root / dependency["name"]).resolve() or sha256(actual) != dependency["sha256"]:
                    raise RuntimeError("actual application loaded a different adjacent runtime image")
                record["instances"].append({"path": str(actual), "sha256": sha256(actual)})
            result.append(record)
        if not any(item["name"].lower() == "onnxruntime.dll" and item["loaded"] for item in result):
            raise RuntimeError("the actual application's statically imported ONNX runtime was not observed")
        return {"pid": pid, "method": "read-only EnumProcessModulesEx", "modules": result}
    finally:
        kernel.CloseHandle(handle)


def force_terminate(process: subprocess.Popen, root: Path, reason: str):
    if process.poll() is not None:
        raise RuntimeError("application already exited; a graceful exit cannot satisfy process-crash acceptance")
    killed_at = time.time_ns()
    killed = subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)],
        capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=30)
    process.wait(timeout=30)
    result = {"pid": process.pid, "wasRunningBeforeForceKill": True, "timeNs": killed_at,
        "mechanism": "Windows taskkill /F /T against the exact application PID",
        "taskkillExitCode": killed.returncode, "applicationExitCode": process.returncode,
        "taskkillOutput": killed.stdout + killed.stderr, "reason": reason,
        "normalStopOrWindowClose": False, "webComponentOnly": False}
    name = "force-termination.json" if reason == "reached independently recorded durable checkpoint" else f"cleanup-force-termination-{process.pid}.json"
    atomic_json(root / name, result)
    if killed.returncode != 0:
        raise RuntimeError("Windows did not confirm force termination of the entire application tree")
    return result


def wait_for_file(process, root: Path, filename: str, timeout: float):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for error in root.glob("*-failure.json"):
            raise RuntimeError(f"application reported {error.name}: {read_json(error)}")
        file = root / filename
        if file.exists():
            try:
                return read_json(file)
            except (OSError, ValueError):
                pass
        if process.poll() is not None:
            raise RuntimeError(f"application exited {process.returncode} before {filename}")
        time.sleep(0.05)
    raise TimeoutError(f"timed out waiting for {filename}")


def snapshot_persistent_files(root: Path):
    target = root / "after-force-kill"
    target.mkdir()
    portable = root / "portable"
    copied = {}
    for file in portable.rglob("*"):
        if file.is_file() and (file.name in ("concat.json", "whole-process-case.json")
                or ".sqlite3" in file.name or file.name.endswith(".sqlite")):
            relative = file.relative_to(portable)
            destination = target / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(file, destination)
            copied[str(relative)] = {"sha256": sha256(destination), "bytes": destination.stat().st_size}
    journal = portable / "bk-cat" / "agent-session" / "native-operations.sqlite3"
    with sqlite3.connect(f"file:{journal.as_posix()}?mode=ro", uri=True) as database:
        database.row_factory = sqlite3.Row
        tables = [row["name"] for row in database.execute("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'")]
        evidence = {}
        for table in tables:
            # Names come from the isolated database, never from external input.
            quoted = '"' + table.replace('"', '""') + '"'
            evidence[table] = [dict(row) for row in database.execute(f"SELECT * FROM {quoted}")]
    atomic_json(target / "journal-readonly-inspection.json", evidence)
    atomic_json(target / "copied-file-sha256.json", copied)
    return copied


def inspect_compaction_evidence(root: Path, fixture: Fixture):
    database_path = root / "portable" / "bk-cat" / "agent-session" / "pi-session.sqlite3"
    with sqlite3.connect(database_path.as_uri() + "?mode=ro", uri=True) as database:
        entries = [json.loads(row[0]) for row in database.execute("SELECT record FROM entries")]
        tasks = [json.loads(row[0]) for row in database.execute("SELECT record FROM tasks")]
    summaries = [entry for entry in entries if entry.get("kind") == "pi.compaction"]
    stored = json.dumps(summaries, ensure_ascii=False)
    for text in fixture.summary_texts:
        assert json.dumps(text, ensure_ascii=False)[1:-1] in stored, "original Pi did not persist the actual fixture summary"
    if fixture.compaction_requests:
        assert fixture.compaction_used_by_followup, "normal resumed inference did not use the original persisted summary"
        assert any(task.get("kind") == "pi.compaction" and task.get("state", {}).get("status") == "terminal"
            and task.get("state", {}).get("outcome", {}).get("status") == "completed" for task in tasks), "original compaction task did not reach successful terminal state"
    evidence = {"readOnlyDatabase": str(database_path), "summaryRequests": fixture.compaction_requests,
        "summaryEntryCount": len(summaries), "summaryEntries": summaries,
        "usedByFollowupInference": fixture.compaction_used_by_followup,
        "compactionTasks": [task for task in tasks if task.get("kind") == "pi.compaction"],
        "responseSource": "controlled loopback fixture; actual original Pi scheduling and SQLite persistence"}
    atomic_json(root / "original-compaction-evidence.json", evidence)
    return evidence


def bind_native_audit_to_submission(database, original: dict, actual: list[dict]):
    """Check original Pi rows; a native tool task ID is not a native request ID."""
    conversation = original["conversationId"]

    def checked_record(row):
        assert row is not None, "an audited original Pi record is missing"
        identity, indexed_conversation, raw = row[:3]
        record = json.loads(raw)
        assert identity == record.get("id") and indexed_conversation == record.get("conversationId"), "original Pi record identity differs from its index"
        assert indexed_conversation == conversation, "audited original Pi record belongs to another conversation"
        return record

    inputs = []
    for row in database.execute("SELECT id,conversation_id,record FROM submissions WHERE conversation_id=?", (conversation,)):
        record = checked_record(row)
        if record.get("type") == "input":
            assert type(record.get("entry")) is int, "an original input has no transcript boundary"
            inputs.append(record)
    source = [record for record in inputs if record["id"] == original["submissionId"]]
    assert len(source) == 1 and source[0].get("requestId") == original["taskId"], "the original native request and Pi submission differ"
    source = source[0]
    assert len({record["entry"] for record in inputs}) == len(inputs), "original inputs have ambiguous transcript boundaries"

    def task(identity: int, kind: str, version: int):
        row = database.execute("SELECT id,conversation_id,record,kind FROM tasks WHERE id=?", (identity,)).fetchone()
        record = checked_record(row)
        assert row[3] in (kind, json.dumps(kind)) and record.get("kind") == kind and record.get("version") == version, "audited task kind or original version differs"
        state = record.get("state", {})
        assert state.get("status") == "terminal" and state.get("outcome", {}).get("status") == "completed", "audited original task did not complete"
        return record

    bindings = []
    for audit in actual:
        assert audit["conversationId"] == conversation, "native audit belongs to another conversation"
        tool = task(audit["taskId"], "pi.tool", 4)
        tool_input = tool.get("input", {})
        assert tool_input.get("kind") == "model" and tool_input.get("callId") == audit["callId"], "native audit does not match its original tool invocation"
        assistant_id, owner_id = tool_input.get("assistant"), tool.get("owner")
        assert type(assistant_id) is int and type(owner_id) is int, "audited tool has no exact assistant and generation owner"
        assistant = checked_record(database.execute("SELECT id,conversation_id,record FROM entries WHERE id=?", (assistant_id,)).fetchone())
        assert assistant.get("kind") == "pi.assistant" and assistant.get("byTaskId") == owner_id, "tool assistant belongs to another generation"
        generation = task(owner_id, "pi.generation", 3)
        assert generation.get("state", {}).get("outcome", {}).get("result", {}).get("entryId") == assistant_id, "generation did not produce the audited assistant"
        # Pinned Pi assigns monotonically increasing record IDs. A user input
        # owns assistant/tool generations until the next input in that conversation.
        preceding = [record for record in inputs if record["entry"] < assistant_id]
        assert preceding, "audited assistant precedes every native input"
        owner_input = max(preceding, key=lambda record: record["entry"])
        assert owner_input["id"] == source["id"] and source["entry"] < owner_id < assistant_id < tool["id"], "audited tool belongs to a different original input"
        calls = [block for message in assistant.get("model", []) if message.get("role") == "assistant"
            for block in message.get("content", []) if isinstance(block, dict) and block.get("type") == "toolCall" and block.get("id") == audit["callId"]]
        assert len(calls) == 1 and calls[0].get("name") == audit["name"], "native audit differs from the exact original assistant tool call"
        bindings.append({"taskId": tool["id"], "callId": audit["callId"], "name": audit["name"],
            "assistantEntryId": assistant_id, "generationTaskId": owner_id,
            "submissionId": source["id"], "nativeRequestId": source["requestId"], "conversationId": conversation})
    return bindings


def sqlite_persistent_fingerprint(path: Path):
    # SQLite may leave all recent durable rows in the WAL after application
    # exit. Hash it together with the main file without checkpointing either.
    result = {}
    for name, candidate in (("main", path), ("wal", path.with_name(path.name + "-wal"))):
        present = candidate.exists()
        assert name != "main" or present, "the original Pi database is missing"
        result[name] = {"path": str(candidate), "present": present}
        if present:
            result[name].update({"bytes": candidate.stat().st_size, "sha256": sha256(candidate)})
    return result


def inspect_deep_continuation(root: Path, fixture: Fixture, recovery: dict):
    expected = {
        "recovery-read-current": ("project_get", "completed"),
        "recovery-new-plan-locked": ("edit_apply_guarded", "refused"),
        "recovery-read-after-unlock": ("project_get", "completed"),
        "recovery-new-plan-unlocked": ("edit_apply_guarded", "completed"),
        **{f"recovery-extra-read-{index}": ("project_get", "completed") for index in (1, 2, 3)},
    }
    log = (root / "recover-application.log").read_text(encoding="utf-8")
    pattern = r"bk-cat-tool-audit event=tool_result name=(\S+) outcome=(\S+) conversation=(\d+) task=(\d+) call_id=(\S+)"
    actual = [{"name": name, "outcome": outcome, "conversationId": int(conversation),
        "taskId": int(task), "callId": call} for name, outcome, conversation, task, call in re.findall(pattern, log)]
    assert len(actual) == 7 and len({item["callId"] for item in actual}) == 7, "expected seven distinct actual native tool results in the resumed task"
    original = recovery["beforeSnapshot"]["resumeTask"]
    for item in actual:
        assert expected.get(item["callId"]) == (item["name"], item["outcome"]), "native tool identity or actual outcome differs"
        assert item["conversationId"] == original["conversationId"], "tool result belongs to another native conversation"
    assert fixture.recovery_inference_requests == 8, "the original resumed task did not make eight normal inference requests"
    database_path = root / "portable" / "bk-cat" / "agent-session" / "pi-session.sqlite3"
    database_before = sqlite_persistent_fingerprint(database_path)
    shared_memory = database_path.with_name(database_path.name + "-shm")
    shared_memory_before = shared_memory.exists()
    with sqlite3.connect(database_path.as_uri() + "?mode=ro", uri=True) as database:
        database.execute("PRAGMA query_only=ON")
        records = [json.loads(row[0]) for row in database.execute("SELECT record FROM submissions")]
        inputs = [record for record in records if record.get("type") == "input"]
        bindings = bind_native_audit_to_submission(database, original, actual)
    database_after = sqlite_persistent_fingerprint(database_path)
    assert database_after == database_before, "original Pi main database or WAL changed during read-only evidence inspection"
    assert len(inputs) == 1 and inputs[0]["id"] == original["submissionId"] and inputs[0]["requestId"] == original["taskId"], "deep continuation replaced or resubmitted the original input"
    assert inputs[0]["status"] == "done", "the original input did not reach its actual completed status"
    evidence = {"sameOriginalSubmission": inputs[0], "normalRecoveryInferenceRequests": 8,
        "actualNativeToolResults": actual, "toolCallCount": 7, "originalPiTaskBindings": bindings,
        "readOnlyDatabase": str(database_path), "persistentDatabaseBefore": database_before,
        "persistentDatabaseAfter": database_after,
        "sharedMemoryPresentBefore": shared_memory_before, "sharedMemoryPresentAfter": shared_memory.exists(),
        "compactionRequestsSeparatelyCounted": fixture.compaction_requests,
        "oldInterruptedWriteReplayed": False,
        "scope": "actual restarted application, original Pi task and production native tools; controlled local model responses"}
    atomic_json(root / "deep-continuation-evidence.json", evidence)
    return evidence


def one_case(executable: Path, output: Path, stage: str, decision: str, timeout: float, deep_continuation: bool = False):
    root = output / (stage + "-" + decision)
    root.mkdir()
    binary = root / executable.name
    executable_digest = sha256(executable)
    # Every old run must remain immutable if a later compiler replaces or
    # rewrites its build output. Never hard-link acceptance evidence to it.
    with executable.open("rb") as source_stream, binary.open("xb") as target_stream:
        shutil.copyfileobj(source_stream, target_stream)
        target_stream.flush()
        os.fsync(target_stream.fileno())
    if sha256(binary) != executable_digest or sha256(executable) != executable_digest or os.path.samefile(executable, binary):
        raise RuntimeError("acceptance executable changed or was not independently copied")
    # Windows searches its system directory before PATH. Preserve the exact
    # adjacent application runtime in every isolated case; relying on PATH
    # selected the system's unrelated ONNX image on the acceptance computer.
    runtime_files = []
    for source in sorted(executable.parent.glob("*.dll")):
        if not source.is_file() or source.is_symlink():
            raise RuntimeError("acceptance runtime dependency must be a regular file")
        digest = sha256(source)
        target = root / source.name
        # Exclusive creation rejects an existing case file instead of replacing it.
        with source.open("rb") as source_stream, target.open("xb") as target_stream:
            shutil.copyfileobj(source_stream, target_stream)
            target_stream.flush()
            os.fsync(target_stream.fileno())
        if sha256(target) != digest or sha256(source) != digest:
            raise RuntimeError("runtime dependency changed during isolated staging")
        runtime_files.append({"name": source.name, "source": str(source),
            "sha256": digest, "bytes": target.stat().st_size})
    environment = os.environ.copy()
    environment["PATH"] = str(executable.parent) + os.pathsep + environment.get("PATH", "")
    for name in ("CONCAT_GPU", "SLINT_BACKEND", "SLINT_RENDERER"):
        environment.pop(name, None)
    run_id = secrets.token_hex(16)
    fixture = Fixture(root, stage, decision, run_id, deep_continuation)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), make_handler(fixture))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    arguments = [str(binary), "--mode", "crash", "--phase", "interrupt", "--stage", stage,
        "--decision", decision, "--fixture-port", str(server.server_port), "--run-id", run_id]
    atomic_json(root / "driver-case.json", {"sourceExecutable": str(executable), "executableSha256": sha256(binary),
        "arguments": arguments, "stage": stage, "decision": decision, "runId": run_id,
        "adjacentRuntimeFiles": runtime_files,
        "deepContinuationRequested": deep_continuation,
        "fixture": "deterministic OpenAI-compatible streaming response on bound loopback HTTP only"})
    process = None
    result = {"stage": stage, "decision": decision, "passed": False, "directory": str(root)}
    try:
        with (root / "interrupt-application.log").open("wb") as log:
            process = subprocess.Popen(arguments, cwd=root, env=environment, stdout=log, stderr=subprocess.STDOUT)
            atomic_json(output / "execution-state.json", {"phase": "interrupt", "directory": str(root), "applicationPid": process.pid, "driverPid": os.getpid()})
            checkpoint = wait_for_file(process, root, "checkpoint.json", timeout)
            assert checkpoint["pid"] == process.pid and checkpoint["stage"] == stage
            assert checkpoint["requiresExternalForceTermination"] is True
            assert fixture.requests == 1 and not fixture.failures, "unexpected request history before crash"
            result["loadedRuntime"] = loaded_runtime_modules(process.pid, root, runtime_files)
            atomic_json(root / "actual-loaded-runtime.json", result["loadedRuntime"])
            screen = screenshot_window(process.pid, root / "checkpoint-os-window.png")
            atomic_json(root / "checkpoint-os-window.json", screen)
            termination = force_terminate(process, root, "reached independently recorded durable checkpoint")
            result["termination"] = termination
            process = None
        result["persistentFilesAfterKill"] = snapshot_persistent_files(root)
        fixture.recover()
        arguments[4] = "recover"
        with (root / "recover-application.log").open("wb") as log:
            process = subprocess.Popen(arguments, cwd=root, env=environment, stdout=log, stderr=subprocess.STDOUT)
            atomic_json(output / "execution-state.json", {"phase": "recover", "directory": str(root), "applicationPid": process.pid, "driverPid": os.getpid()})
            report = wait_for_file(process, root, "recovery-report.json", timeout)
            process.wait(timeout=45)
            assert process.returncode == 0, f"restarted application exited {process.returncode}"
            assert report["passed"] is True and report["oldPid"] != report["newPid"]
            assert not fixture.failures, f"controlled fixture failures: {fixture.failures}"
            expected_task_requests = 9 if deep_continuation else (1 if decision == "discard" else (6 if stage in ("intent", "applied", "receipt") else 3))
            expected = expected_task_requests + fixture.compaction_requests
            assert fixture.requests == expected, f"expected {expected_task_requests} task requests plus {fixture.compaction_requests} actual compaction requests, observed {fixture.requests}"
            assert fixture.recovery_inference_requests == expected_task_requests - 1, "unexpected normal resumed inference request count"
            result["originalCompaction"] = inspect_compaction_evidence(root, fixture)
            result["recovery"] = report
            if deep_continuation:
                result["deepContinuation"] = inspect_deep_continuation(root, fixture, report)
            process = None
        result["passed"] = True
    except Exception as error:
        result["error"] = str(error)
        result["traceback"] = traceback.format_exc()
    finally:
        if process is not None and process.poll() is None:
            try:
                force_terminate(process, root, "failed acceptance cleanup; does not count as a passed checkpoint")
            except Exception as error:
                result["cleanupProcessError"] = str(error)
        if (root / "whole-process-case.json").exists():
            cleanup = arguments.copy()
            cleanup[4] = "cleanup"
            try:
                with (root / "credential-cleanup.log").open("wb") as log:
                    cleaned = subprocess.run(cleanup, cwd=root, env=environment, stdout=log, stderr=subprocess.STDOUT, timeout=45)
                result["credentialCleanupExitCode"] = cleaned.returncode
                if cleaned.returncode != 0:
                    raise RuntimeError("isolated test credential cleanup did not verify")
            except Exception as error:
                result["passed"] = False
                result["cleanupCredentialError"] = str(error)
                result["cleanupCredentialCommandArguments"] = cleanup
                result["cleanupCredentialWorkingDirectory"] = str(root)
        server.shutdown()
        server.server_close()
        try:
            for dependency in runtime_files:
                if sha256(root / dependency["name"]) != dependency["sha256"] or sha256(Path(dependency["source"])) != dependency["sha256"]:
                    raise RuntimeError("runtime dependency changed during actual application acceptance")
            result["adjacentRuntimeFiles"] = runtime_files
        except Exception as error:
            result["passed"] = False
            result["runtimeIntegrityError"] = str(error)
        atomic_json(root / "driver-result.json", result)
    return result


def main():
    # Assertions below are acceptance gates, so reject optimization before
    # creating any directory, fixture, credential or application process.
    if not __debug__:
        raise RuntimeError("acceptance assertions were disabled; use Python without -O/PYTHONOPTIMIZE")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--stages", nargs="+", choices=STAGES, default=list(STAGES))
    parser.add_argument("--decisions", nargs="+", choices=DECISIONS, default=list(DECISIONS))
    parser.add_argument("--timeout", type=float, default=180)
    parser.add_argument("--deep-continuation", action="store_true",
        help="separate intent/continue case with eight normal resumed requests and seven actual native tool calls")
    arguments = parser.parse_args()
    if arguments.deep_continuation and (arguments.stages != ["intent"] or arguments.decisions != ["continue"]):
        parser.error("deep continuation is a separate intent/continue case; it cannot alter the five-state matrix")
    if os.name != "nt":
        parser.error("this actual window/TerminateProcess acceptance driver requires Windows")
    executable = arguments.executable.resolve(strict=True)
    output = arguments.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    results = []
    try:
        for stage in arguments.stages:
            for decision in arguments.decisions:
                print(f"START {stage} {decision}", flush=True)
                result = one_case(executable, output, stage, decision, arguments.timeout, arguments.deep_continuation)
                results.append(result)
                print(f"RESULT {stage} {decision} passed={result['passed']} directory={result['directory']}", flush=True)
                atomic_json(output / "matrix-progress.json", {"driverPid": os.getpid(), "results": results})
                if not result["passed"]:
                    # Preserve the first failed case and its cleanup record;
                    # do not begin another application before it is reviewed.
                    return 1
    finally:
        passed = len(results) == len(arguments.stages) * len(arguments.decisions) and all(result["passed"] for result in results)
        atomic_json(output / "matrix-report.json", {"passed": passed, "results": results,
            "fullFiveByTwoMatrix": list(arguments.stages) == list(STAGES) and list(arguments.decisions) == list(DECISIONS),
            "sourceExecutable": str(executable), "executableSha256": sha256(executable),
            "realWindowAndEntireApplicationCrash": True, "controlledLocalModel": True,
            "deepContinuationRequested": arguments.deep_continuation,
            "realExternalProviderVerification": False, "androidDeviceAcceptance": False})
        atomic_json(output / "execution-state.json", {"phase": "complete", "driverPid": os.getpid(), "passed": passed})
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())

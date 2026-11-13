#!/usr/bin/env python3
"""Reproduce the fixed APK bundle failure in an explicitly supplied old engine.

This offline diagnostic never installs or replaces a device component. It uses
no native application bridge and cannot claim native recovery or product success.
Generated bundles, browser profiles and HTML stay in the caller's fresh output
directory; only the small results and manifests are intended for source control.
"""

import argparse
import hashlib
import html
import json
from pathlib import Path
import re
import subprocess
import zipfile


APK_SHA256 = "2b749e7e0d7896c3b1c9694ea5afee7591f9544f22e1581d19f4216c492afb5c"
MANIFEST_SHA256 = "f5c111f099dd7cbe46b331220f97c30d95e96d7956304a22004d7f35188046b1"
EXPECTED_TRANSFORM = "431b891fd7138de836edb2849226c877aa10291c6a1153badffdd59ce43b1726"
PRELUDE = """<!doctype html><meta charset="utf-8"><pre id="out"></pre>
<script>
function record(value) { document.getElementById('out').textContent += JSON.stringify(value) + '\\n'; }
window.addEventListener('error', function(e) { record({stage:'uncaught',error:e.message,source:e.filename.split('/').pop(),line:e.lineno,column:e.colno}); });
record({stage:'features',userAgent:navigator.userAgent,randomUUID:typeof crypto.randomUUID,throwIfAborted:typeof AbortSignal.prototype.throwIfAborted,abortReason:'reason' in AbortSignal.prototype,arrayAt:typeof Array.prototype.at,objectHasOwn:typeof Object.hasOwn,promiseWithResolvers:typeof Promise.withResolvers});
window.BKCatNative={postMessage:function(message){record({stage:'bridge',message:JSON.parse(message)})},addEventListener:function(){},removeEventListener:function(){}};
</script>
"""
POSTLUDE = "<script>record({stage:'complete',runtime:typeof BKCatRuntime,bootstrap:typeof BKCatBootstrap});</script>\n"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def checked_run(command, timeout=30):
    return subprocess.run(command, check=True, capture_output=True, text=True, timeout=timeout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apk", type=Path, required=True)
    parser.add_argument("--browser", type=Path, required=True)
    parser.add_argument("--esbuild", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    apk, browser, esbuild, out = (getattr(args, key).resolve() for key in ("apk", "browser", "esbuild", "out"))
    if out.exists():
        raise RuntimeError("Use a fresh output directory; no existing diagnostic will be overwritten")
    if sha256(apk.read_bytes()) != APK_SHA256:
        raise RuntimeError("APK differs from the actual tested f687/9765 artifact")
    engine_version = checked_run([str(browser), "--version"]).stdout.strip()
    if engine_version != "Chromium 83.0.4103.0":
        raise RuntimeError("This reproduction is pinned to Chromium 83.0.4103.0")
    if checked_run([str(esbuild), "--version"]).stdout.strip() != "0.28.2":
        raise RuntimeError("Use the original reference's pinned esbuild 0.28.2")
    out.mkdir(parents=True)
    with zipfile.ZipFile(apk) as archive:
        library = archive.read("lib/x86_64/libconcat_android.so")
    start = library.index(b'{\n  "piCommit":')
    manifest_text = library[start:start + 10000].decode("utf-8", errors="replace")
    manifest, end = json.JSONDecoder().raw_decode(manifest_text)
    manifest_bytes = (manifest_text[:end] + "\n").encode()
    if sha256(manifest_bytes) != MANIFEST_SHA256:
        raise RuntimeError("Embedded manifest differs from the tested installation")
    (out / "runtime-manifest.json").write_bytes(manifest_bytes)
    needles = {"runtime.js": b"// BK CAT controlled runtime.",
               "bootstrap.js": b"var BKCatBootstrap =", "index.html": b"<!doctype html>"}
    for name, needle in needles.items():
        offset = library.index(needle)
        content = library[offset:offset + manifest["files"][name]["bytes"]]
        if sha256(content) != manifest["files"][name]["sha256"]:
            raise RuntimeError("Embedded resource integrity mismatch: " + name)
        (out / name).write_bytes(content)
    conversion = checked_run([str(esbuild), str(out / "runtime.js"), "--target=chrome83",
                              "--outfile=" + str(out / "runtime-chrome83.js")])
    (out / "syntax-transform.log").write_text(conversion.stdout + conversion.stderr)
    if sha256((out / "runtime-chrome83.js").read_bytes()) != EXPECTED_TRANSFORM:
        raise RuntimeError("Syntax-only diagnostic output differs from the recorded bytes")
    bootstrap = (out / "bootstrap.js").read_text()
    results = {}
    for name, filename, expected_error in [
        ("original-inline", "runtime.js", "Uncaught SyntaxError: Unexpected token '('"),
        ("syntax-only-inline", "runtime-chrome83.js", "Uncaught ReferenceError: FinalizationRegistry is not defined"),
    ]:
        runtime = (out / filename).read_text()
        if "</script" in (runtime + bootstrap).lower():
            raise RuntimeError("Inline diagnostic would change script parsing")
        page = out / (name + ".html")
        page.write_text(PRELUDE + "<script>" + runtime + "</script><script>" + bootstrap + "</script>\n" + POSTLUDE)
        command = [str(browser), "--headless", "--no-sandbox", "--disable-gpu",
                   "--disable-background-networking", "--disable-extensions",
                   "--user-data-dir=" + str(out / "profiles" / name), "--dump-dom", page.as_uri()]
        execution = checked_run(command)
        (out / (name + "-dom.html")).write_text(execution.stdout)
        (out / (name + "-stderr.log")).write_text(execution.stderr)
        match = re.search(r'<pre id="out">(.*?)</pre>', execution.stdout, re.S)
        if not match:
            raise RuntimeError("Browser did not execute the local diagnostic document")
        events = [json.loads(line) for line in html.unescape(match.group(1)).splitlines()]
        if [event["error"] for event in events if event["stage"] == "uncaught"] != [expected_error]:
            raise RuntimeError("The recorded failure was not reproduced: " + name)
        if any(event["stage"] == "bridge" for event in events):
            raise RuntimeError("Unexpected bridge event; this is not the recorded startup failure")
        if events[-1] != {"stage": "complete", "runtime": "undefined", "bootstrap": "object"}:
            raise RuntimeError("Unexpected final diagnostic state")
        results[name] = events
    result_bytes = (json.dumps(results, ensure_ascii=False, indent=2) + "\n").encode()
    (out / "old-engine-reproduction.json").write_bytes(result_bytes)
    print(json.dumps({"engine": engine_version, "apk_sha256": APK_SHA256,
                      "result_sha256": sha256(result_bytes), "failures_reproduced": list(results)}, indent=2))


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# SPDX-FileCopyrightText: 2026 Jareer Abdullah

"""Derive BK CUT display assets from the exact approved raster, without redrawing."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import math
from pathlib import Path

from PIL import Image, ImageChops, __version__ as pillow_version

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets/brand/bkcut.png"
SOURCE_SHA256 = "2d0d0775b513cb417ec48507cce54b9c72b726a94f310eb16fec77d3fe5c23e5"
# Reviewed bounds around the upper graphic. The lettering begins below this
# rectangle and remains unchanged in the complete logo used at large sizes.
MARK_CROP = (128, 128, 1174, 914)
# Desktop/legacy icons use nearly all of the available square. Adaptive
# icons use Android's 108dp layer and the central 66dp circular safe zone.
ICON_FILL = 0.92
ANDROID_ADAPTIVE_LAYER_DP = 108
ANDROID_ADAPTIVE_SAFE_RADIUS_DP = 32.0
ICON_SIZES = (16, 32, 64, 128, 256, 512)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
ANDROID_SIZES = {"mdpi": 48, "hdpi": 72, "xhdpi": 96, "xxhdpi": 144, "xxxhdpi": 192}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def centered_mark(source: Image.Image) -> Image.Image:
    """Retain the approved graphic and trim only the transparent fringe."""
    mark = source.crop(MARK_CROP)
    bounds = mark.getchannel("A").point(lambda a: 255 if a >= 8 else 0).getbbox()
    if bounds is None:
        raise ValueError("The approved mark is empty.")
    return mark.crop(bounds)


def render_mark(mark: Image.Image, size: int, longest_edge: float) -> Image.Image:
    scale = longest_edge / max(mark.size)
    extent = tuple(max(1, round(n * scale)) for n in mark.size)
    resized = mark.resize(extent, Image.Resampling.LANCZOS)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.alpha_composite(resized, ((size - resized.width) // 2, (size - resized.height) // 2))
    return canvas


def icon(source: Image.Image, size: int) -> Image.Image:
    return render_mark(centered_mark(source), size, round(size * ICON_FILL))


def adaptive_foreground(source: Image.Image, density_size: int) -> Image.Image:
    mark = centered_mark(source)
    width, height = mark.size
    alpha = mark.getchannel("A")
    # Bound actual visible pixels, not the corners of the transparent rectangle.
    radius = max(
        math.hypot(x + 0.5 - width / 2, y + 0.5 - height / 2)
        for y in range(height) for x in range(width) if alpha.getpixel((x, y)) >= 8
    )
    density = density_size / 48
    layer_size = round(ANDROID_ADAPTIVE_LAYER_DP * density)
    max_edge = max(mark.size) * ANDROID_ADAPTIVE_SAFE_RADIUS_DP * density / radius
    return render_mark(mark, layer_size, min(66 * density, max_edge))


def png_bytes(image: Image.Image) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG", optimize=False, compress_level=9)
    return buffer.getvalue()


def expected_assets(source: Image.Image) -> dict[str, bytes]:
    assets = {
        f"assets/icons/concat_logo_{size}.png": png_bytes(icon(source, size))
        for size in ICON_SIZES
    }
    # Keep existing resource filenames, which are internal build references.
    assets["assets/concat_logo_512.png"] = assets["assets/icons/concat_logo_512.png"]
    assets["assets/brand/bk-cut-full-512.png"] = png_bytes(
        source.resize((512, 512), Image.Resampling.LANCZOS)
    )
    for density, size in ANDROID_SIZES.items():
        assets[f"src/crates/concat-android/res/mipmap-{density}/bk_cut_launcher.png"] = png_bytes(
            icon(source, size)
        )
        assets[f"src/crates/concat-android/res/mipmap-{density}/bk_cut_launcher_foreground.png"] = png_bytes(
            adaptive_foreground(source, size)
        )
    assets["src/crates/concat-android/res/mipmap-anydpi-v26/bk_cut_launcher.xml"] = (
        '<?xml version="1.0" encoding="utf-8"?>\n'
        '<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">\n'
        '    <background android:drawable="@android:color/white" />\n'
        '    <foreground android:drawable="@mipmap/bk_cut_launcher_foreground" />\n'
        '</adaptive-icon>\n'
    ).encode("utf-8")
    buffer = io.BytesIO()
    images = [icon(source, size) for size in ICO_SIZES]
    images[-1].save(
        buffer,
        format="ICO",
        sizes=[(size, size) for size in ICO_SIZES],
        append_images=images[:-1],
    )
    assets["assets/icons/bk-cut-app.ico"] = buffer.getvalue()
    return assets


def verify_geometry(assets: dict[str, bytes]) -> None:
    """Independently guard visible fill and the adaptive mask's safe circle."""
    for name, data in assets.items():
        if not name.endswith(".png"):
            continue
        if not (name.startswith("assets/icons/") or "mipmap-" in name):
            continue
        with Image.open(io.BytesIO(data)) as image:
            alpha = image.convert("RGBA").getchannel("A")
            bounds = alpha.point(lambda value: 255 if value >= 128 else 0).getbbox()
            if bounds is None:
                raise ValueError(f"Empty icon: {name}")
            w, h = image.size
            width = bounds[2] - bounds[0]
            if "foreground" in name:
                radius = max(
                    math.hypot(x + 0.5 - w / 2, y + 0.5 - h / 2)
                    for y in range(h) for x in range(w) if alpha.getpixel((x, y)) >= 8
                ) * ANDROID_ADAPTIVE_LAYER_DP / w
                if radius > 33 or width * ANDROID_ADAPTIVE_LAYER_DP / w < 48:
                    raise ValueError(f"Unsafe or undersized adaptive foreground: {name}")
            elif width / w < (0.75 if w <= 32 else 0.85):
                raise ValueError(f"Excessive transparent padding: {name}")


def same_pixels(actual: Path, expected: bytes) -> bool:
    """Check raster content, independently of PNG compression-library details."""
    if not actual.is_file():
        return False
    if actual.suffix == ".xml":
        return actual.read_bytes().replace(b"\r\n", b"\n") == expected
    with Image.open(actual) as current, Image.open(io.BytesIO(expected)) as target:
        if actual.suffix == ".ico":
            expected_sizes = set(target.ico.sizes())
            if set(current.ico.sizes()) != expected_sizes:
                return False
            pairs = [(current.ico.getimage(size), target.ico.getimage(size)) for size in expected_sizes]
        else:
            pairs = [(current, target)]
        for left, right in pairs:
            if left.size != right.size:
                return False
            difference = ImageChops.difference(left.convert("RGBA"), right.convert("RGBA"))
            if any(channel.getbbox() for channel in difference.split()):
                return False
    return True


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="Verify existing derived raster content.")
    args = parser.parse_args()
    original = SOURCE.read_bytes()
    if digest(original) != SOURCE_SHA256:
        raise SystemExit("The approved original is missing or its SHA-256 differs.")
    with Image.open(io.BytesIO(original)) as opened:
        if opened.size != (1254, 1254) or opened.mode != "RGBA":
            raise SystemExit("The approved original must be the 1254 x 1254 RGBA source.")
        source = opened.copy()
    assets = expected_assets(source)
    verify_geometry(assets)
    if args.check:
        incorrect = [name for name, data in assets.items() if not same_pixels(ROOT / name, data)]
        if incorrect:
            raise SystemExit("Brand assets differ from their approved source: " + ", ".join(incorrect))
        print(f"Verified the original and {len(assets)} derived brand resources.")
        return
    for name, data in assets.items():
        path = ROOT / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    manifest = {
        "brand": "BK CUT",
        "original": {"path": "assets/brand/bkcut.png", "sha256": SOURCE_SHA256, "width": 1254, "height": 1254, "mode": "RGBA"},
        "smallIcon": {"sourceCrop": list(MARK_CROP), "maximumCanvasFraction": ICON_FILL, "resampling": "LANCZOS", "background": "transparent"},
        "androidAdaptive": {"layerDp": ANDROID_ADAPTIVE_LAYER_DP, "safeRadiusDp": ANDROID_ADAPTIVE_SAFE_RADIUS_DP,
                            "background": "@android:color/white", "foreground": "@mipmap/bk_cut_launcher_foreground"},
        "largeLogo": "assets/brand/bk-cut-full-512.png",
        "pillowVersion": pillow_version,
        "outputs": [{"path": name, "bytes": len(data), "sha256": digest(data)} for name, data in sorted(assets.items())],
    }
    (ROOT / "assets/brand/bk-cut-assets.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"Generated {len(assets)} brand resources from the unchanged approved original.")


if __name__ == "__main__":
    main()

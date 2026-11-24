#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# SPDX-FileCopyrightText: 2026 Jareer Abdullah

"""Derive BK CUT display assets from the exact approved raster, without redrawing."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
from pathlib import Path

from PIL import Image, ImageChops, __version__ as pillow_version

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets/brand/bkcut.png"
SOURCE_SHA256 = "2d0d0775b513cb417ec48507cce54b9c72b726a94f310eb16fec77d3fe5c23e5"
# Reviewed bounds around the upper graphic. The lettering begins below this
# rectangle and remains unchanged in the complete logo used at large sizes.
MARK_CROP = (128, 128, 1174, 914)
ICON_FILL = 0.76
ICON_SIZES = (16, 32, 64, 128, 256, 512)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
ANDROID_SIZES = {"mdpi": 48, "hdpi": 72, "xhdpi": 96, "xxhdpi": 144, "xxxhdpi": 192}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def icon(source: Image.Image, size: int) -> Image.Image:
    mark = source.crop(MARK_CROP)
    scale = (round(size * ICON_FILL)) / max(mark.size)
    extent = tuple(max(1, round(n * scale)) for n in mark.size)
    mark = mark.resize(extent, Image.Resampling.LANCZOS)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.alpha_composite(mark, ((size - mark.width) // 2, (size - mark.height) // 2))
    return canvas


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
        assets[f"src/crates/concat-android/res/mipmap-{density}/ic_launcher.png"] = png_bytes(
            icon(source, size)
        )
    buffer = io.BytesIO()
    images = [icon(source, size) for size in ICO_SIZES]
    images[-1].save(
        buffer,
        format="ICO",
        sizes=[(size, size) for size in ICO_SIZES],
        append_images=images[:-1],
    )
    assets["assets/icons/concat.ico"] = buffer.getvalue()
    return assets


def same_pixels(actual: Path, expected: bytes) -> bool:
    """Check raster content, independently of PNG compression-library details."""
    if not actual.is_file():
        return False
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

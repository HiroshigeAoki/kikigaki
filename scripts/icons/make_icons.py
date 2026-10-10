# /// script
# dependencies = ["numpy==2.5.3", "pillow==12.3.0"]
# ///
"""Regenerate approved app and B2 tray assets; run with uv run."""
import argparse
import io
import struct
from pathlib import Path
import tempfile
import unittest

import numpy as np
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[2]
NAVY = np.array([5, 26, 59.0])
CREAM = np.array([245, 228, 198.0])


def soft_alpha(rgb, background, foreground):
    delta = foreground - background
    return np.clip(((rgb - background) @ delta) / (delta @ delta), 0, 1)


def slash_cut(canvas, half_width):
    yy, xx = np.mgrid[0:canvas, 0:canvas]
    return np.where(np.abs((yy - xx) - 0.5) <= half_width, 0.0, 1.0)


def pack(rgb, alpha):
    colors = np.broadcast_to(rgb, (*alpha.shape, 3))
    return np.clip(np.rint(np.dstack([colors, alpha * 255])), 0, 255).astype("uint8")


class HelperTests(unittest.TestCase):
    def test_projection(self):
        rgb = np.array([[NAVY, (NAVY + CREAM) / 2, CREAM]])
        np.testing.assert_allclose(soft_alpha(rgb, NAVY, CREAM), [[0, 0.5, 1]])
        np.testing.assert_allclose(soft_alpha(np.array([[NAVY * 0, NAVY]]), np.zeros(3), NAVY), [[0, 1]])

    def test_slash(self):
        expected = np.array([[0, 1, 1, 1], [0, 0, 1, 1], [1, 0, 0, 1], [1, 1, 0, 0]])
        np.testing.assert_array_equal(slash_cut(4, 0.5), expected)
        mask = np.full((4, 4), 0.6)
        np.testing.assert_array_equal(mask * slash_cut(4, 0.5), expected * 0.6)

    def test_template(self):
        result = pack((0, 0, 0), np.array([[0, 0.5, 1]]))
        np.testing.assert_array_equal(result[..., :3], np.zeros((1, 3, 3)))
        np.testing.assert_array_equal(result[..., 3], [[0, 128, 255]])
        self.assertEqual(result.dtype, np.uint8)


def resize_rgba(image, size):
    # Filter premultiplied colors to avoid dark fringes at transparent edges.
    return image.convert("RGBa").resize(size, Image.Resampling.LANCZOS).convert("RGBA")


def build_master(src):
    alpha = soft_alpha(src, np.zeros(3), NAVY)
    alpha[alpha < 0.02] = 0
    rgb = np.where((alpha < 0.999)[..., None], NAVY, src)
    ys, xs = np.where(alpha > 0.5)
    box = (xs.min(), ys.min(), xs.max() + 1, ys.max() + 1)
    body = resize_rgba(Image.fromarray(pack(rgb, alpha)).crop(box), (824, 824))
    master = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    master.alpha_composite(body, (100, 100))
    return master


def glyph_alpha_full(src):
    alpha = soft_alpha(src, NAVY, CREAM)
    r, g = src[..., 0], src[..., 1]
    alpha[(r > 120) & (g < 130) & (r - g > 60)] = 0
    crop = alpha[200:1025, 270:940]
    ys, xs = np.where(crop > 0.3)
    return crop[ys.min():ys.max() + 1, xs.min():xs.max() + 1]


def glyph_mask(glyph, canvas, height, dy=0.0):
    ss = 4
    width = max(1, round(glyph.width * height / glyph.height))
    big = glyph.resize((width * ss, round(height * ss)), Image.Resampling.LANCZOS)
    cv = Image.new("L", (canvas * ss, canvas * ss), 0)
    cv.paste(big, ((canvas * ss - big.width) // 2,
                   int((canvas * ss - big.height) / 2 + dy * ss)))
    return np.asarray(cv.resize((canvas, canvas), Image.Resampling.BOX)).astype(float) / 255


def disk(canvas, cx, cy, radius):
    yy, xx = np.mgrid[0:canvas, 0:canvas]
    return np.clip(radius + 0.5 - np.hypot(xx + 0.5 - cx, yy + 0.5 - cy), 0, 1)


def make(glyph, state):
    canvas = 36
    if state == "finalizing":
        mask = glyph_mask(glyph, canvas, 26, dy=-3)
        for i in (-1, 0, 1):
            mask = np.maximum(mask, disk(canvas, 18 + i * 8, 33, 1.8))
    else:
        mask = glyph_mask(glyph, canvas, 32)
        if state == "loading":
            mask *= 0.45
        elif state == "disconnected":
            # Approved B2 uses half-width 2: a four-pixel diagonal band.
            mask *= slash_cut(canvas, 2)
    color = (229, 59, 47) if state == "recording" else (0, 0, 0)
    return Image.fromarray(pack(color, mask))


def social_preview(master):
    # Cream ground so the navy icon body reads as a shape, not a ghost outline.
    preview = Image.new("RGB", (1280, 640), tuple(CREAM.astype(int)))
    icon = resize_rgba(master, (360, 360))
    preview.paste(icon, (60, 140), icon)
    draw = ImageDraw.Draw(preview)
    # Pillow's embedded scalable default keeps rendering independent of host fonts.
    draw.text((460, 245), "kikigaki", font=ImageFont.load_default(size=88),
              fill=tuple(NAVY.astype(int)))
    draw.text((465, 355), "Japanese voice typing for macOS",
              font=ImageFont.load_default(size=30), fill=(84, 96, 120))
    return preview


def render(destination):
    with Image.open(ROOT / "docs/branding/kikigaki-icon-source.png") as source:
        src = np.asarray(source.convert("RGB")).astype(float)
    master = build_master(src)
    glyph = Image.fromarray((glyph_alpha_full(src) * 255).astype("uint8"))
    outputs = {
        "docs/branding/kikigaki-icon-master-1024.png": master,
        "docs/branding/social-preview.png": social_preview(master),
    }
    for name, size in (("32x32.png", 32), ("128x128.png", 128),
                       ("128x128@2x.png", 256), ("icon.png", 512)):
        outputs[f"crates/kikigaki/icons/{name}"] = resize_rgba(master, (size, size))
    for state in ("idle", "loading", "finalizing", "disconnected", "recording"):
        outputs[f"crates/kikigaki/icons/tray/{state}.png"] = make(glyph, state)
    for name, image in outputs.items():
        path = destination / name
        path.parent.mkdir(parents=True, exist_ok=True)
        image.save(path, format="PNG", optimize=False, compress_level=9)
    icns = "crates/kikigaki/icons/icon.icns"
    master.save(destination / icns, format="ICNS", append_images=[
        resize_rgba(master, (size, size)) for size in (16, 32, 64, 128, 256, 512)
    ])
    # Pillow 12.3 omits the 16px 1x entry; add standard icp4 and rebuild the TOC.
    path = destination / icns
    data = path.read_bytes()
    entries = []
    offset = 8
    while offset < len(data):
        tag, length = struct.unpack_from(">4sI", data, offset)
        if tag != b"TOC ":
            entries.append((tag, data[offset + 8:offset + length]))
        offset += length
    small = io.BytesIO()
    resize_rgba(master, (16, 16)).save(small, format="PNG", optimize=False, compress_level=9)
    entries.append((b"icp4", small.getvalue()))
    toc = b"".join(struct.pack(">4sI", tag, len(payload) + 8) for tag, payload in entries)
    chunks = struct.pack(">4sI", b"TOC ", len(toc) + 8) + toc
    chunks += b"".join(struct.pack(">4sI", tag, len(payload) + 8) + payload
                       for tag, payload in entries)
    path.write_bytes(struct.pack(">4sI", b"icns", len(chunks) + 8) + chunks)
    return [*outputs, icns]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--check", action="store_true")
    modes.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(HelperTests))
        return 0 if result.wasSuccessful() else 1
    if args.check:
        with tempfile.TemporaryDirectory(prefix="kikigaki-icons-") as directory:
            destination = Path(directory)
            stale = []
            for name in render(destination):
                committed = ROOT / name
                if not committed.exists():
                    stale.append(f"missing: {name}")
                elif committed.read_bytes() != (destination / name).read_bytes():
                    stale.append(f"stale: {name}")
            if stale:
                print("\n".join(stale))
                return 1
        print("Icon assets are current (12 files).")
    else:
        print(f"Wrote {len(render(ROOT))} icon assets.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

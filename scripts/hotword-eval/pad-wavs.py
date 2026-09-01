#!/usr/bin/env python3
"""Pad 16 kHz mono PCM16 WAVs with leading/trailing silence.

The ReazonSpeech offline decode truncates word onsets when speech starts
almost immediately (TTS output has only ~80 ms of leading silence, and e.g.
クバネティス decoded as キス without padding). 400 ms of silence at both ends
restores the onsets. Usage:

    pad-wavs.py --src DIR --dst DIR [--pad-ms 400]
"""

from __future__ import annotations

import argparse
import struct
import sys
import wave
from pathlib import Path


def pad_dir(src: Path, dst: Path, pad_ms: int) -> int:
    dst.mkdir(parents=True, exist_ok=True)
    pad = b"\x00\x00" * (16_000 * pad_ms // 1000)
    count = 0
    for wav_path in sorted(src.glob("*.wav")):
        with wave.open(str(wav_path)) as reader:
            if (
                reader.getnchannels() != 1
                or reader.getsampwidth() != 2
                or reader.getframerate() != 16_000
            ):
                raise SystemExit(f"{wav_path}: expected mono PCM16 16 kHz")
            frames = reader.readframes(reader.getnframes())
        with wave.open(str(dst / wav_path.name), "wb") as writer:
            writer.setnchannels(1)
            writer.setsampwidth(2)
            writer.setframerate(16_000)
            writer.writeframes(pad + frames + pad)
        count += 1
    if count == 0:
        raise SystemExit(f"no .wav files found in {src}")
    return count


def self_test() -> int:
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "src"
        dst = Path(tmp) / "dst"
        src.mkdir()
        with wave.open(str(src / "a.wav"), "wb") as writer:
            writer.setnchannels(1)
            writer.setsampwidth(2)
            writer.setframerate(16_000)
            writer.writeframes(struct.pack("<4h", 100, -100, 100, -100))
        pad_dir(src, dst, 100)
        with wave.open(str(dst / "a.wav")) as reader:
            frames = reader.getnframes()
        expected = 4 + 2 * (16_000 * 100 // 1000)
        assert frames == expected, f"expected {expected} frames, got {frames}"
    print("OK")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--src", type=Path)
    parser.add_argument("--dst", type=Path)
    parser.add_argument("--pad-ms", type=int, default=400)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if args.src is None or args.dst is None:
        parser.error("--src and --dst are required unless --self-test")
    count = pad_dir(args.src, args.dst, args.pad_ms)
    print(f"padded {count} WAVs with {args.pad_ms} ms silence into {args.dst}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

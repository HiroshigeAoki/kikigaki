#!/usr/bin/env python3
"""Synthesize a sentencepiece-style BPE vocabulary from tokens.txt."""

from __future__ import annotations

import argparse
import hashlib
import io
import sys
import unittest
from pathlib import Path
from typing import TextIO


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = ROOT / "scripts" / "hotword-eval" / "out" / "bpe.vocab"


class GenerationError(RuntimeError):
    """A safe, expected generation failure with a user-facing message."""


def load_pieces(rows: TextIO, source_name: str) -> list[str]:
    pieces: list[str] = []
    seen: set[str] = set()
    for line_number, line in enumerate(rows, 1):
        line = line.rstrip("\r\n")
        if " " not in line:
            raise GenerationError(
                f"{source_name}:{line_number}: missing space separator"
            )
        piece, token_id = line.split(" ", 1)
        if not piece or not token_id or " " in token_id:
            raise GenerationError(
                f"{source_name}:{line_number}: malformed token line; "
                "expected <piece> <id>"
            )
        if piece in seen:
            raise GenerationError(
                f"{source_name}:{line_number}: duplicate piece {piece!r}"
            )
        seen.add(piece)
        pieces.append(piece)
    return pieces


def render_vocab(pieces: list[str]) -> bytes:
    lines = []
    for index, piece in enumerate(pieces):
        score = "0.0" if index == 0 else f"{-index / 10:.1f}"
        lines.append(f"{piece}\t{score}\n")
    return "".join(lines).encode("utf-8")


def generate(tokens: Path, output: Path) -> int:
    try:
        with tokens.open(encoding="utf-8") as rows:
            pieces = load_pieces(rows, str(tokens))
    except OSError as error:
        raise GenerationError(f"cannot read {tokens}: {error}") from error

    contents = render_vocab(pieces)
    try:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(contents)
    except OSError as error:
        raise GenerationError(f"cannot write {output}: {error}") from error

    digest = hashlib.sha256(contents).hexdigest()
    print(f"wrote {len(pieces)} lines to {output}; sha256 {digest}")
    return 0


class SelfTests(unittest.TestCase):
    def test_deterministic_output_for_fixture(self) -> None:
        fixture = "<blk> 0\n▁ƊĢĥ 1\n<unk> 2\n"
        pieces = load_pieces(io.StringIO(fixture), "tokens.txt")
        expected = "<blk>\t0.0\n▁ƊĢĥ\t-0.1\n<unk>\t-0.2\n".encode()
        self.assertEqual(render_vocab(pieces), expected)
        self.assertEqual(render_vocab(pieces), render_vocab(pieces))

    def test_rejects_malformed_line(self) -> None:
        with self.assertRaisesRegex(GenerationError, "tokens.txt:2.*space separator"):
            load_pieces(io.StringIO("<blk> 0\nmalformed\n"), "tokens.txt")

    def test_rejects_duplicate_piece(self) -> None:
        with self.assertRaisesRegex(GenerationError, "tokens.txt:3.*duplicate piece"):
            load_pieces(io.StringIO("<blk> 0\nfoo 1\n<blk> 2\n"), "tokens.txt")

    def test_score_sequence(self) -> None:
        self.assertEqual(
            render_vocab(["a", "b", "c", "d"]).decode().splitlines(),
            ["a\t0.0", "b\t-0.1", "c\t-0.2", "d\t-0.3"],
        )

    def test_sha256_stability(self) -> None:
        contents = render_vocab(["<blk>", "▁ƊĢĥ", "<unk>"])
        self.assertEqual(
            hashlib.sha256(contents).hexdigest(),
            "d42f8d7d674fede0ba0824fb64e154446e96a4c2d4103e52e77a49225172de09",
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--tokens", type=Path)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if args.tokens is None:
        parser.error("--tokens is required unless --self-test is used")
    try:
        return generate(args.tokens, args.output)
    except GenerationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

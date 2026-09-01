#!/usr/bin/env python3
"""Generate a sherpa-onnx cjkchar hotwords file from approved readings."""

from __future__ import annotations

import argparse
import hashlib
import io
import re
import sys
import unittest
from pathlib import Path
from typing import TextIO


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_APPROVED = ROOT / "scripts" / "dict" / "approved.tsv"
DEFAULT_OUTPUT = ROOT / "scripts" / "hotword-eval" / "out" / "hotwords.txt"
READING_RE = re.compile(r"[ァ-ヶー・]+\Z")
MIN_HOTWORDS = 200


class GenerationError(RuntimeError):
    """A safe, expected generation failure with a user-facing message."""


def valid_reading(reading: str) -> bool:
    return len(reading) >= 4 and bool(READING_RE.fullmatch(reading))


def load_readings(rows: TextIO, source_name: str) -> list[str]:
    readings: list[str] = []
    seen: set[str] = set()
    for line_number, line in enumerate(rows, 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        fields = line.rstrip("\r\n").split("\t")
        if len(fields) != 5:
            raise GenerationError(
                f"{source_name}:{line_number}: expected reading, surface, "
                "source_id, provenance, approval_note"
            )
        reading, _surface, _source_id, provenance, approval_note = fields
        if (
            "misrecognition" in approval_note
            or "misrecognition" in provenance
            or not valid_reading(reading)
            or reading in seen
        ):
            continue
        seen.add(reading)
        readings.append(reading)
    return readings


def render_hotwords(readings: list[str]) -> bytes:
    return "".join(f"{reading}\n" for reading in readings).encode("utf-8")


def validate_tokens(readings: list[str], rows: TextIO, source_name: str) -> int:
    vocabulary = {
        fields[0] for line in rows if (fields := line.strip().split())
    }
    errors: list[str] = []
    for line_number, reading in enumerate(readings, 1):
        for character in dict.fromkeys(reading):
            if character not in vocabulary:
                errors.append(
                    f"hotwords line {line_number} ({reading!r}): "
                    f"character {character!r} is absent from {source_name}"
                )
    if errors:
        raise GenerationError("token preflight failed:\n" + "\n".join(errors))
    return len(readings)


def generate(output: Path, tokens: Path | None) -> int:
    try:
        with DEFAULT_APPROVED.open(encoding="utf-8") as rows:
            readings = load_readings(rows, str(DEFAULT_APPROVED))
    except OSError as error:
        raise GenerationError(f"cannot read {DEFAULT_APPROVED}: {error}") from error

    contents = render_hotwords(readings)
    output_count = len(contents.splitlines())
    if output_count != len(readings):
        raise GenerationError(
            f"rendered count {output_count} does not match retained distinct "
            f"reading count {len(readings)}"
        )
    if output_count < MIN_HOTWORDS:
        raise GenerationError(
            f"retained distinct reading count {output_count} is below {MIN_HOTWORDS}"
        )

    if tokens is not None:
        try:
            with tokens.open(encoding="utf-8") as rows:
                validated_count = validate_tokens(readings, rows, str(tokens))
        except OSError as error:
            raise GenerationError(f"cannot read {tokens}: {error}") from error
        if validated_count != output_count:
            raise GenerationError(
                f"validated count {validated_count} does not match output count "
                f"{output_count}"
            )

    try:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(contents)
    except OSError as error:
        raise GenerationError(f"cannot write {output}: {error}") from error

    print(f"wrote {output_count} hotwords to {output}")
    if tokens is not None:
        digest = hashlib.sha256(output.read_bytes()).hexdigest()
        print(
            f"validated {output_count} hotwords against {tokens}; "
            f"sha256 {digest}"
        )
    return 0


class SelfTests(unittest.TestCase):
    def test_deduplicates_readings_preserving_first_occurrence(self) -> None:
        rows = approved_fixture(
            ("アイフォーン", "ordinary"),
            ("ウェブパック", "ordinary"),
            ("アイフォーン", "duplicate"),
        )
        self.assertEqual(load_readings(io.StringIO(rows), "fixture.tsv"), [
            "アイフォーン",
            "ウェブパック",
        ])

    def test_applies_katakana_and_length_gate(self) -> None:
        rows = approved_fixture(
            ("アイオーエス", "valid"),
            ("デノ", "too short"),
            ("テストA", "not katakana"),
            ("サーバ・レス", "valid middle dot"),
        )
        self.assertEqual(load_readings(io.StringIO(rows), "fixture.tsv"), [
            "アイオーエス",
            "サーバ・レス",
        ])

    def test_excludes_misrecognition_note_rows(self) -> None:
        rows = approved_fixture(
            ("クーベネティス", "misrecognition correction"),
            ("クバネティス", "canonical reading"),
        )
        self.assertEqual(
            load_readings(io.StringIO(rows), "fixture.tsv"), ["クバネティス"]
        )

    def test_excludes_misrecognition_note_embedded_in_provenance(self) -> None:
        rows = (
            "# reading\tsurface\tsource_id\tprovenance\tapproval_note\n"
            "クマネティス\tKubernetes\tcurated-extra\t"
            "ASR misrecognition variant\thuman-authored curated entry\n"
            "クバネティス\tKubernetes\tcurated-extra\t"
            "canonical reading\thuman-authored curated entry\n"
        )
        self.assertEqual(
            load_readings(io.StringIO(rows), "fixture.tsv"), ["クバネティス"]
        )

    def test_output_is_deterministic(self) -> None:
        readings = ["アイオーエス", "ウェブパック"]
        self.assertEqual(render_hotwords(readings), render_hotwords(readings))
        self.assertEqual(
            render_hotwords(readings), "アイオーエス\nウェブパック\n".encode()
        )

    def test_tokens_preflight_accepts_all_in_vocab(self) -> None:
        readings = ["アイオーエス"]
        tokens = io.StringIO("<blank> 0\n\u30a2 1\n\u30a4 2\n\u30aa 3\n\u30fc 4\n\u30a8 5\n\u30b9 6\n")
        self.assertEqual(validate_tokens(readings, tokens, "tokens.txt"), 1)

    def test_tokens_preflight_rejects_out_of_vocabulary_character(self) -> None:
        readings = ["アイオーエス"]
        tokens = io.StringIO("\u30a2 1\n\u30a4 2\n\u30aa 3\n\u30fc 4\n\u30a8 5\n")
        with self.assertRaisesRegex(
            GenerationError, "hotwords line 1.*character '\u30b9'"
        ):
            validate_tokens(readings, tokens, "tokens.txt")


def approved_fixture(*rows: tuple[str, str]) -> str:
    contents = "# reading\tsurface\tsource_id\tprovenance\tapproval_note\n"
    for index, (reading, note) in enumerate(rows, 1):
        contents += f"{reading}\tTerm {index}\tfixture\tfixture:{index}\t{note}\n"
    return contents


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test", action="store_true", help="run the embedded offline fixture suite"
    )
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--tokens", type=Path, help="validate every cjkchar against tokens.txt"
    )
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    try:
        return generate(args.output, args.tokens)
    except GenerationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

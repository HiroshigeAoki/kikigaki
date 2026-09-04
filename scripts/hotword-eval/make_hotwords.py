#!/usr/bin/env python3
"""Generate a sherpa-onnx cjkchar hotwords file from approved readings."""

from __future__ import annotations

import argparse
import hashlib
import io
import re
import sys
import tempfile
import unittest
from collections.abc import Iterator
from pathlib import Path
from typing import TextIO


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_APPROVED = ROOT / "scripts" / "dict" / "approved.tsv"
DEFAULT_EXCLUDE = ROOT / "scripts" / "dict" / "hotword-exclude.txt"
DEFAULT_OUTPUT = ROOT / "scripts" / "hotword-eval" / "out" / "hotwords.txt"
DEFAULT_RENDER_OUTPUT = ROOT / "crates" / "kikigaki-engine" / "data" / "hotwords.txt"
READING_RE = re.compile(r"[ァ-ヶー・]+\Z")
MIN_HOTWORDS = 200


class GenerationError(RuntimeError):
    """A safe, expected generation failure with a user-facing message."""


def valid_reading(reading: str) -> bool:
    return len(reading) >= 4 and bool(READING_RE.fullmatch(reading))


def iter_approved_rows(
    rows: TextIO, source_name: str
) -> Iterator[tuple[int, list[str]]]:
    """Yields each non-comment, non-blank `approved.tsv` row as `(line_number, fields)`.

    Raises if a row does not split into exactly the five expected columns.
    """
    for line_number, line in enumerate(rows, 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        fields = line.rstrip("\r\n").split("\t")
        if len(fields) != 5:
            raise GenerationError(
                f"{source_name}:{line_number}: expected reading, surface, "
                "source_id, provenance, approval_note"
            )
        yield line_number, fields


def load_readings(rows: TextIO, source_name: str) -> list[str]:
    readings: list[str] = []
    seen: set[str] = set()
    for _line_number, fields in iter_approved_rows(rows, source_name):
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


def load_approved_reading_names(rows: TextIO, source_name: str) -> set[str]:
    return {fields[0] for _line_number, fields in iter_approved_rows(rows, source_name)}


def load_exclusions(
    rows: TextIO, source_name: str, approved_readings: set[str]
) -> set[str]:
    excluded: set[str] = set()
    for line_number, line in enumerate(rows, 1):
        if "\r" in line:
            raise GenerationError(
                f"{source_name}:{line_number}: CRLF line endings are not allowed"
            )
        line = line.removesuffix("\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        reading, _separator, _comment = line.partition("\t")
        if not READING_RE.fullmatch(reading):
            raise GenerationError(
                f"{source_name}:{line_number}: non-katakana reading {reading!r}"
            )
        if reading in excluded:
            raise GenerationError(
                f"{source_name}:{line_number}: duplicate reading {reading!r}"
            )
        if reading not in approved_readings:
            raise GenerationError(
                f"{source_name}:{line_number}: unknown reading {reading!r}"
            )
        excluded.add(reading)
    return excluded


def apply_exclusions(readings: list[str], excluded: set[str]) -> list[str]:
    return [reading for reading in readings if reading not in excluded]


def render_hotwords(readings: list[str]) -> bytes:
    return "".join(f"{reading}\n" for reading in readings).encode("utf-8")


def checked_render(readings: list[str]) -> tuple[bytes, int]:
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
    return contents, output_count


def write_artifact(output: Path, contents: bytes) -> None:
    try:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(contents)
    except OSError as error:
        raise GenerationError(f"cannot write {output}: {error}") from error


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

    contents, output_count = checked_render(readings)

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

    write_artifact(output, contents)

    print(f"wrote {output_count} hotwords to {output}")
    if tokens is not None:
        digest = hashlib.sha256(output.read_bytes()).hexdigest()
        print(
            f"validated {output_count} hotwords against {tokens}; "
            f"sha256 {digest}"
        )
    return 0


def render_approved(args: argparse.Namespace) -> int:
    try:
        approved_contents = args.approved.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise GenerationError(f"cannot read {args.approved}: {error}") from error

    readings = load_readings(io.StringIO(approved_contents), str(args.approved))
    approved_readings = load_approved_reading_names(
        io.StringIO(approved_contents), str(args.approved)
    )
    try:
        with args.exclude.open(encoding="utf-8", newline="") as rows:
            excluded = load_exclusions(rows, str(args.exclude), approved_readings)
    except (OSError, UnicodeError) as error:
        raise GenerationError(f"cannot read {args.exclude}: {error}") from error

    contents, output_count = checked_render(apply_exclusions(readings, excluded))
    if args.check:
        try:
            existing = args.output.read_bytes()
        except OSError as error:
            raise GenerationError(
                f"cannot read rendered artifact {args.output}: {error}"
            ) from error
        if existing != contents:
            raise GenerationError(
                f"rendered artifact drift: run {Path(__file__).name} render --write"
            )
        print(
            f"rendered artifact is up to date: {args.output} "
            f"({output_count} hotwords)"
        )
        return 0
    if not args.write:
        raise GenerationError("refusing to overwrite without render --write")
    write_artifact(args.output, contents)
    print(f"wrote {output_count} hotwords to {args.output}")
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

    def test_exclusion_list_filters_readings_and_accepts_comments(self) -> None:
        approved = {"アイフォーン", "ウェブパック"}
        rows = io.StringIO(
            "# Hotwords that must not be boosted.\n"
            "\n"
            "アイフォーン\tregression observed in field testing\n"
        )
        excluded = load_exclusions(rows, "hotword-exclude.txt", approved)
        self.assertEqual(excluded, {"アイフォーン"})
        self.assertEqual(
            apply_exclusions(["アイフォーン", "ウェブパック"], excluded),
            ["ウェブパック"],
        )

    def test_exclusion_list_rejects_unknown_reading(self) -> None:
        with self.assertRaisesRegex(GenerationError, "unknown reading"):
            load_exclusions(
                io.StringIO("ウェブパック\n"),
                "hotword-exclude.txt",
                {"アイフォーン"},
            )

    def test_exclusion_list_rejects_duplicate_reading(self) -> None:
        with self.assertRaisesRegex(GenerationError, "duplicate reading"):
            load_exclusions(
                io.StringIO("アイフォーン\nアイフォーン\tsecond entry\n"),
                "hotword-exclude.txt",
                {"アイフォーン"},
            )

    def test_exclusion_list_rejects_non_katakana_entry(self) -> None:
        with self.assertRaisesRegex(GenerationError, "non-katakana"):
            load_exclusions(
                io.StringIO("iPhone\n"),
                "hotword-exclude.txt",
                {"iPhone"},
            )

    def test_exclusion_list_rejects_crlf(self) -> None:
        with self.assertRaisesRegex(GenerationError, "CRLF"):
            load_exclusions(
                io.StringIO("アイフォーン\r\n"),
                "hotword-exclude.txt",
                {"アイフォーン"},
            )

    def test_render_requires_exclusion_file_but_eval_mode_does_not(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            missing = root / "missing-hotword-exclude.txt"
            output = root / "hotwords.txt"
            for write, check in ((True, False), (False, True)):
                with self.subTest(write=write, check=check):
                    args = argparse.Namespace(
                        approved=DEFAULT_APPROVED,
                        exclude=missing,
                        output=output,
                        write=write,
                        check=check,
                    )
                    with self.assertRaisesRegex(
                        GenerationError, "cannot read.*missing-hotword-exclude"
                    ):
                        render_approved(args)

            self.assertEqual(generate(output, None), 0)
            self.assertTrue(output.exists())

    def test_render_default_output_is_fixed_repository_path(self) -> None:
        args = build_parser().parse_args(["render", "--write"])
        self.assertEqual(args.output, DEFAULT_RENDER_OUTPUT)

    def test_render_write_and_check_modes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "hotwords.txt"
            exclude = root / "hotword-exclude.txt"
            exclude.write_text("# Empty fixture exclusion list.\n", encoding="utf-8")
            args = argparse.Namespace(
                approved=DEFAULT_APPROVED,
                exclude=exclude,
                output=output,
                write=True,
                check=False,
            )
            render_approved(args)
            generated = output.read_bytes()

            args.write = False
            args.check = True
            self.assertEqual(render_approved(args), 0)
            output.write_bytes(b"drift\n")
            with self.assertRaisesRegex(GenerationError, "artifact drift"):
                render_approved(args)
            self.assertEqual(output.read_bytes(), b"drift\n")
            self.assertNotEqual(generated, b"drift\n")

    def test_real_approved_semantic_oracle(self) -> None:
        with DEFAULT_APPROVED.open(encoding="utf-8") as rows:
            readings = load_readings(rows, str(DEFAULT_APPROVED))
        rendered = render_hotwords(readings).decode("utf-8").splitlines()
        self.assertIn("ウィンドウズ", rendered)
        self.assertIn("ドッカー", rendered)
        self.assertNotIn("クマネティス", rendered)

        excluded = load_exclusions(
            io.StringIO("ウィンドウズ\ttest fixture\n"),
            "fixture-exclude.txt",
            set(readings),
        )
        fixture_rendered = render_hotwords(
            apply_exclusions(readings, excluded)
        ).decode("utf-8")
        self.assertNotIn("ウィンドウズ", fixture_rendered.splitlines())


def approved_fixture(*rows: tuple[str, str]) -> str:
    contents = "# reading\tsurface\tsource_id\tprovenance\tapproval_note\n"
    for index, (reading, note) in enumerate(rows, 1):
        contents += f"{reading}\tTerm {index}\tfixture\tfixture:{index}\t{note}\n"
    return contents


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test", action="store_true", help="run the embedded offline fixture suite"
    )
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--tokens", type=Path, help="validate every cjkchar against tokens.txt"
    )
    subcommands = parser.add_subparsers(dest="command")
    render = subcommands.add_parser(
        "render", help="render approved.tsv as the embedded hotword artifact"
    )
    render.add_argument("--approved", type=Path, default=DEFAULT_APPROVED)
    render.add_argument("--exclude", type=Path, default=DEFAULT_EXCLUDE)
    render.add_argument("--output", type=Path, default=DEFAULT_RENDER_OUTPUT)
    output_mode = render.add_mutually_exclusive_group()
    output_mode.add_argument("--write", action="store_true")
    output_mode.add_argument("--check", action="store_true")
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    if args.self_test:
        if args.command is not None:
            parser.error("--self-test cannot be combined with a subcommand")
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    try:
        if args.command == "render":
            return render_approved(args)
        return generate(args.output, args.tokens)
    except GenerationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

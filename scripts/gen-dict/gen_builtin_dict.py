#!/usr/bin/env python3
"""Generate the reviewed builtin katakana-to-English dictionary."""

from __future__ import annotations

import argparse
import contextlib
import csv
import dataclasses
import hashlib
import io
import itertools
import json
import lzma
import os
import re
import sys
import tempfile
import unittest
import unicodedata
from collections import defaultdict
from pathlib import Path
from typing import Iterable, Iterator, TextIO

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - the project requires Python 3.11+
    tomllib = None  # type: ignore[assignment]


ROOT = Path(__file__).resolve().parents[2]
DICT_DIR = ROOT / "scripts" / "dict"
CACHE_DIR = DICT_DIR / "cache"
DEFAULT_LOCK = DICT_DIR / "sources.lock.json"
DEFAULT_APPROVED = DICT_DIR / "approved.tsv"
DEFAULT_OUTPUT = ROOT / "crates" / "kikigaki-core" / "data" / "builtin-replace.toml"
DEFAULT_CANDIDATES = DICT_DIR / "candidates.tsv"

SURFACE_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9 +#.\-]*\Z")
READING_RE = re.compile(r"[ァ-ヶー・]+\Z")
MIN_MAPPINGS = 200
MAX_MAPPINGS = 2000


class GenerationError(RuntimeError):
    """A safe, expected pipeline failure with a user-facing message."""


@dataclasses.dataclass(frozen=True)
class Candidate:
    surface: str
    reading: str
    source_id: str
    provenance: str


def normalize(value: str) -> str:
    return unicodedata.normalize("NFKC", value).strip()


def valid_surface(value: str) -> bool:
    return bool(SURFACE_RE.fullmatch(value) and re.search(r"[A-Za-z]", value))


def valid_reading(value: str) -> bool:
    return len(value) >= 4 and bool(READING_RE.fullmatch(value))


def parse_neologd(rows: Iterable[str], source_name: str) -> Iterator[Candidate]:
    """Parse the 13-column MeCab seed format; reading is column 12."""
    for line_number, row in enumerate(csv.reader(rows), 1):
        if len(row) < 13:
            continue
        surface = row[0]
        reading = row[11]
        if not surface or not reading:
            continue
        yield Candidate(surface, reading, "neologd", f"{source_name}:{line_number}")


def parse_wikidata(rows: TextIO, source_name: str) -> Iterator[Candidate]:
    reader = csv.reader(rows, dialect="excel-tab")
    try:
        header = next(reader)
    except StopIteration:
        return
    columns = {name.lstrip("?"): index for index, name in enumerate(header)}
    required = {"item", "surface", "reading"}
    if not required.issubset(columns):
        raise GenerationError(
            f"{source_name}: expected TSV columns item, surface, reading"
        )
    for row in reader:
        if len(row) <= max(columns.values()):
            continue
        item = row[columns["item"]].strip()
        surface = row[columns["surface"]]
        reading = row[columns["reading"]]
        if surface and reading:
            yield Candidate(surface, reading, "wikidata", item or source_name)


def _markdown_cells(line: str) -> list[str] | None:
    stripped = line.strip()
    if "|" not in stripped or "\\|" in stripped:
        return None
    return [cell.strip() for cell in stripped.strip("|").split("|")]


def _is_markdown_separator(cells: list[str]) -> bool:
    return bool(cells) and all(re.fullmatch(r":?-{3,}:?", cell) for cell in cells)


def _header_index(cells: list[str], names: tuple[str, ...]) -> int | None:
    matches = []
    for index, cell in enumerate(cells):
        normalized = normalize(cell).casefold()
        if any(name in normalized for name in names):
            matches.append(index)
    return matches[0] if len(matches) == 1 else None


def _surface_aliases(cell: str) -> list[str] | None:
    """Split only the glossary's explicit `Long name, ABC` alias shape."""
    value = normalize(cell).strip("`*_ ")
    lowered = value.casefold()
    compound_markers = (" or ", " and ", " aka ", "/", ";", ":", "(", ")", "[", "]", "<", ">")
    if any(marker in lowered for marker in compound_markers):
        return None
    if "," in value and not re.fullmatch(r"[^,]+(?:, [^,]+)*", value):
        return None
    aliases = value.split(", ")
    if not aliases or any(not valid_surface(normalize(alias)) for alias in aliases):
        return None
    return aliases


def parse_lingo(markdown: str, source_name: str) -> Iterator[Candidate]:
    lines = markdown.splitlines()
    index = 0
    while index + 1 < len(lines):
        headers = _markdown_cells(lines[index])
        separator = _markdown_cells(lines[index + 1])
        if headers is None or separator is None or not _is_markdown_separator(separator):
            index += 1
            continue
        reading_index = _header_index(
            headers, ("japanese", "日本語", "katakana", "reading", "読み")
        )
        surface_index = _header_index(headers, ("english", "英語"))
        index += 2
        while index < len(lines):
            cells = _markdown_cells(lines[index])
            if cells is None:
                break
            line_number = index + 1
            index += 1
            if (
                reading_index is None
                or surface_index is None
                or reading_index == surface_index
                or max(reading_index, surface_index) >= len(cells)
            ):
                continue
            reading = normalize(cells[reading_index]).strip("`*_ ")
            aliases = _surface_aliases(cells[surface_index])
            if aliases is None or not READING_RE.fullmatch(reading):
                continue
            for surface in aliases:
                yield Candidate(
                    surface,
                    reading,
                    "japanese-dev-lingo",
                    f"{source_name}:{line_number}",
                )


def parse_curated(rows: Iterable[str], source_name: str) -> Iterator[Candidate]:
    for line_number, line in enumerate(rows, 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        fields = line.rstrip("\r\n").split("\t")
        if len(fields) != 3:
            raise GenerationError(
                f"{source_name}:{line_number}: expected surface, reading, note"
            )
        surface, reading, note = fields
        yield Candidate(
            surface,
            reading,
            "curated-extra",
            f"{source_name}:{line_number}: {note}",
        )


def load_denylist(rows: Iterable[str], source_name: str) -> set[str]:
    denied: set[str] = set()
    for line_number, line in enumerate(rows, 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        fields = line.rstrip("\r\n").split("\t", 1)
        if len(fields) != 2 or not fields[1].lstrip().startswith("#"):
            raise GenerationError(
                f"{source_name}:{line_number}: expected reading<TAB># reason"
            )
        reading = normalize(fields[0])
        if not READING_RE.fullmatch(reading):
            raise GenerationError(f"{source_name}:{line_number}: invalid reading")
        denied.add(reading)
    return denied


def filter_candidates(rows: Iterable[Candidate], denied: set[str]) -> list[Candidate]:
    normalized_denied = {normalize(reading) for reading in denied}
    accepted: list[Candidate] = []
    seen: set[Candidate] = set()
    for row in rows:
        candidate = Candidate(
            normalize(row.surface),
            normalize(row.reading),
            row.source_id,
            row.provenance,
        )
        if (
            not valid_surface(candidate.surface)
            or not valid_reading(candidate.reading)
            or candidate.reading in normalized_denied
            or candidate in seen
        ):
            continue
        seen.add(candidate)
        accepted.append(candidate)

    owners: dict[str, set[str]] = defaultdict(set)
    for row in accepted:
        owners[row.reading].add(row.surface.casefold())
    return [row for row in accepted if len(owners[row.reading]) == 1]


def sha256_bytes(contents: bytes) -> str:
    return hashlib.sha256(contents).hexdigest()


def verify_sha256(path: Path, expected: str) -> None:
    actual = sha256_bytes(path.read_bytes())
    if actual != expected.lower():
        raise GenerationError(
            f"sha256 mismatch for {path}: expected {expected.lower()}, got {actual}"
        )


def decompress_xz(contents: bytes, source_name: str) -> bytes:
    try:
        return lzma.decompress(contents)
    except (EOFError, lzma.LZMAError) as error:
        raise GenerationError(f"invalid or truncated xz data in {source_name}") from error


def _locked_sources(lock_path: Path) -> dict[str, dict[str, object]]:
    try:
        document = json.loads(lock_path.read_text(encoding="utf-8"))
        sources = document["sources"]
        records = {record["id"]: record for record in sources}
    except (OSError, KeyError, TypeError, json.JSONDecodeError) as error:
        raise GenerationError(f"cannot read source lock manifest {lock_path}: {error}") from error
    if not isinstance(sources, list) or len(records) != len(sources):
        raise GenerationError(f"invalid or duplicate source records in {lock_path}")
    return records


def _verify_locked_source(
    records: dict[str, dict[str, object]], source_id: str, path: Path
) -> None:
    try:
        expected = records[source_id]["sha256"]
    except KeyError as error:
        raise GenerationError(f"source {source_id!r} is missing from the lock manifest") from error
    if not isinstance(expected, str):
        raise GenerationError(f"source {source_id!r} has no valid sha256 in the lock manifest")
    try:
        verify_sha256(path, expected)
    except OSError as error:
        raise GenerationError(f"cannot read cached source {path}: {error}") from error


def _atomic_write(path: Path, contents: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_name: str | None = None
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
            temporary_name = temporary.name
            temporary.write(contents)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_name, path)
        temporary_name = None
    finally:
        if temporary_name is not None:
            Path(temporary_name).unlink(missing_ok=True)


def extract_candidates(args: argparse.Namespace) -> int:
    records = _locked_sources(args.lock)
    neologd_path = args.cache_dir / "neologd-seed.csv.xz"
    wikidata_path = args.cache_dir / "wikidata.tsv"
    lingo_path = args.cache_dir / "japanese-dev-lingo-ReadMe.md"
    for source_id, path in (
        ("neologd-seed", neologd_path),
        ("wikidata", wikidata_path),
        ("japanese-dev-lingo-readme", lingo_path),
    ):
        _verify_locked_source(records, source_id, path)

    try:
        with args.deny.open(encoding="utf-8") as rows:
            denied = load_denylist(rows, str(args.deny))
        with lzma.open(neologd_path, mode="rt", encoding="utf-8", newline="") as neologd:
            with wikidata_path.open(encoding="utf-8", newline="") as wikidata:
                with args.curated.open(encoding="utf-8") as curated:
                    raw_rows = itertools.chain(
                        parse_neologd(neologd, neologd_path.name),
                        parse_wikidata(wikidata, wikidata_path.name),
                        parse_lingo(
                            lingo_path.read_text(encoding="utf-8"), lingo_path.name
                        ),
                        parse_curated(curated, args.curated.name),
                    )
                    candidates = filter_candidates(raw_rows, denied)
    except (OSError, EOFError, lzma.LZMAError, UnicodeError) as error:
        raise GenerationError(f"failed to extract cached sources: {error}") from error

    candidates.sort(
        key=lambda row: (row.surface, row.reading, row.source_id, row.provenance)
    )
    lines = ["# surface\treading\tsource_id\tprovenance\n"]
    lines.extend(
        f"{row.surface}\t{row.reading}\t{row.source_id}\t{row.provenance}\n"
        for row in candidates
    )
    _atomic_write(args.output, "".join(lines).encode("utf-8"))
    print(f"wrote {len(candidates)} candidates to {args.output}")
    return 0


def parse_approved(contents: bytes) -> dict[str, set[str]]:
    try:
        text = contents.decode("utf-8")
    except UnicodeDecodeError as error:
        raise GenerationError("approved.tsv is not valid UTF-8") from error
    groups: dict[str, set[str]] = defaultdict(set)
    reading_owners: dict[str, str] = {}
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) != 5:
            raise GenerationError(
                f"approved.tsv:{line_number}: expected reading, surface, source_id, provenance, approval_note"
            )
        reading, surface, source_id, provenance, approval_note = fields
        if not all((reading, surface, source_id, provenance, approval_note)):
            raise GenerationError(f"approved.tsv:{line_number}: columns must not be empty")
        previous = reading_owners.setdefault(reading, surface)
        if previous != surface:
            raise GenerationError(
                f"approved.tsv:{line_number}: reading {reading!r} is approved for both {previous!r} and {surface!r}"
            )
        groups[surface].add(reading)
    return groups


def toml_string(value: str) -> str:
    escapes = {
        "\b": "\\b",
        "\t": "\\t",
        "\n": "\\n",
        "\f": "\\f",
        "\r": "\\r",
        '"': '\\"',
        "\\": "\\\\",
    }
    encoded = []
    for character in value:
        if character in escapes:
            encoded.append(escapes[character])
        elif ord(character) < 0x20 or ord(character) == 0x7F:
            encoded.append(f"\\u{ord(character):04X}")
        else:
            encoded.append(character)
    return f'"{"".join(encoded)}"'


def parse_toml(contents: str) -> dict[str, object]:
    if tomllib is None:
        raise GenerationError("Python 3.11 or newer is required for TOML validation")
    return tomllib.loads(contents)


def mapping_count(groups: dict[str, set[str]]) -> int:
    return sum(len(readings) for readings in groups.values())


def render_bytes(approved: bytes, lock_manifest: bytes) -> bytes:
    groups = parse_approved(approved)
    count = mapping_count(groups)
    if count == 0:
        raise GenerationError(
            "approved.tsv contains no approved mappings; refusing to replace the bootstrap artifact"
        )
    if not MIN_MAPPINGS <= count <= MAX_MAPPINGS:
        raise GenerationError(
            f"rendered mapping count {count} is outside {MIN_MAPPINGS}..={MAX_MAPPINGS}"
        )

    lines = [
        "# Generated builtin katakana→English dictionary — DO NOT EDIT BY HAND.\n",
        "# Generated by scripts/gen-dict/gen_builtin_dict.py from approved.tsv only.\n",
        f"# lock-manifest-sha256: {sha256_bytes(lock_manifest)}\n",
        f"# approved.tsv-sha256: {sha256_bytes(approved)}\n",
        "\n",
    ]
    expected_rules = []
    for surface in sorted(groups):
        readings = sorted(groups[surface])
        expected_rules.append({"from": readings, "to": surface})
        encoded_readings = ", ".join(toml_string(reading) for reading in readings)
        lines.extend(
            (
                "[[rule]]\n",
                f"from = [{encoded_readings}]\n",
                f"to = {toml_string(surface)}\n",
                "\n",
            )
        )
    if lines and lines[-1] == "\n":
        lines.pop()  # no trailing blank line at EOF (git diff --check)
    rendered = "".join(lines).encode("utf-8")
    parsed = parse_toml(rendered.decode("utf-8"))
    if parsed != {"rule": expected_rules}:
        raise GenerationError("internal TOML serialization round-trip failed")
    return rendered


def render_approved(args: argparse.Namespace) -> int:
    try:
        approved = args.approved.read_bytes() if args.approved.exists() else b""
    except OSError as error:
        raise GenerationError(f"cannot read {args.approved}: {error}") from error
    # Parse approvals before the lock so bootstrap repositories get the intended error.
    groups = parse_approved(approved)
    if not groups:
        raise GenerationError(
            "approved.tsv contains no approved mappings; refusing to replace the bootstrap artifact"
        )
    try:
        lock_manifest = args.lock.read_bytes()
    except OSError as error:
        raise GenerationError(f"cannot read {args.lock}: {error}") from error
    rendered = render_bytes(approved, lock_manifest)

    if args.check:
        try:
            existing = args.output.read_bytes()
        except OSError as error:
            raise GenerationError(f"cannot read rendered artifact {args.output}: {error}") from error
        with tempfile.NamedTemporaryFile() as temporary:
            temporary.write(rendered)
            temporary.flush()
            temporary.seek(0)
            regenerated = temporary.read()
        if regenerated != existing:
            raise GenerationError(
                f"rendered artifact drift: run {Path(__file__).name} render --write"
            )
        print(f"rendered artifact is up to date: {args.output}")
        return 0
    if not args.write:
        raise GenerationError("refusing to overwrite without render --write")
    _atomic_write(args.output, rendered)
    print(f"wrote {mapping_count(groups)} approved mappings to {args.output}")
    return 0


def _katakana_counter(index: int) -> str:
    alphabet = "アイウエオカキクケコサシスセソタチツテトナニヌネノ"
    base = len(alphabet)
    return "テ" + "".join(
        alphabet[(index // (base**power)) % base] for power in (2, 1, 0)
    )


def approved_fixture(
    count: int, *, duplicate_first: bool = False, quoted_surface: bool = False
) -> bytes:
    surface = 'Surface "quoted" C:\\tools' if quoted_surface else "Surface"
    lines = ["# reading\tsurface\tsource_id\tprovenance\tapproval_note\n"]
    for index in range(count):
        lines.append(
            f"{_katakana_counter(index)}\t{surface}\tfixture\trow-{index}\treviewed\n"
        )
    if duplicate_first and count:
        lines.append(f"{_katakana_counter(0)}\t{surface}\tfixture\tduplicate\treviewed\n")
    return "".join(lines).encode("utf-8")


class SelfTests(unittest.TestCase):
    def test_neologd_parser_and_shared_filters(self) -> None:
        fixture = io.StringIO(
            "Terraform,1285,1285,1000,名詞,固有名詞,一般,*,*,*,Terraform,テラフォーム,テラフォーム\n"
            "東京,1285,1285,1000,名詞,固有名詞,地域,*,*,*,東京,トウキョウ,トーキョー\n"
            "malformed,row\n"
        )
        raw = list(parse_neologd(fixture, "fixture.csv"))
        self.assertEqual(len(raw), 2)
        self.assertEqual(
            filter_candidates(raw, set()),
            [Candidate("Terraform", "テラフォーム", "neologd", "fixture.csv:1")],
        )

    def test_wikidata_parser(self) -> None:
        fixture = io.StringIO(
            "item\tsurface\treading\n"
            "http://www.wikidata.org/entity/Q1\tPostgreSQL\tポストグレス\n"
            "http://www.wikidata.org/entity/Q2\tDocker\tドッカー\n"
        )
        self.assertEqual(
            list(parse_wikidata(fixture, "fixture.tsv")),
            [
                Candidate(
                    "PostgreSQL",
                    "ポストグレス",
                    "wikidata",
                    "http://www.wikidata.org/entity/Q1",
                ),
                Candidate(
                    "Docker",
                    "ドッカー",
                    "wikidata",
                    "http://www.wikidata.org/entity/Q2",
                ),
            ],
        )

    def test_lingo_aliases_are_explicit_and_compounds_are_rejected(self) -> None:
        fixture = """\
| Japanese | English |
| --- | --- |
| パーソナルコンピューター | Personal computer, PC |
| アプリケーション | Application or app |
| クライアント | Client/server |
"""
        parsed = list(parse_lingo(fixture, "ReadMe.md"))
        self.assertEqual(
            [(row.surface, row.reading) for row in parsed],
            [
                ("Personal computer", "パーソナルコンピューター"),
                ("PC", "パーソナルコンピューター"),
            ],
        )

    def test_curated_rows_use_shared_normalization_and_denylist(self) -> None:
        fixture = io.StringIO(
            "Ｔｅｒｒａｆｏｒｍ\tﾃﾗﾌｫｰﾑ\tfull width fixture\n"
            "Rust\tラスト\tunsafe collision\n"
            "Deno\tデノ\ttoo short\n"
        )
        rows = list(parse_curated(fixture, "curated-extra.tsv"))
        self.assertEqual(
            filter_candidates(rows, {"ラスト"}),
            [
                Candidate(
                    "Terraform",
                    "テラフォーム",
                    "curated-extra",
                    "curated-extra.tsv:1: full width fixture",
                )
            ],
        )

    def test_ambiguity_drops_all_claimants_but_not_substrings(self) -> None:
        rows = [
            Candidate("Alpha", "アルファー", "one", "1"),
            Candidate("Beta", "アルファー", "two", "2"),
            Candidate("Gamma", "アルファーベット", "three", "3"),
        ]
        self.assertEqual(
            filter_candidates(rows, set()),
            [Candidate("Gamma", "アルファーベット", "three", "3")],
        )

    def test_casefold_is_only_used_for_surface_identity(self) -> None:
        rows = [
            Candidate("GitHub", "ギットハブ", "one", "1"),
            Candidate("github", "ギットハブ", "two", "2"),
        ]
        self.assertEqual(filter_candidates(rows, set()), rows)

    def test_toml_escaping_round_trips_quotes_and_backslashes(self) -> None:
        value = 'A "quoted" C:\\tools'
        encoded = toml_string(value)
        self.assertEqual(parse_toml(f"value = {encoded}\n")["value"], value)

    def test_render_is_deterministic_and_deduplicates_from_values(self) -> None:
        approved = approved_fixture(200, duplicate_first=True, quoted_surface=True)
        first = render_bytes(approved, b'{"sources":[]}\n')
        second = render_bytes(approved, b'{"sources":[]}\n')
        self.assertEqual(first, second)
        parsed = parse_toml(first.decode("utf-8"))
        self.assertEqual(sum(len(rule["from"]) for rule in parsed["rule"]), 200)
        self.assertIn('"', parsed["rule"][0]["to"])
        self.assertIn("\\", parsed["rule"][0]["to"])

    def test_hash_mismatch_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory, "fixture")
            path.write_bytes(b"actual")
            with self.assertRaisesRegex(GenerationError, "sha256 mismatch"):
                verify_sha256(path, hashlib.sha256(b"other").hexdigest())

    def test_truncated_xz_fails(self) -> None:
        compressed = lzma.compress(b"fixture\n")
        with self.assertRaisesRegex(GenerationError, "invalid or truncated xz"):
            decompress_xz(compressed[:-4], "fixture.xz")

    def test_empty_approved_refuses_to_render(self) -> None:
        with self.assertRaisesRegex(GenerationError, "no approved mappings"):
            render_bytes(b"# reading\tsurface\tsource_id\tprovenance\tapproval_note\n", b"{}\n")

    def test_count_gate_is_inclusive(self) -> None:
        render_bytes(approved_fixture(200), b"{}\n")
        render_bytes(approved_fixture(2000), b"{}\n")
        for count in (199, 2001):
            with self.subTest(count=count):
                with self.assertRaisesRegex(GenerationError, "outside 200..=2000"):
                    render_bytes(approved_fixture(count), b"{}\n")

    def test_render_modes_preserve_or_atomically_replace_the_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            approved = root / "approved.tsv"
            lock = root / "sources.lock.json"
            output = root / "builtin-replace.toml"
            approved.write_bytes(approved_fixture(200))
            lock.write_bytes(b'{"sources":[]}\n')
            output.write_bytes(b"old artifact\n")

            args = argparse.Namespace(
                approved=approved,
                lock=lock,
                output=output,
                write=False,
                check=False,
            )
            with self.assertRaisesRegex(GenerationError, "without render --write"):
                render_approved(args)
            self.assertEqual(output.read_bytes(), b"old artifact\n")

            args.write = True
            with contextlib.redirect_stdout(io.StringIO()):
                render_approved(args)
            generated = output.read_bytes()
            self.assertNotEqual(generated, b"old artifact\n")

            args.write = False
            args.check = True
            with contextlib.redirect_stdout(io.StringIO()):
                render_approved(args)
            output.write_bytes(b"drift\n")
            with self.assertRaisesRegex(GenerationError, "artifact drift"):
                render_approved(args)
            self.assertEqual(output.read_bytes(), b"drift\n")

    def test_checked_in_curation_and_denylist_are_valid(self) -> None:
        with (DICT_DIR / "deny-readings.txt").open(encoding="utf-8") as rows:
            denied = load_denylist(rows, "deny-readings.txt")
        with (DICT_DIR / "curated-extra.tsv").open(encoding="utf-8") as rows:
            curated = filter_candidates(
                parse_curated(rows, "curated-extra.tsv"), denied
            )
        mappings = {(row.surface, row.reading) for row in curated}
        self.assertIn(("Terraform", "テラフォーム"), mappings)
        self.assertNotIn(("Rust", "ラスト"), mappings)
        self.assertNotIn("サーバー", denied)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Extract candidates and render the reviewed builtin dictionary"
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the embedded offline fixture suite"
    )
    subcommands = parser.add_subparsers(dest="command")

    extract = subcommands.add_parser("extract", help="extract offline source snapshots")
    extract.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    extract.add_argument("--cache-dir", type=Path, default=CACHE_DIR)
    extract.add_argument(
        "--curated", type=Path, default=DICT_DIR / "curated-extra.tsv"
    )
    extract.add_argument("--deny", type=Path, default=DICT_DIR / "deny-readings.txt")
    extract.add_argument("--output", type=Path, default=DEFAULT_CANDIDATES)

    render = subcommands.add_parser("render", help="render approved.tsv as TOML")
    render.add_argument("--approved", type=Path, default=DEFAULT_APPROVED)
    render.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    render.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    output_mode = render.add_mutually_exclusive_group()
    output_mode.add_argument("--write", action="store_true")
    output_mode.add_argument("--check", action="store_true")

    args = parser.parse_args()
    if args.self_test:
        if args.command is not None:
            parser.error("--self-test cannot be combined with a subcommand")
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    try:
        if args.command == "extract":
            return extract_candidates(args)
        if args.command == "render":
            return render_approved(args)
        parser.error("a subcommand or --self-test is required")
    except GenerationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 2  # argparse exits before this point


if __name__ == "__main__":
    raise SystemExit(main())

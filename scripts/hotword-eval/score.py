#!/usr/bin/env python3
"""Validate and score hotword-evaluation JSONL results."""

from __future__ import annotations

import argparse
import json
import math
import re
import statistics
import sys
import tomllib
import unicodedata
import unittest
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence


ID_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]*\Z")
WILSON_Z_95 = 1.959963984540054
HEADER_KEYS = {
    "condition",
    "hotwords_file_sha256",
    "hotwords_line_count",
    "hotwords_score",
    "num_threads",
    "manifest_sha256",
    "git_head",
    "model_dir",
}
RESULT_KEYS = {"id", "ref", "hyp", "decode_ms"}


class ScoringError(RuntimeError):
    """A safe, expected scoring failure with a user-facing message."""


@dataclass(frozen=True)
class Rule:
    from_values: tuple[str, ...]
    to: str


Match = tuple[str, str, tuple[int, int]]


class DictionaryMapper:
    """Dictionary indexed in the same order as kikigaki-core replace.rs."""

    def __init__(self, rules: Sequence[Rule]) -> None:
        self.rules = tuple(rules)
        ordered: list[tuple[int, int]] = []
        for rule_index, rule in enumerate(self.rules):
            for from_index in range(len(rule.from_values)):
                ordered.append((rule_index, from_index))
        ordered.sort(
            key=lambda indices: (
                -len(self.rules[indices[0]].from_values[indices[1]]),
                indices[0],
                indices[1],
            )
        )
        self.ordered = tuple(ordered)

    @property
    def to_surfaces(self) -> set[str]:
        return {rule.to for rule in self.rules}

    def apply(self, text: str) -> tuple[str, list[Match]]:
        output: list[str] = []
        matches: list[Match] = []
        position = 0
        while position < len(text):
            selected: tuple[str, str] | None = None
            for rule_index, from_index in self.ordered:
                rule = self.rules[rule_index]
                from_value = rule.from_values[from_index]
                if text.startswith(from_value, position):
                    selected = (from_value, rule.to)
                    break
            if selected is None:
                output.append(text[position])
                position += 1
                continue
            from_value, to = selected
            end = position + len(from_value)
            output.append(to)
            matches.append((from_value, to, (position, end)))
            position = end
        return "".join(output), matches


@dataclass(frozen=True)
class ManifestRow:
    id: str
    spoken_text: str
    ref_text: str
    expected_surfaces: tuple[str, ...]
    source_name: str
    line_number: int
    target: bool

    def context(self) -> str:
        return f"{self.source_name}:{self.line_number} (id={self.id!r})"


@dataclass(frozen=True)
class RunMetadata:
    condition: str
    hotwords_file_sha256: str | None
    hotwords_line_count: int | None
    hotwords_score: float | None
    num_threads: int
    manifest_sha256: tuple[str, ...]
    git_head: str
    model_dir: str


@dataclass(frozen=True)
class ResultRecord:
    id: str
    ref: str
    hyp: str
    decode_ms: float


@dataclass(frozen=True)
class ResultFile:
    path: Path
    metadata: RunMetadata
    records: tuple[ResultRecord, ...]


@dataclass(frozen=True)
class MappedResult:
    text: str
    matches: tuple[Match, ...]


@dataclass(frozen=True)
class RecallScore:
    hits: int
    total: int
    all_hit: int
    utterances: int


@dataclass(frozen=True)
class RunScore:
    result_file: ResultFile
    recall: RecallScore
    hallucinations: int
    target_matches: int
    hallucination_details: tuple[tuple[str, Match], ...]
    false_trigger_ids: frozenset[str]
    false_trigger_surfaces: frozenset[tuple[str, str]]
    false_trigger_details: tuple[tuple[str, Match], ...]
    negative_count: int
    cer_edits: int
    cer_characters: int
    latency_p50_ms: float
    latency_p95_ms: float
    latency_ratios: tuple[float, ...]


def load_dictionary(path: Path) -> DictionaryMapper:
    try:
        with path.open("rb") as source:
            document = tomllib.load(source)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ScoringError(f"cannot load dictionary {path}: {error}") from error
    if set(document) - {"rule"}:
        extras = ", ".join(sorted(set(document) - {"rule"}))
        raise ScoringError(f"{path}: unknown top-level dictionary keys: {extras}")
    raw_rules = document.get("rule", [])
    if not isinstance(raw_rules, list):
        raise ScoringError(f"{path}: 'rule' must be an array of tables")

    rules: list[Rule] = []
    owners: dict[str, int] = {}
    for rule_index, raw_rule in enumerate(raw_rules):
        context = f"{path}: replacement rule {rule_index}"
        if not isinstance(raw_rule, dict) or set(raw_rule) != {"from", "to"}:
            raise ScoringError(f"{context}: expected exactly 'from' and 'to'")
        from_values = raw_rule["from"]
        to = raw_rule["to"]
        if not isinstance(from_values, list) or not from_values:
            raise ScoringError(f"{context} has no source strings")
        if not all(isinstance(value, str) for value in from_values):
            raise ScoringError(f"{context}: every 'from' value must be a string")
        if not isinstance(to, str):
            raise ScoringError(f"{context}: 'to' must be a string")
        for from_value in from_values:
            if not from_value:
                raise ScoringError(f"{path}: replacement source strings must not be empty")
            previous = owners.get(from_value)
            if previous is not None and previous != rule_index:
                raise ScoringError(
                    f"{path}: duplicate replacement source {from_value!r} "
                    f"across rules {previous} and {rule_index}"
                )
            owners[from_value] = rule_index
        rules.append(Rule(tuple(from_values), to))
    return DictionaryMapper(rules)


def normalized_text(text: str) -> str:
    return unicodedata.normalize("NFKC", text.strip())


def load_manifest(
    path: Path, target: bool, seen: dict[str, tuple[str, int]]
) -> list[ManifestRow]:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ScoringError(f"cannot read manifest {path}: {error}") from error
    rows: list[ManifestRow] = []
    for line_number, line in enumerate(text.splitlines(), 1):
        if line.startswith("#"):
            continue
        columns = line.split("\t")
        row_id = columns[0] if columns else "<missing>"
        context = f"{path}:{line_number} (id={row_id!r})"
        if len(columns) != 4:
            raise ScoringError(
                f"{context}: expected 4 tab-separated columns, got {len(columns)}"
            )
        if not ID_RE.fullmatch(row_id):
            raise ScoringError(
                f"{context}: malformed ID; expected [A-Za-z0-9][A-Za-z0-9_-]*"
            )
        if row_id in seen:
            first_path, first_line = seen[row_id]
            raise ScoringError(
                f"{context}: duplicate ID; first seen at {first_path}:{first_line}"
            )
        seen[row_id] = (str(path), line_number)
        expected = () if not columns[3] else tuple(columns[3].split(","))
        rows.append(
            ManifestRow(
                row_id,
                columns[1],
                columns[2],
                expected,
                str(path),
                line_number,
                target,
            )
        )
    return rows


def preflight_manifests(
    target_rows: Sequence[ManifestRow],
    negative_rows: Sequence[ManifestRow],
    dictionary_surfaces: set[str],
) -> None:
    for row in target_rows:
        if not row.expected_surfaces:
            raise ScoringError(
                f"{row.context()}: target expected_surfaces must be non-empty"
            )
        for surface in row.expected_surfaces:
            if surface not in dictionary_surfaces:
                raise ScoringError(
                    f"{row.context()}: expected surface {surface!r} is absent "
                    "from dictionary 'to' surfaces"
                )
    for row in negative_rows:
        if row.expected_surfaces:
            raise ScoringError(
                f"{row.context()}: negative expected_surfaces must be empty"
            )
        if not normalized_text(row.ref_text):
            raise ScoringError(f"{row.context()}: empty normalized ref_text")


def _expect_exact_keys(
    value: object, expected: set[str], path: Path, line_number: int, kind: str
) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ScoringError(f"{path}:{line_number}: {kind} must be a JSON object")
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        raise ScoringError(
            f"{path}:{line_number}: invalid {kind} fields; "
            f"missing={missing}, extra={extra}"
        )
    return value


def _is_number(value: object) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def parse_metadata(value: object, path: Path) -> RunMetadata:
    record = _expect_exact_keys(value, HEADER_KEYS, path, 1, "metadata header")
    condition = record["condition"]
    if condition not in {"B", "D"}:
        raise ScoringError(f"{path}:1: metadata condition must be 'B' or 'D'")
    num_threads = record["num_threads"]
    hashes = record["manifest_sha256"]
    if not isinstance(num_threads, int) or isinstance(num_threads, bool) or num_threads <= 0:
        raise ScoringError(f"{path}:1: num_threads must be a positive integer")
    if not isinstance(hashes, list) or not all(isinstance(item, str) for item in hashes):
        raise ScoringError(f"{path}:1: manifest_sha256 must be an array of strings")
    if not isinstance(record["git_head"], str) or not isinstance(record["model_dir"], str):
        raise ScoringError(f"{path}:1: git_head and model_dir must be strings")

    digest = record["hotwords_file_sha256"]
    line_count = record["hotwords_line_count"]
    score = record["hotwords_score"]
    if condition == "B":
        if digest is not None or line_count is not None or score is not None:
            raise ScoringError(f"{path}:1: baseline hotword metadata must be null")
    else:
        if not isinstance(digest, str):
            raise ScoringError(f"{path}:1: D hotwords_file_sha256 must be a string")
        if not isinstance(line_count, int) or isinstance(line_count, bool) or line_count < 0:
            raise ScoringError(f"{path}:1: D hotwords_line_count must be non-negative")
        if not _is_number(score) or not math.isfinite(float(score)) or float(score) <= 0:
            raise ScoringError(f"{path}:1: D hotwords_score must be finite and positive")
    return RunMetadata(
        condition,
        digest if isinstance(digest, str) else None,
        line_count if isinstance(line_count, int) else None,
        float(score) if _is_number(score) else None,
        num_threads,
        tuple(hashes),
        record["git_head"],
        record["model_dir"],
    )


def parse_result_record(value: object, path: Path, line_number: int) -> ResultRecord:
    record = _expect_exact_keys(value, RESULT_KEYS, path, line_number, "result record")
    row_id, reference, hypothesis, decode_ms = (
        record["id"],
        record["ref"],
        record["hyp"],
        record["decode_ms"],
    )
    if not isinstance(row_id, str) or not ID_RE.fullmatch(row_id):
        raise ScoringError(f"{path}:{line_number}: invalid result ID {row_id!r}")
    if not isinstance(reference, str) or not isinstance(hypothesis, str):
        raise ScoringError(f"{path}:{line_number}: ref and hyp must be strings")
    if (
        not _is_number(decode_ms)
        or not math.isfinite(float(decode_ms))
        or float(decode_ms) <= 0
    ):
        raise ScoringError(f"{path}:{line_number}: decode_ms must be finite and positive")
    return ResultRecord(row_id, reference, hypothesis, float(decode_ms))


def load_results(path: Path) -> ResultFile:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise ScoringError(f"cannot read results {path}: {error}") from error
    if not lines:
        raise ScoringError(f"{path}: missing metadata header record")
    values: list[object] = []
    for line_number, line in enumerate(lines, 1):
        if not line:
            raise ScoringError(f"{path}:{line_number}: blank JSONL record")
        try:
            values.append(json.loads(line))
        except json.JSONDecodeError as error:
            raise ScoringError(f"{path}:{line_number}: invalid JSON: {error.msg}") from error
    metadata = parse_metadata(values[0], path)
    records = tuple(
        parse_result_record(value, path, line_number)
        for line_number, value in enumerate(values[1:], 2)
    )
    return ResultFile(path, metadata, records)


def validate_bijection(
    result_name: str, manifest_rows: Sequence[ManifestRow], records: Sequence[ResultRecord]
) -> dict[str, ResultRecord]:
    expected = {row.id: row for row in manifest_rows}
    actual: dict[str, ResultRecord] = {}
    for record in records:
        if record.id in actual:
            raise ScoringError(f"{result_name}: duplicate result ID {record.id!r}")
        actual[record.id] = record
    extra = sorted(set(actual) - set(expected))
    if extra:
        raise ScoringError(f"{result_name}: extra result ID {extra[0]!r}")
    missing = sorted(set(expected) - set(actual))
    if missing:
        raise ScoringError(f"{result_name}: missing result ID {missing[0]!r}")
    for row in manifest_rows:
        record = actual[row.id]
        if record.ref != row.ref_text:
            raise ScoringError(
                f"{result_name}: ID {row.id!r} ref mismatch: "
                f"result={record.ref!r}, manifest={row.ref_text!r}"
            )
    return actual


def count_occurrences(text: str, needle: str) -> int:
    if not needle:
        return 0
    count = 0
    position = 0
    while True:
        found = text.find(needle, position)
        if found < 0:
            return count
        count += 1
        position = found + len(needle)


def score_recall(
    rows: Sequence[ManifestRow], mapped_results: dict[str, MappedResult]
) -> RecallScore:
    hits = 0
    total = 0
    all_hit = 0
    for row in rows:
        expected_counts = Counter(row.expected_surfaces)
        row_complete = True
        for surface, expected_count in expected_counts.items():
            found = count_occurrences(mapped_results[row.id].text, surface)
            hits += min(found, expected_count)
            total += expected_count
            if found < expected_count:
                row_complete = False
        all_hit += int(row_complete)
    return RecallScore(hits, total, all_hit, len(rows))


def map_negative_result(
    row: ManifestRow, result: ResultRecord, mapper: DictionaryMapper
) -> tuple[MappedResult, frozenset[str]]:
    mapped_ref, _ref_matches = mapper.apply(row.ref_text)
    mapped_hyp, hyp_matches = mapper.apply(result.hyp)
    false_surfaces = frozenset(
        to for _from, to, _span in hyp_matches if to not in mapped_ref
    )
    return MappedResult(mapped_hyp, tuple(hyp_matches)), false_surfaces


def levenshtein(left: str, right: str) -> int:
    if len(left) > len(right):
        left, right = right, left
    previous = list(range(len(left) + 1))
    for right_index, right_character in enumerate(right, 1):
        current = [right_index]
        for left_index, left_character in enumerate(left, 1):
            current.append(
                min(
                    current[-1] + 1,
                    previous[left_index] + 1,
                    previous[left_index - 1] + (left_character != right_character),
                )
            )
        previous = current
    return previous[-1]


def corpus_cer(
    rows: Sequence[ManifestRow], results: dict[str, ResultRecord]
) -> tuple[int, int]:
    edits = 0
    characters = 0
    for row in rows:
        reference = normalized_text(row.ref_text)
        if not reference:
            raise ScoringError(f"{row.context()}: empty normalized ref_text")
        hypothesis = normalized_text(results[row.id].hyp)
        edits += levenshtein(reference, hypothesis)
        characters += len(reference)
    return edits, characters


def wilson_upper_bound(successes: int, trials: int) -> float:
    if trials <= 0:
        raise ScoringError("Wilson bound requires a positive trial count")
    if not 0 <= successes <= trials:
        raise ScoringError("Wilson bound successes must be between zero and trials")
    proportion = successes / trials
    z_squared = WILSON_Z_95**2
    denominator = 1 + z_squared / trials
    center = proportion + z_squared / (2 * trials)
    radius = WILSON_Z_95 * math.sqrt(
        proportion * (1 - proportion) / trials + z_squared / (4 * trials**2)
    )
    return (center + radius) / denominator


def percentile(values: Sequence[float], proportion: float) -> float:
    if not values:
        raise ScoringError("cannot compute a percentile of an empty sample")
    ordered = sorted(values)
    position = (len(ordered) - 1) * proportion
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] * (1 - fraction) + ordered[upper] * fraction


def score_run(
    result_file: ResultFile,
    all_rows: Sequence[ManifestRow],
    target_rows: Sequence[ManifestRow],
    negative_rows: Sequence[ManifestRow],
    mapper: DictionaryMapper,
    baseline_results: dict[str, ResultRecord],
) -> RunScore:
    results = validate_bijection(str(result_file.path), all_rows, result_file.records)
    mapped_targets: dict[str, MappedResult] = {}
    hallucination_details: list[tuple[str, Match]] = []
    target_matches = 0
    for row in target_rows:
        mapped_text, matches = mapper.apply(results[row.id].hyp)
        mapped_targets[row.id] = MappedResult(mapped_text, tuple(matches))
        target_matches += len(matches)
        expected = set(row.expected_surfaces)
        hallucination_details.extend(
            (row.id, match) for match in matches if match[1] not in expected
        )

    false_ids: set[str] = set()
    false_surfaces: set[tuple[str, str]] = set()
    false_details: list[tuple[str, Match]] = []
    for row in negative_rows:
        _mapped, row_false_surfaces = map_negative_result(row, results[row.id], mapper)
        if row_false_surfaces:
            false_ids.add(row.id)
        false_surfaces.update((row.id, surface) for surface in row_false_surfaces)
        mapped_ref, _ = mapper.apply(row.ref_text)
        _mapped_hyp, hyp_matches = mapper.apply(results[row.id].hyp)
        false_details.extend(
            (row.id, match) for match in hyp_matches if match[1] not in mapped_ref
        )

    edits, characters = corpus_cer(negative_rows, results)
    latencies = [results[row.id].decode_ms for row in all_rows]
    ratios = tuple(
        results[row.id].decode_ms / baseline_results[row.id].decode_ms
        for row in all_rows
    )
    return RunScore(
        result_file,
        score_recall(target_rows, mapped_targets),
        len(hallucination_details),
        target_matches,
        tuple(hallucination_details),
        frozenset(false_ids),
        frozenset(false_surfaces),
        tuple(false_details),
        len(negative_rows),
        edits,
        characters,
        statistics.median(latencies),
        percentile(latencies, 0.95),
        ratios,
    )


def format_rate(numerator: int, denominator: int) -> str:
    if denominator == 0:
        return f"{numerator}/{denominator} (n/a)"
    return f"{numerator}/{denominator} ({numerator / denominator:.1%})"


def format_metadata(metadata: RunMetadata) -> str:
    score = "—" if metadata.hotwords_score is None else f"{metadata.hotwords_score:g}"
    return f"{metadata.condition}; score {score}"


def format_markdown(scores: Sequence[RunScore], baseline: RunScore) -> str:
    lines = [
        "| Results | Run metadata | Term recall | Utterance all-hit | "
        "Hallucinations | False triggers (Wilson 95% upper) | New vs B | "
        "Negative micro-CER | Latency ms p50 (p95) | Paired D/B ratio p50 (p95) |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for score in scores:
        false_count = len(score.false_trigger_ids)
        false_rate = format_rate(false_count, score.negative_count)
        wilson = wilson_upper_bound(false_count, score.negative_count)
        if score.result_file.metadata.condition == "B":
            new_ids: set[str] = set()
        else:
            new_surface_pairs = (
                score.false_trigger_surfaces - baseline.false_trigger_surfaces
            )
            new_ids = {row_id for row_id, _surface in new_surface_pairs}
        lines.append(
            "| "
            + " | ".join(
                [
                    score.result_file.path.name,
                    format_metadata(score.result_file.metadata),
                    format_rate(score.recall.hits, score.recall.total),
                    format_rate(score.recall.all_hit, score.recall.utterances),
                    format_rate(score.hallucinations, score.target_matches),
                    f"{false_rate}; ≤ {wilson:.1%}",
                    format_rate(len(new_ids), score.negative_count),
                    format_rate(score.cer_edits, score.cer_characters),
                    f"{score.latency_p50_ms:.1f} ({score.latency_p95_ms:.1f})",
                    f"{statistics.median(score.latency_ratios):.3f}× "
                    f"({percentile(score.latency_ratios, 0.95):.3f}×)",
                ]
            )
            + " |"
        )

    lines.extend(
        [
            "",
            "CER is corpus micro-CER after stripping surrounding whitespace and NFKC "
            "normalization; Levenshtein distance is over Unicode code points, not "
            "grapheme clusters.",
            "Latency ratios are paired per utterance against B. p95 values are "
            "descriptive only at this sample size.",
            "Mapper provenance spans are half-open Unicode code-point offsets in the "
            "original hypothesis.",
        ]
    )
    for score in scores:
        details: list[str] = []
        for row_id, (from_value, to, span) in score.hallucination_details:
            details.append(f"hallucination {row_id}: {from_value!r}→{to!r}@{span[0]}:{span[1]}")
        for row_id, (from_value, to, span) in score.false_trigger_details:
            details.append(f"false trigger {row_id}: {from_value!r}→{to!r}@{span[0]}:{span[1]}")
        if score.result_file.metadata.condition != "B":
            for row_id, surface in sorted(
                score.false_trigger_surfaces - baseline.false_trigger_surfaces
            ):
                details.append(f"new vs B {row_id}: {surface!r}")
        if details:
            lines.append("")
            lines.append(f"{score.result_file.path.name} provenance: " + "; ".join(details))
    return "\n".join(lines)


def validate_and_score(args: argparse.Namespace) -> int:
    mapper = load_dictionary(args.dictionary)
    seen: dict[str, tuple[str, int]] = {}
    target_rows = load_manifest(args.manifest, True, seen)
    negative_rows = load_manifest(args.manifest_negative, False, seen)
    preflight_manifests(target_rows, negative_rows, mapper.to_surfaces)
    print(
        f"validated {len(target_rows)} target rows and {len(negative_rows)} "
        f"negative rows against {len(mapper.rules)} dictionary rules"
    )
    if not args.results:
        return 0

    result_files = [load_results(path) for path in args.results]
    baselines = [item for item in result_files if item.metadata.condition == "B"]
    if len(baselines) != 1:
        raise ScoringError(
            f"results must contain exactly one B metadata header, found {len(baselines)}"
        )
    all_rows = [*target_rows, *negative_rows]
    baseline_results = validate_bijection(
        str(baselines[0].path), all_rows, baselines[0].records
    )
    for result_file in result_files:
        validate_bijection(str(result_file.path), all_rows, result_file.records)
    scores = [
        score_run(
            result_file,
            all_rows,
            target_rows,
            negative_rows,
            mapper,
            baseline_results,
        )
        for result_file in result_files
    ]
    baseline_score = next(
        score for score in scores if score.result_file is baselines[0]
    )
    print(format_markdown(scores, baseline_score))
    return 0


class SelfTests(unittest.TestCase):
    def test_mapper_prefers_a_synthetic_overlapping_prefix(self) -> None:
        mapper = DictionaryMapper(
            [
                Rule(("アイ",), "SHORT"),
                Rule(("アイス",), "LONG"),
            ]
        )
        self.assertEqual(
            mapper.apply("アイスとアイ"),
            (
                "LONGとSHORT",
                [
                    ("アイス", "LONG", (0, 3)),
                    ("アイ", "SHORT", (4, 6)),
                ],
            ),
        )

    def test_mapper_reports_source_span_provenance(self) -> None:
        mapper = DictionaryMapper([Rule(("クバネティス",), "Kubernetes")])
        self.assertEqual(
            mapper.apply("前クバネティス後"),
            (
                "前Kubernetes後",
                [("クバネティス", "Kubernetes", (1, 7))],
            ),
        )

    def test_micro_recall_counts_repeated_expected_terms(self) -> None:
        row = ManifestRow("t1", "", "", ("Term", "Term"), "target.tsv", 1, True)
        complete = score_recall([row], {"t1": MappedResult("Term then Term", ())})
        partial = score_recall([row], {"t1": MappedResult("only Term", ())})
        self.assertEqual((complete.hits, complete.total, complete.all_hit), (2, 2, 1))
        self.assertEqual((partial.hits, partial.total, partial.all_hit), (1, 2, 0))

    def test_false_trigger_excludes_surface_present_in_mapped_reference(self) -> None:
        mapper = DictionaryMapper(
            [Rule(("カフカ",), "Kafka"), Rule(("レディス",), "Redis")]
        )
        row = ManifestRow("n1", "", "カフカを使う", (), "negative.tsv", 1, False)
        result = ResultRecord("n1", row.ref_text, "カフカとレディス", 1.0)
        _mapped, false_surfaces = map_negative_result(row, result, mapper)
        self.assertEqual(false_surfaces, frozenset({"Redis"}))

    def test_corpus_cer_known_pair(self) -> None:
        rows = [ManifestRow("n1", "", " kitten ", (), "negative.tsv", 1, False)]
        results = {"n1": ResultRecord("n1", " kitten ", "sitting", 1.0)}
        edits, characters = corpus_cer(rows, results)
        self.assertEqual((edits, characters), (3, 6))

    def test_bijection_rejects_duplicate_id(self) -> None:
        rows = [ManifestRow("x", "", "ref", (), "m.tsv", 1, False)]
        records = [
            ResultRecord("x", "ref", "a", 1.0),
            ResultRecord("x", "ref", "b", 2.0),
        ]
        with self.assertRaisesRegex(ScoringError, "results.jsonl.*duplicate.*x"):
            validate_bijection("results.jsonl", rows, records)

    def test_bijection_rejects_missing_id(self) -> None:
        rows = [ManifestRow("x", "", "ref", (), "m.tsv", 1, False)]
        with self.assertRaisesRegex(ScoringError, "results.jsonl.*missing.*x"):
            validate_bijection("results.jsonl", rows, [])

    def test_bijection_rejects_extra_id(self) -> None:
        rows = [ManifestRow("x", "", "ref", (), "m.tsv", 1, False)]
        records = [
            ResultRecord("x", "ref", "a", 1.0),
            ResultRecord("y", "ref", "b", 2.0),
        ]
        with self.assertRaisesRegex(ScoringError, "results.jsonl.*extra.*y"):
            validate_bijection("results.jsonl", rows, records)

    def test_bijection_rejects_reference_mismatch(self) -> None:
        rows = [ManifestRow("x", "", "right", (), "m.tsv", 1, False)]
        records = [ResultRecord("x", "wrong", "hyp", 1.0)]
        with self.assertRaisesRegex(ScoringError, "results.jsonl.*x.*ref mismatch"):
            validate_bijection("results.jsonl", rows, records)

    def test_wilson_upper_bound_known_value(self) -> None:
        self.assertAlmostEqual(wilson_upper_bound(0, 15), 0.203883301, places=8)

    def test_preflight_rejects_target_without_expected_surfaces(self) -> None:
        row = ManifestRow("t", "", "ref", (), "target.tsv", 2, True)
        with self.assertRaisesRegex(ScoringError, "target.tsv:2.*non-empty"):
            preflight_manifests([row], [], {"Known"})

    def test_preflight_rejects_negative_with_expected_surfaces(self) -> None:
        row = ManifestRow("n", "", "ref", ("Known",), "negative.tsv", 3, False)
        with self.assertRaisesRegex(ScoringError, "negative.tsv:3.*must be empty"):
            preflight_manifests([], [row], {"Known"})

    def test_preflight_rejects_unknown_surface(self) -> None:
        row = ManifestRow("t", "", "ref", ("Unknown",), "target.tsv", 4, True)
        with self.assertRaisesRegex(ScoringError, "target.tsv:4.*Unknown"):
            preflight_manifests([row], [], {"Known"})

    def test_preflight_rejects_empty_normalized_negative_reference(self) -> None:
        row = ManifestRow("n", "", "  ", (), "negative.tsv", 5, False)
        with self.assertRaisesRegex(ScoringError, "negative.tsv:5.*empty normalized"):
            preflight_manifests([], [row], {"Known"})


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test", action="store_true", help="run the embedded offline fixture suite"
    )
    parser.add_argument("--results", action="append", type=Path, default=[])
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--manifest-negative", type=Path)
    parser.add_argument("--dict", dest="dictionary", type=Path)
    args = parser.parse_args()
    if args.self_test:
        if args.results or args.manifest or args.manifest_negative or args.dictionary:
            parser.error("--self-test cannot be combined with scoring arguments")
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)
        return (
            0
            if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful()
            else 1
        )
    missing = [
        flag
        for flag, value in (
            ("--manifest", args.manifest),
            ("--manifest-negative", args.manifest_negative),
            ("--dict", args.dictionary),
        )
        if value is None
    ]
    if missing:
        parser.error("the following arguments are required: " + ", ".join(missing))
    try:
        return validate_and_score(args)
    except ScoringError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

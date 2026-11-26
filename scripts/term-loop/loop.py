#!/usr/bin/env python3
"""Validate and measure a private personal-vocabulary improvement loop."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import sys
import tempfile
import tomllib
import unicodedata
import unittest
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path


class LoopError(ValueError):
    """Input error whose message contains no sentence or transcript text."""


class SelfTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.terms = self.root / 'terms.toml'
        self.terms.write_text('[[term]]\nto="Alpha"\nvariants=["アルファ"]\n')
        self.sentences = self.root / 'sentences.tsv'
        self.sentences.write_text('# header\na1\tPRIVATE\tPRIVATE\tAlpha\na2\tx\tx\tAlpha\nn1\tx\tx\t\n')

    def test_validate_failures(self):
        validate_inputs(self.terms, self.sentences, 2)
        original = self.sentences.read_text()
        for text in ['a1\tPRIVATE\n', 'bad id\tx\tx\tAlpha\n',
                     original + 'a1\tx\tx\tAlpha\n',
                     original.replace('Alpha', 'Unknown'),
                     original.replace('Alpha', 'Alpha|'),
                     original.replace('n1\tx\tx\t\n', '')]:
            with self.subTest(text=text):
                self.sentences.write_text(text)
                with self.assertRaises(LoopError) as caught:
                    validate_inputs(self.terms, self.sentences, 2)
                self.assertNotIn('PRIVATE', str(caught.exception))
                self.assertIn(str(self.sentences), str(caught.exception))
        self.sentences.write_text(original)
        for minimum in [3, 0]:
            with self.assertRaises(LoopError):
                validate_inputs(self.terms, self.sentences, minimum)
        for text in ['invalid TOML PRIVATE', '[[term]]\nto="A"\nvariants=[]',
                     '[[term]]\nto="A"\nvariants=["v"]\n[[keep]]\nword="v"',
                     'term="PRIVATE"', '[[term]]\nto=""\nvariants=["v"]',
                     '[[term]]\nto="A"\nvariants="v"',
                     '[[term]]\nto="A"\nvariants=[""]',
                     '[[term]]\nto="A"\nvariants=["v","v"]',
                     '[[term]]\nto="A"\nvariants=["v"]\n[[term]]\nto="A"\nvariants=["w"]',
                     '[[keep]]\nword=""']:
            self.terms.write_text(text)
            with self.assertRaises(LoopError):
                validate_inputs(self.terms, self.sentences, 1)
        self.terms.unlink()
        with self.assertRaises(LoopError):
            validate_inputs(self.terms, self.sentences, 1)

    def test_split(self):
        rows = load_sentences(self.sentences)
        self.assertEqual(split_rows(rows), {'a1': 'train', 'a2': 'test', 'n1': 'train'})
        self.assertEqual(split_rows(rows), split_rows(list(reversed(rows))))

    def test_private_write_and_invalid_records(self):
        path = self.root / 'private.txt'
        path.write_text('old')
        path.chmod(0o644)
        private_write(path, 'new')
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        results = self.root / 'bad.jsonl'
        for text in ['', 'PRIVATE', '[]', '{}\n{}\n',
                     '{}\n{"id":"x","hyp":42}\n',
                     '{}\n{"id":"x","hyp":"a"}\n{"id":"x","hyp":null}\n']:
            results.write_text(text)
            with self.assertRaises(LoopError) as caught:
                to_transcripts(results, self.root / 'output')
            self.assertNotIn('PRIVATE', str(caught.exception))
        private_directory(self.root / 'nested' / 'child')
        for directory in [self.root / 'nested', self.root / 'nested' / 'child']:
            self.assertEqual(directory.stat().st_mode & 0o777, 0o700)

    def test_conversion(self):
        results = self.root / 'hyp.jsonl'
        results.write_text('{}\n' + json.dumps(dict(id='a1', hyp='秘密', ref='秘密', decode_ms=1)) +
                           '\n' + json.dumps(dict(id='a2', hyp=None, ref='', decode_ms=1)) + '\n')
        out = self.root / 'transcripts'
        to_transcripts(results, out)
        self.assertEqual(list(json_objects(out / 'results.jsonl')),
                         [dict(at='a1', engine='eval', raw='秘密'), dict(at='a2', engine='eval', raw='')])
        self.assertEqual(out.stat().st_mode & 0o777, 0o700)
        self.assertEqual((out / 'results.jsonl').stat().st_mode & 0o777, 0o600)
        with self.assertRaises(LoopError):
            to_transcripts(results, out)
        to_transcripts(results, out, True)

    def test_train_hyps(self):
        results = self.root / 'hyp.jsonl'
        results.write_text('{}\n' + ''.join(json.dumps(dict(id=i, hyp='h' + i, ref='', decode_ms=1)) + '\n'
                                            for i in ('a1', 'a2', 'n1')))
        out = self.root / 'train.tsv'
        train_hyps(self.sentences, results, out)
        self.assertEqual(out.read_text(), 'a1\tPRIVATE\tha1\nn1\tx\thn1\n')
        self.assertEqual(out.stat().st_mode & 0o777, 0o600)
        for rows in [[dict(id='zz', hyp='')], [dict(id=['a1'], hyp='PRIVATE')],
                     [dict(id='a1', hyp=42)], [dict(id='a1', hyp='PRIVATE')], []]:
            with self.subTest(rows=rows):
                results.write_text('{}\n' + ''.join(json.dumps(r) + '\n' for r in rows))
                with self.assertRaises(LoopError) as caught:
                    train_hyps(self.sentences, results, out)
                self.assertNotIn('PRIVATE', str(caught.exception))

    def test_validate_inputs_edge_cases(self):
        self.sentences.write_text(self.sentences.read_text() + '\n\n')
        validate_inputs(self.terms, self.sentences, 2)
        for to in ['A,B', 'A|B']:
            self.terms.write_text(f'[[term]]\nto="{to}"\nvariants=["v"]\n')
            with self.assertRaises(LoopError) as caught:
                validate_inputs(self.terms, self.sentences, 1)
            self.assertIn('must not contain', str(caught.exception))
        # Beta appears only as a second surface, so both of its rows land in train.
        self.terms.write_text('[[term]]\nto="Alpha"\nvariants=["a"]\n[[term]]\nto="Beta"\nvariants=["b"]\n')
        self.sentences.write_text('a1\tx\tx\tAlpha,Beta\na2\tx\tx\tAlpha\nb1\tx\tx\tAlpha,Beta\nn1\tx\tx\t\n')
        with self.assertRaises(LoopError) as caught:
            validate_inputs(self.terms, self.sentences, 1)
        self.assertIn('both halves', str(caught.exception))

    def test_existing_output_directory_mode_is_kept(self):
        results = self.root / 'hyp.jsonl'
        results.write_text('{}\n' + json.dumps(dict(id='a1', hyp='x')) + '\n')
        existing = self.root / 'shared'
        existing.mkdir(mode=0o755)
        existing.chmod(0o755)
        to_transcripts(results, existing)
        self.assertEqual(existing.stat().st_mode & 0o777, 0o755)

    def test_score(self):
        self.sentences.write_text('a1\tx\tx\tAlpha|ALPHA\na2\tx\tx\tAlpha\na3\tx\tx\tAlpha,Beta\nn1\tx\tx\t\n')
        replay = self.root / 'replay.jsonl'
        records = [dict(at=i, baseline=b, proposal=p, user_hits=h) for i, b, p, h in
                   [('a1', '', 'ＡＬＰＨＡ', []), ('a2', 'Alpha', 'alpha', []),
                    ('a3', 'Alpha', 'Alpha Beta', []), ('n1', '', '', [{}])]]
        replay.write_text(''.join(json.dumps(r) + '\n' for r in records))
        report = score(self.sentences, replay, 'all')
        self.assertIn('| Alpha | 3 | 1 | 2 | 3 |', report)
        self.assertIn('| Beta | 1 | 0 | 1 | 1 |', report)
        self.assertIn('n1', report)
        self.assertIn('Regressions: a2', report)
        self.assertIn('Totals: rows=3 before=1 after=2 after_ci=3', report)
        self.assertIn('| Alpha | 1 | 0 | 1 | 1 |', score(self.sentences, replay, 'test'))
        for changed in [records[:-1], records + [dict(at='extra', baseline='', proposal='', user_hits=[])], records + records[:1]]:
            replay.write_text(''.join(json.dumps(r) + '\n' for r in changed))
            with self.assertRaises(LoopError):
                score(self.sentences, replay, 'all')

    def test_install(self):
        rules, dest = self.root / 'rules.toml', self.root / 'dest.toml'
        rules.write_text(terms_module().HEADER + '\n[[rule]]\nfrom=["v"]\nto="Alpha"\nwhole_word=true\n')
        dest.write_bytes(b'handwritten\r\n')
        with self.assertRaises(LoopError):
            install(rules, dest)
        install(rules, dest, True)
        self.assertEqual(dest.read_bytes(), rules.read_bytes())
        backups = list(self.root.glob('dest.toml.bak-*'))
        self.assertEqual(backups[0].read_bytes(), b'handwritten\r\n')
        install(rules, dest)
        self.assertEqual(len(list(self.root.glob('dest.toml.bak-*'))), 2)
        for path in [dest, *self.root.glob('dest.toml.bak-*')]:
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        for text in ['PRIVATE [', 'rule="PRIVATE"', '[[rule]]\nfrom="v"\nto="x"',
                     '[[rule]]\nfrom=["v"]\nto="x"\nwhole_word="yes"']:
            rules.write_text(text)
            with self.assertRaises(LoopError):
                install(rules, dest, True)


ID_RE = re.compile(r'[A-Za-z0-9][A-Za-z0-9_-]*\Z')


def terms_module():
    # Resolve from this script, never the caller's working directory.
    spec = importlib.util.spec_from_file_location(
        'transcript_terms', Path(__file__).resolve().parents[1] / 'transcript-terms/terms.py')
    module = importlib.util.module_from_spec(spec)
    previous = sys.dont_write_bytecode
    try:
        sys.dont_write_bytecode = True
        spec.loader.exec_module(module)
    finally:
        sys.dont_write_bytecode = previous
    return module


def load_sentences(path):
    rows, seen = [], set()
    for number, line in enumerate(path.read_text(encoding='utf-8').splitlines(), 1):
        if not line.strip() or line.startswith('#'):
            continue
        columns = line.split('\t')
        # Malformed IDs may themselves be private sentence text; never echo them.
        row_id = columns[0] if ID_RE.fullmatch(columns[0]) else '<invalid>'
        context = f'{path}:{number} (id={row_id})'
        if len(columns) != 4:
            raise LoopError(f'{context}: expected four tab-separated columns')
        if row_id == '<invalid>':
            raise LoopError(f'{context}: invalid id')
        if row_id in seen:
            raise LoopError(f'{context}: duplicate id')
        seen.add(row_id)
        surfaces = () if not columns[3] else tuple(columns[3].split(','))
        expected = tuple(tuple(surface.split('|')) for surface in surfaces)
        if any(not alternative for group in expected for alternative in group):
            raise LoopError(f'{context}: empty expected alternative')
        rows.append(dict(id=row_id, ref=columns[2], expected=expected, group=surfaces[0] if surfaces else None,
                         context=context))
    return rows


def validate_inputs(terms_path, sentences, minimum):
    if minimum < 1:
        raise LoopError(f'{sentences}:1 (id=<all>): --min-per-term must be positive')
    module = terms_module()
    if not terms_path.is_file():
        raise LoopError(f'{terms_path}:1 (id=<terms>): terms file does not exist')
    try:
        terms, _ = module.load_terms(terms_path)
    except module.TermError as error:
        raise LoopError(f'{terms_path}:1 (id=<terms>): {error}') from error
    for term in terms:
        if ',' in term or '|' in term:
            raise LoopError(f'{terms_path}:1 (id=<terms>): term.to must not contain "," or "|" '
                            '(they separate surfaces in sentences.tsv)')
    rows = load_sentences(sentences)
    counts = dict.fromkeys(terms, 0)
    for row in rows:
        alternatives = row_terms(row)
        if alternatives - terms.keys():
            raise LoopError(f"{row['context']}: expected alternative is not a term.to")
        for term in alternatives:
            counts[term] += 1
    if any(count < minimum for count in counts.values()):
        raise LoopError(f'{sentences}:1 (id=<all>): insufficient rows per term')
    halves = split_rows(rows)
    seen_in = defaultdict(set)
    for row in rows:
        for term in row_terms(row):
            seen_in[term].add(halves[row['id']])
    if any(seen_in[term] != {'train', 'test'} for term in terms):
        raise LoopError(f'{sentences}:1 (id=<all>): every term needs rows in both halves; '
                        'make it the first surface of more rows')
    negatives = sum(not row['expected'] for row in rows)
    if not negatives:
        raise LoopError(f'{sentences}:1 (id=<all>): at least one negative row required')
    return f'validated {len(terms)} terms, {len(rows)} rows, {negatives} negatives'


def split_rows(rows):
    groups = defaultdict(list)
    for row in rows:
        groups[row['group']].append(row['id'])
    return {row_id: ('train' if index % 2 == 0 else 'test')
            for group in groups.values() for index, row_id in enumerate(sorted(group))}


def json_objects(path):
    for number, line in enumerate(path.read_text(encoding='utf-8').splitlines(), 1):
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise LoopError(f'{path}:{number}: invalid JSON') from error
        if not isinstance(value, dict):
            raise LoopError(f'{path}:{number}: expected JSON object')
        yield value


def private_write(path, text, force=True):
    # Create privately before writing; replacement also fixes existing permissive modes.
    if not force:
        with open(path, 'x', encoding='utf-8', newline='',
                  opener=lambda p, f: os.open(p, f, 0o600)) as stream:
            stream.write(text)
        return
    descriptor, name = tempfile.mkstemp(prefix='.' + path.name + '-', dir=path.parent)
    try:
        with os.fdopen(descriptor, 'w', encoding='utf-8', newline='') as stream:
            stream.write(text)
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def private_directory(path):
    if not path.exists():
        private_directory(path.parent)
        path.mkdir(mode=0o700)


def result_hyps(results):
    """Return {id: hypothesis} from hotword-eval results, in file order."""
    objects = iter(json_objects(results))
    if next(objects, None) is None:
        raise LoopError(f'{results}:1: missing metadata object')
    hyps = {}
    for number, row in enumerate(objects, 2):
        row_id = row.get('id')
        hyp = row.get('hyp')
        if (not isinstance(row_id, str) or not ID_RE.fullmatch(row_id) or row_id in hyps
                or 'hyp' not in row or (hyp is not None and not isinstance(hyp, str))):
            raise LoopError(f'{results}:{number}: invalid or duplicate result record')
        hyps[row_id] = hyp or ''
    return hyps


def to_transcripts(results, out, force=False):
    records = [dict(at=row_id, engine='eval', raw=hyp) for row_id, hyp in result_hyps(results).items()]
    # A new directory is created 0700; an existing one keeps its mode (it may be shared, e.g. ".").
    private_directory(out)
    try:
        private_write(out / 'results.jsonl', ''.join(json.dumps(r, ensure_ascii=False) + '\n'
                                                  for r in records), force)
    except FileExistsError as error:
        raise LoopError(f'{out}/results.jsonl: output exists; use --force') from error


def train_hyps(sentences, results, out):
    """Write id, ref_text and hypothesis for train-half rows only, so tuning never sees the test half."""
    rows = load_sentences(sentences)
    halves = split_rows(rows)
    hyps = result_hyps(results)
    if hyps.keys() != halves.keys():
        # A partial or mismatched run would silently shrink the training set.
        raise LoopError(f'{results}: result ids do not match the sentence ids '
                        f'({len(halves.keys() - hyps.keys())} missing, {len(hyps.keys() - halves.keys())} extra)')
    lines = [f"{row['id']}\t{row['ref']}\t{' '.join(hyps[row['id']].split())}\n" for row in rows
             if halves[row['id']] == 'train']
    private_write(out, ''.join(lines))


def row_terms(row):
    return {a for group in row['expected'] for a in group}


def hit(row, text, ci=False):
    def fold(value):
        value = unicodedata.normalize('NFKC', value)
        return value.casefold() if ci else value
    text = fold(text)
    return all(any(fold(a) in text for a in group) for group in row['expected'])


def score(sentences, replay, half):
    rows = load_sentences(sentences)
    records = {}
    for number, record in enumerate(json_objects(replay), 1):
        row_id = record.get('at')
        if (not isinstance(row_id, str) or not ID_RE.fullmatch(row_id) or row_id in records
                or not all(isinstance(record.get(k), str) for k in ('baseline', 'proposal'))
                or not isinstance(record.get('user_hits'), list)):
            raise LoopError(f'{replay}:{number}: invalid or duplicate replay record')
        records[row_id] = record
    ids = {row['id'] for row in rows}
    if ids != records.keys():
        raise LoopError(f'{replay}: missing ids: {", ".join(sorted(ids - records.keys()))}; '
                        f'extra ids: {", ".join(sorted(records.keys() - ids))}')
    halves = split_rows(rows)
    counts = defaultdict(lambda: [0, 0, 0, 0])
    for row in rows:
        for term in row_terms(row):
            counts[term]  # Include terms with zero rows in the selected half.
    totals, negatives, false_ids, regressions = [0, 0, 0, 0], 0, [], []
    for row in rows:
        row_id = row['id']
        if half != 'all' and halves[row_id] != half:
            continue
        record = records[row_id]
        if not row['expected']:
            negatives += 1
            if record['user_hits']:
                false_ids.append(row_id)
            continue
        values = [1, int(hit(row, record['baseline'])), int(hit(row, record['proposal'])),
                  int(hit(row, record['proposal'], True))]
        totals = [a + b for a, b in zip(totals, values)]
        if values[2] < values[1]:
            regressions.append(row_id)
        for term in row_terms(row):
            counts[term] = [a + b for a, b in zip(counts[term], values)]
    def cell(text):
        return text.replace('&', '&amp;').replace('<', '&lt;').replace('|', '&#124;').replace('\n', '&#10;').replace('\r', '&#13;')
    lines = ['| term | rows | before | after | after_ci |', '| --- | ---: | ---: | ---: | ---: |']
    lines += ['| ' + cell(term) + ' | ' + ' | '.join(map(str, values)) + ' |'
              for term, values in sorted(counts.items())]
    lines += ['', 'Totals: ' + ' '.join(f'{key}={value}' for key, value in
                                       zip(('rows', 'before', 'after', 'after_ci'), totals)),
              f'Negatives: rows={negatives} false_replacements={len(false_ids)} ids={", ".join(sorted(false_ids)) or "none"}',
              'Regressions: ' + (', '.join(sorted(regressions)) or 'none')]
    return '\n'.join(lines) + '\n'


def install(rules, dest, force=False):
    text = rules.read_bytes().decode('utf-8')
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise LoopError(f'{rules}: invalid TOML') from error
    raw_rules = document.get('rule')
    if set(document) != {'rule'} or not isinstance(raw_rules, list):
        raise LoopError(f'{rules}: expected [[rule]] tables')
    for index, rule in enumerate(raw_rules, 1):
        if (not isinstance(rule, dict) or set(rule) - {'from', 'to', 'whole_word'}
                or not isinstance(rule.get('from'), list) or not rule['from']
                or not all(isinstance(v, str) and v for v in rule['from'])
                or not isinstance(rule.get('to'), str)
                or not isinstance(rule.get('whole_word', False), bool)):
            raise LoopError(f'{rules}: invalid rule {index}')
    if dest.exists():
        previous = dest.read_bytes().decode('utf-8')
        if not force and (previous.splitlines() or [''])[0] != terms_module().HEADER:
            raise LoopError(f'{dest}: refusing overwrite without generated header; use --force')
        stamp = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
        private_write(dest.with_name(dest.name + '.bak-' + stamp), previous, False)
    private_write(dest, text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    commands = parser.add_subparsers(dest='command')
    for name, required in [('validate', ('terms', 'sentences')), ('split', ('sentences',)),
                           ('to-transcripts', ('results', 'out')),
                           ('train-hyps', ('sentences', 'results', 'out')), ('score', ('sentences', 'replay')),
                           ('install', ('rules', 'dest'))]:
        command = commands.add_parser(name)
        for flag in required:
            command.add_argument('--' + flag, type=Path, required=True)
        if name in ('install', 'to-transcripts'):
            command.add_argument('--force', action='store_true')
        if name == 'validate':
            command.add_argument('--min-per-term', type=int, default=4)
        if name == 'score':
            command.add_argument('--half', choices=('train', 'test', 'all'), default='all')
            command.add_argument('--out', type=Path)
    args = parser.parse_args()
    if args.self_test:
        if args.command:
            parser.error('--self-test cannot be combined with a subcommand')
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)).wasSuccessful() else 1
    if not args.command:
        parser.error('a subcommand is required')
    try:
        if args.command == 'validate':
            print(validate_inputs(args.terms, args.sentences, args.min_per_term))
        elif args.command == 'split':
            rows = load_sentences(args.sentences)
            halves = split_rows(rows)
            for row in rows:
                print(row['id'] + '\t' + halves[row['id']])
        elif args.command == 'to-transcripts':
            to_transcripts(args.results, args.out, args.force)
        elif args.command == 'train-hyps':
            train_hyps(args.sentences, args.results, args.out)
        elif args.command == 'score':
            report = score(args.sentences, args.replay, args.half)
            if args.out:
                private_write(args.out, report)
            else:
                print(report, end='')
        else:
            install(args.rules, args.dest, args.force)
    except LoopError as error:
        print(f'error: {error}', file=sys.stderr)
        return 1
    except (OSError, UnicodeError):
        # Exception details can include private bytes or decoded input.
        print('error: unable to read or write input/output file', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())

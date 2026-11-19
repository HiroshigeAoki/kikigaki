# Transcript term mining

Python 3.11+ standard-library tool for turning dictation logs into explicitly
approved user replacement rules. Run `python3 scripts/transcript-terms/terms.py
--self-test` for synthetic fixtures (including a local HTTP stub server).

Transcripts and generated files are private: keep them outside git.
Replay output and candidates/terms files are as private as the transcripts. `mine` emits
only tokens and counts; `suggest` sends only each candidate token to the explicitly
provided OpenAI-compatible endpoint, never sentences. `summarize` prints only hit
contexts, limited to ten characters on each side. Review tokens before sending
them to any endpoint.

Example workflow (all data paths below are outside the repository):

```sh
# Copy the app's transcript JSONL files into /private/work/transcripts first.
python3 scripts/transcript-terms/terms.py mine \
  --transcripts /private/work/transcripts --terms /private/work/terms.toml \
  --builtin crates/kikigaki-core/data/builtin-replace.toml --out /private/work/candidates.tsv
python3 scripts/transcript-terms/terms.py suggest \
  --candidates /private/work/candidates.tsv --url "$TERM_MODEL_URL"
# Edit candidates.tsv: check suggested_to, then mark decision approve or keep.
python3 scripts/transcript-terms/terms.py approve \
  --candidates /private/work/candidates.tsv --terms /private/work/terms.toml
python3 scripts/transcript-terms/terms.py build \
  --terms /private/work/terms.toml --out /private/work/replace.toml
cargo run -p kikigaki-core --bin transcript-replay -- \
  --transcripts /private/work/transcripts --user-rules /private/work/replace.toml \
  --out /private/work/replay.jsonl
python3 scripts/transcript-terms/terms.py summarize \
  --replay /private/work/replay.jsonl --haas-since 2026-11-01T00:00:00Z
# After reviewing every hit, install replace.toml at the app's user rules path.
```

A missing terms file starts an empty term collection. `approve` with a
`suggested_to` of `=keep` registers a keep word. Other approved targets append a
variant to an existing term or create a new term. Unmarked rows are ignored.
`build` protects handwritten output; use `--force` only to intentionally replace it.
The optional `--haas-since` labels non-Whisper records at or after that timestamp
as Haas; earlier records (or all non-Whisper records without it) are Reazon.

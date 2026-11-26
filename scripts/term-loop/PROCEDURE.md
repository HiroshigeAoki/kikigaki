# Personal vocabulary loop

Run this procedure with a coding agent. Letting an agent read session history or
transcripts sends that text to the agent's provider. Keep history, terms,
sentences, recordings, hypotheses, rules, and reports in the private directory;
never commit them. Personal terms must never enter the built-in dictionary or
hotwords. Without a coding agent, hand-write `terms.toml` and `sentences.tsv`,
then start at step 4.

1. Create a private directory outside every Git repository. Set `REPO` to the
   checkout path; run subsequent commands in this shell. Use `umask 077` on the
   Mac too. All paths below refer to private files.

   ```sh
   umask 077
   REPO=/absolute/path/to/kikigaki
   WORK=$(mktemp -d "${TMPDIR:-/tmp}/kikigaki-term-loop.XXXXXXXX")
   chmod 700 "$WORK"
   cd "$WORK"
   ```

2. The agent reads **its own session history**, in its native format. This repo
   provides no history parser. List frequent dictated terms and observed
   misrecognized → intended pairs from prompts dictated with kikigaki. Write
   `terms.toml` with the editor command below. `variants` must contain observed
   misrecognitions, and each term needs at least one variant. Add `[[keep]]`
   words for katakana already recognized correctly; they cannot overlap variants.

   ```sh
   "${EDITOR:-vi}" terms.toml
   ```

   ```toml
   [[term]]
   to = "ExampleTool"
   variants = ["エグザンプルツール"]

   [[keep]]
   word = "ツール"
   ```

3. Write at least four natural sentences per term in varied contexts, plus
   about 20 negatives using correct katakana and near-collisions. Use exactly
   four tab-separated columns: `id`, `spoken_text` (as the user would say it),
   `ref_text` (intended spelling), `expected_surfaces`. An optional comment/header
   line starts with `#`. IDs match `[A-Za-z0-9][A-Za-z0-9_-]*` and are unique.
   Expected surfaces are comma-separated, with no trimming; an empty fourth
   column marks a negative. Within a surface, `|` separates acceptable spellings,
   each of which must also be a term `to`. Do not accept an unwanted spelling
   merely to increase the score.

   ```sh
   "${EDITOR:-vi}" sentences.tsv
   ```

4. Validate before recording. Every accepted term spelling needs at least four
   rows and there must be a negative row.

   ```sh
   python3 "$REPO/scripts/term-loop/loop.py" validate --terms terms.toml --sentences sentences.tsv
   ```

5. On the Mac, copy the private inputs into a private directory outside Git and
   record with the same microphone used for dictation. Set `REPO` and `WORK`
   there as in step 1, then run:

   ```sh
   umask 077
   "$REPO/scripts/hotword-eval/record-voice.sh" --manifest sentences.tsv --out "$WORK/wavs"
   ```

   Keep or re-record each take; re-record only reading mistakes. If transcription
   runs elsewhere, transfer the WAVs into that machine's private `wavs/` directory.

6. Set `MODELS` to the Haas model directory. Transcribe using the checkout's
   manifest, WAVs, and output flags (Haas support requires the companion change).

   ```sh
   MODELS=/absolute/path/to/haas-ja-v1.0-onnx-quantized
   cargo run --manifest-path "$REPO/Cargo.toml" --release -p kikigaki-engine --bin hotword-eval -- --engine haas --models-dir "$MODELS" --manifest "$WORK/sentences.tsv" --wavs-dir "$WORK/wavs" --out "$WORK/hyp.jsonl"
   ```

7. Round 0 uses only history-derived variants. Build rules, convert the results
   into a transcript directory, and let the app's Rust replay code apply built-in
   rules (`baseline`) and built-in plus user rules (`proposal`).

   ```sh
   python3 "$REPO/scripts/transcript-terms/terms.py" build --terms terms.toml --out replace.toml
   python3 "$REPO/scripts/term-loop/loop.py" to-transcripts --results hyp.jsonl --out transcripts
   cargo run --manifest-path "$REPO/Cargo.toml" -p kikigaki-core --bin transcript-replay -- --transcripts "$WORK/transcripts" --user-rules "$WORK/replace.toml" --out "$WORK/replay-round0.jsonl"
   python3 "$REPO/scripts/term-loop/loop.py" score --sentences sentences.tsv --replay replay-round0.jsonl --half all --out score-round0.md
   ```

8. Round 1: expose **only training-half hypotheses** to the agent. `train-hyps`
   writes `id`, `ref_text` and the hypothesis for train rows only; the agent reads
   that file, never `hyp.jsonl` itself.

   ```sh
   python3 "$REPO/scripts/term-loop/loop.py" train-hyps --sentences sentences.tsv --results hyp.jsonl --out hyp-train.tsv
   "${EDITOR:-vi}" terms.toml
   python3 "$REPO/scripts/term-loop/loop.py" validate --terms terms.toml --sentences sentences.tsv
   python3 "$REPO/scripts/transcript-terms/terms.py" build --terms terms.toml --out replace.toml
   cargo run --manifest-path "$REPO/Cargo.toml" -p kikigaki-core --bin transcript-replay -- --transcripts "$WORK/transcripts" --user-rules "$WORK/replace.toml" --out "$WORK/replay-round1.jsonl"
   python3 "$REPO/scripts/term-loop/loop.py" score --sentences sentences.tsv --replay replay-round1.jsonl --half test --out score-round1-test.md
   ```

   The agent reads `hyp-train.tsv` to add observed variants. Keep the sentence
   set fixed. Never tune on test hypotheses or test replay text. The headline is
   held-out test accuracy before/after round 1; retain round 0's all-row score.
   A row hits only when every expected surface has some alternative present
   after NFKC normalization. Exact case is primary; `after_ci` uses case folding.
   A multi-term row contributes to each term; totals count each target row once.
   Splits group by the first complete surface (including its alternatives), with
   negatives in one group, and alternate train/test in ID order. Review negative
   false replacements and regression IDs; do not use held-out text to tune rules.

9. Replay past app transcripts. Set `PAST` to a private directory of app JSONL
   files (copy them into the private work directory first). Read every user-rule
   hit in the summary and replay. Any wrong hit requires fixing the term or adding
   a keep word, rebuilding, and replaying again. Recheck negative counts and
   held-out scores after changes, and report that held-out feedback was used if
   it influenced a change. Do not claim a fresh held-out result in that case;
   use a new recording set for a fresh evaluation.

   ```sh
   PAST="$WORK/past-transcripts"
   cargo run --manifest-path "$REPO/Cargo.toml" -p kikigaki-core --bin transcript-replay -- --transcripts "$PAST" --user-rules "$WORK/replace.toml" --out "$WORK/replay-past.jsonl"
   python3 "$REPO/scripts/transcript-terms/terms.py" summarize --replay replay-past.jsonl > replay-past-summary.txt
   ```

10. Once reviewed, install on the Mac that runs the app. Ensure the application
    supports `whole_word`. The installer backs up an existing destination and
    refuses a handwritten destination unless explicitly passed `--force`.

    ```sh
    python3 "$REPO/scripts/term-loop/loop.py" install --rules replace.toml --dest "$HOME/.config/kikigaki/replace.toml"
    ```

# 設定リファレンス

設定ファイルは `~/.config/kikigaki/config.toml`。任意で、省略した項目はすべて既定値のまま動く。

## 全項目と既定値

```toml
engine = "local"
hotkey = "Alt+Space"
long_key = false
long_hotkey = "Alt+Shift+Space"
paste_method = "clipboard"
strip_trailing_period = true
silence_pad_ms = 500
final_timeout_ms = 3000
builtin_replace_dict = false
replace_file = "~/.config/kikigaki/replace.toml"
metrics_path = "~/Library/Logs/kikigaki/latency.jsonl"
models_dir = "~/Library/Application Support/kikigaki/models"

[asr]
num_threads = 4
decoding_method = "modified_beam_search"
hotwords_score = 3.0

[vad]
min_silence_ms = 350
min_speech_ms = 250
max_speech_s = 12.0
threshold = 0.5

[punct]
enabled = "auto"
comma_threshold = 0.5
period_threshold = 0.5

[remote]
ws_url = "ws://127.0.0.1:8766/ingest"
hayamimi_dir = "~/dev/voice-engine/hayamimi"
python = "~/dev/voice-engine/hayamimi/.venv/bin/python"
spawn_sidecar = true
extra_args = ["--serve", "--no-refine"]
connect_timeout_ms = 30000
server_punctuates = true
```

`punct.enabled` は `"auto"` / `true` / `false` を取り、`"auto"` は句読点サポート付きビルドでのみ有効になる。

## 長文キー

`long_key` は真偽値で、既定値は `false`。`true` にするとWhisperによる長文キーを有効にする。設定画面の「長文キー（高精度）を使う」と同じ項目で、Apple Silicon搭載Macのビルドでのみ働く。`long_hotkey` は長文キーの組み合わせで、`hotkey` と同じ組み合わせは指定できない。`long_hotkey` が未設定で `hotkey` がすでに `Alt+Shift+Space` の場合、長文キーの既定値は `Ctrl+Alt+Space` になる。この値は、設定画面で項目を変更したときにファイルへ書き込まれる。

以前の版で `engine = "whisper"` を設定していた場合、起動時に `engine = "local"` と `long_key = true` として読み込む。設定画面で項目を変更すると、この値がファイルにも書き戻される。`engine` に指定できるのは `"local"` と `"remote"` のみ。

## 旧設定からの移行

旧名 `hitorigochi` / `koe` の設定は自動では引き継がれない。移行時のスキーマ変更点は [`config-migration.md`](config-migration.md) にまとめている。

## 置換辞書

`~/.config/kikigaki/replace.toml` の書き方は [README](../README.md#置換辞書) を参照。

`builtin_replace_dict` は真偽値で、既定値は `false`。精査済みのOSSソースから構築してバイナリに同梱した、カタカナ語→英語の内蔵置換辞書を有効にする。`replace.toml` のユーザー定義ルールと学習済みの修正は、内蔵辞書より優先される。

`asr.hotwords_score` は、内蔵辞書を有効化した際に適用される、登録語の認識を促す強さで、既定値は `3.0`。変更はkikigakiの再起動後に反映される。`asr.decoding_method = "greedy_search"` と内蔵辞書を同時に有効にした場合、ホットワードによる認識バイアスは利用できない。ログに警告を出し、認識はバイアスなしのベースライン相当で続行する。置換辞書は引き続き適用される。

動作だけを元に戻す場合は、`builtin_replace_dict = false` に設定するか、設定画面のトグルをオフにする。古いkikigakiバイナリへダウングレードする場合は、`builtin_replace_dict` と `asr.hotwords_score` の行を削除すること。設定スキーマは未知の項目を拒否するため、未対応の行を残したままダウングレードすると起動できない。

メンテナ向け: 生成する `bpe.vocab` のSHA-256固定値は、`ASR_MODEL_ID` が示すモデルの `tokens.txt` に対応している。ASRモデルを更新するときは固定値を更新し、ホットワード評価ハーネスを再実行すること。

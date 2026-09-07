# 設定リファレンス

設定ファイルは `~/.config/kikigaki/config.toml`。任意で、省略した項目はすべて既定値のまま動く。

## 全項目と既定値

```toml
engine = "local"
hotkey = "Alt+Space"
paste_method = "clipboard"
strip_trailing_period = true
silence_pad_ms = 500
final_timeout_ms = 3000
builtin_replace_dict = false
replace_file = "~/.config/kikigaki/replace.toml"
metrics_path = "~/Library/Logs/kikigaki/latency.jsonl"
models_dir = "~/Library/Application Support/kikigaki/models"

[asr]
model = "haas"
num_threads = 4
decoding_method = "modified_beam_search"
hotwords_score = 3.0
haas_hotwords_score = 1.5

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

`hotkey` の既定値はmacOSで `Alt+Space`、Windowsで `Ctrl+Alt+Space`（Windowsでは `Alt+Space` がシステムメニューを開くため）。

`replace_file` / `metrics_path` / `models_dir` の既定値はOSごとに異なる。一覧は [README](../README.md#ファイルの場所)を参照。

`punct.enabled` は `"auto"` / `true` / `false` を取り、`"auto"` は句読点サポート付きビルドでのみ有効になる。

## 廃止した設定と不明な項目

`long_key`、`long_hotkey`、`auto_long_ms`、`[asr]` の `blank_penalty` は、公開前の開発版にだけあった項目で、いまは無視する。読み込むとログに警告を出す。開発版に戻しても設定ファイルを読めるよう、アプリはこれらの項目を書き換えたり削除したりしない。

`engine = "whisper"` は `engine = "local"` として読み込み、警告を出す。`engine` に指定できるのは `"local"` と `"remote"` である。

不明な項目は、名前を含む警告をログに出して無視する。既知の項目で型が違う場合はエラーとなり、アプリに設定エラーを表示する。旧版のトップレベルのリモート設定項目(`ws_url`、`hayamimi_dir`、`python`、`spawn_sidecar`、`extra_args`、`connect_timeout_ms`)も引き続きエラーとなる。これらは `[remote]` へ移す。

開発版でダウンロードしたWhisperモデルは、アプリでは削除しない。約874 MBで、保存先は `<models_dir>/whisper-large-v3-turbo-q8_0/` である。macOSの既定の保存先は `~/Library/Application Support/kikigaki/models/whisper-large-v3-turbo-q8_0/` となる。開発版を使わなくなったら手動で削除できる。

開発版が保存した `<models_dir>/hotwords/bpe.vocab` も、いまは使わない。現在は `hotwords/<model-id>/bpe.vocab` を使うため、このファイルは削除できる。

## 旧設定からの移行

旧名 `hitorigochi` / `koe` の設定は自動では引き継がれない。移行時のスキーマ変更点は [`config-migration.md`](config-migration.md) にまとめている。

## 置換辞書

`~/.config/kikigaki/replace.toml` の書き方は [README](../README.md#置換辞書) を参照。

`builtin_replace_dict` は真偽値で、既定値は `false`。精査済みのOSSソースから構築してバイナリに同梱した、カタカナ語→英語の内蔵置換辞書を有効にする。`replace.toml` のユーザー定義ルールと学習済みの修正は、内蔵辞書より優先される。

`asr.hotwords_score` は、内蔵辞書を有効化した際に適用される、登録語の認識を促す強さで、既定値は `3.0`。変更はkikigakiの再起動後に反映される。`asr.decoding_method = "greedy_search"` と内蔵辞書を同時に有効にした場合、ホットワードによる認識バイアスは利用できない。ログに警告を出し、認識はバイアスなしのベースライン相当で続行する。置換辞書は引き続き適用される。

`asr.model` は `"haas"` (既定)または `"reazonspeech"` を取る。通常は設定画面の「認識モデル」で選ぶ。Haasを選ぶと `model = "haas"`、ReazonSpeechを選ぶと `model = "reazonspeech"` が `[asr]` に保存される。モデル未指定の既存環境でReazonSpeechだけがインストールされている場合は、Haasを選ぶかインストールするまでReazonSpeechを使い続ける。Haasのモデルがすでに入っている場合は、モデル未指定ならHaasを使う。ReazonSpeechを明示的に使い続けるには、`[asr]` に `model = "reazonspeech"` を指定する。変更はkikigakiの再起動後に反映される。

`asr.haas_hotwords_score` は、Haas使用時に `asr.hotwords_score` の代わりに使う認識バイアスの強さで、既定値は `1.5`。

動作だけを元に戻す場合は、`builtin_replace_dict = false` に設定するか、設定画面のトグルをオフにする。古いkikigakiバイナリへダウングレードする場合は、`builtin_replace_dict`、`asr.hotwords_score`、`asr.model`、`asr.haas_hotwords_score` の行を削除すること。古い版は未知の項目を拒否するため、未対応の行を残したままダウングレードすると起動できない。

メンテナ向け: 生成する `bpe.vocab` のSHA-256固定値は、`ASR_MODEL_ID` が示すモデルの `tokens.txt` に対応している。ASRモデルを更新するときは固定値を更新し、ホットワード評価ハーネスを再実行すること。

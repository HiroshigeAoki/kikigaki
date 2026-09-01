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
num_threads = 4
decoding_method = "modified_beam_search"

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

## 旧設定からの移行

旧名 `hitorigochi` / `koe` の設定は自動では引き継がれない。移行時のスキーマ変更点は [`config-migration.md`](config-migration.md) にまとめている。

## 置換辞書

`~/.config/kikigaki/replace.toml` の書き方は [README](../README.md#置換辞書) を参照。

`builtin_replace_dict` は真偽値で、既定値は `false`。精査済みのOSSソースから構築してバイナリに同梱した、カタカナ語→英語の内蔵置換辞書を有効にする。`replace.toml` のユーザー定義ルールと学習済みの修正は、内蔵辞書より優先される。

動作だけを元に戻す場合は、`builtin_replace_dict = false` に設定するか、設定画面のトグルをオフにする。古いkikigakiバイナリへダウングレードする場合は、この行自体を削除すること。古いバイナリは未知の設定項目を含む `config.toml` を拒否するため、行を残したままダウングレードすると起動できない。

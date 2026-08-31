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

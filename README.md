# kikigaki

macOS向けのプッシュトゥトーク型日本語ディクテーションアプリ。`Alt+Space` を押しながら話し、キーを離すと認識結果が最前面のアプリにペーストされる。既定ではモデルのダウンロードを除きネットワークを使わず、音声認識と句読点付与はローカルで実行する。

## インストール (macOS)

[Releases](https://github.com/HiroshigeAoki/kikigaki/releases)から `kikigaki-<version>-macos-arm64.dmg` をダウンロードして開き、`kikigaki.app` をApplicationsにドラッグする。

未公証のため、初回起動は「"kikigaki" にマルウェアが含まれていないことを確認できませんでした」と表示されてブロックされる。システム設定 → プライバシーとセキュリティ → セキュリティ欄の **このまま開く** から起動する。必要なのはインストールごとに一度だけ(macOS 15以降、未公証アプリを右クリック → 開くで起動する方法は廃止された)。ターミナルからなら次のコマンドでも同じ。

```bash
xattr -d com.apple.quarantine /Applications/kikigaki.app
```

初回起動時はオンボーディング画面が開き、マイク権限 → アクセシビリティ権限 → モデルのダウンロードの順に進む。ダウンロードするのはモデルデータのみで約548 MB(展開後は約183 MB)。ONNX Runtimeはアプリに同梱している。リリースは固定の自己署名IDで署名しているため、許可した権限はアップデート後も保持される。

## ファイルの場所

| 項目 | パス |
| --- | --- |
| 設定 | `~/.config/kikigaki/config.toml` |
| 置換辞書 | `~/.config/kikigaki/replace.toml` |
| モデル | `~/Library/Application Support/kikigaki/models` |
| ログ | `~/Library/Logs/kikigaki/kikigaki.<date>.log` |
| レイテンシ計測 | `~/Library/Logs/kikigaki/latency.jsonl` |

## 設定

設定ファイルは任意。以下はすべて既定値で、省略した項目は既定値のまま動く。

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

`punct.enabled` は `"auto"` / `true` / `false` を取り、`"auto"` は句読点サポート付きビルドでのみ有効になる。旧名 `hitorigochi` / `koe` の設定は自動では引き継がれない。移行時のスキーマ変更点は [`docs/config-migration.md`](docs/config-migration.md) にまとめている。

### 置換辞書

`~/.config/kikigaki/replace.toml` に `[[rule]]` を書くと、認識結果を文字列置換できる。ファイルの変更は自動で反映される。

```toml
[[rule]]
from = ["クバネティス", "クーバネティス"]
to = "Kubernetes"
```

一致した認識結果はすべて置換されるため、同音異義語や一般的な単語は避けること。

## 開発

`scripts/mac-sync-build.sh` でビルド用Macへソースを同期してビルドし、Mac側で `open target/release/bundle/macos/kikigaki.app` を実行する。ログは `tail -f ~/Library/Logs/kikigaki/kikigaki.*.log` で追える。環境変数 `KIKIGAKI_MODELS_DIR` は実モデルを使うテストツール専用で、GUIの `models_dir` 設定は上書きしない。

自己署名ID `kikigaki` の秘密鍵は、ビルド用Macの専用キーチェーンとメンテナのパスワードマネージャーにのみ存在し、このリポジトリには含まれない。証明書をローテートした場合(現行の有効期限は2036-08)、次のアップデート後にマイクとアクセシビリティの許可が一度ずつ再要求される。影響はそれだけ。

## クレジット

| プロジェクト | ライセンス | 用途 |
| --- | --- | --- |
| [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) | Apache-2.0 | 音声認識・VADの実行エンジン |
| [ReazonSpeech](https://research.reazon.jp/projects/ReazonSpeech/) | Apache-2.0(モデル) | 日本語の音声認識モデル |
| [Silero VAD](https://github.com/snakers4/silero-vad) | MIT | 発話区間検出モデル |
| [mojicast-punct-onnx](https://huggingface.co/ishiki-emo/mojicast-punct-onnx) | Apache-2.0(モデル) | 句読点付与モデル |
| [ONNX Runtime](https://github.com/microsoft/onnxruntime) | MIT | 句読点モデルの実行 |
| [hayamimi](https://github.com/oboroge0/hayamimi) | MIT | 句読点パイプラインの移植元 |

静的ライブラリの一覧と追加のライセンス情報は [NOTICE](NOTICE) に記載している。

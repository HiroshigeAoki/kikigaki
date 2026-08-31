# kikigaki

日本語特化・完全ローカルのプッシュトゥトーク型音声入力アプリ(macOS)。

Fast, private Japanese voice typing. 100% local and open source.

`Alt+Space` を押しながら話し、キーを離すと認識結果が最前面のアプリにペーストされる。

- 音声認識・発話区間検出・句読点付与をすべて端末内で実行
- 既定ではモデルの初回ダウンロードを除きネットワークを使わない
- 置換辞書でカタカナ語や固有名詞を好みの表記に変換
- 無料・オープンソース(MIT)

## インストール (macOS)

[Releases](https://github.com/HiroshigeAoki/kikigaki/releases)から `kikigaki-<version>-macos-arm64.dmg` をダウンロードして開き、`kikigaki.app` をApplicationsにドラッグする。

未公証のため、初回起動は「"kikigaki" にマルウェアが含まれていないことを確認できませんでした」と表示されてブロックされる。システム設定 → プライバシーとセキュリティ → セキュリティ欄の **このまま開く** から起動する。必要なのはインストールごとに一度だけ(macOS 15以降、未公証アプリを右クリック → 開くで起動する方法は廃止された)。ターミナルからなら次のコマンドでも同じ。

```bash
xattr -d com.apple.quarantine /Applications/kikigaki.app
```

初回起動時はオンボーディング画面が開き、マイク権限 → アクセシビリティ権限 → モデルのダウンロードの順に進む。ダウンロードするのはモデルデータのみで約548 MB(展開後は約183 MB)。ONNX Runtimeはアプリに同梱している。リリースは固定の自己署名IDで署名しているため、許可した権限はアップデート後も保持される。

## プライバシー

音声認識(ReazonSpeech)、発話区間検出(Silero VAD)、句読点付与(mojicast)は、すべて端末上で実行される。既定構成でネットワークを使うのはモデルの初回ダウンロードだけで、音声データや認識結果を外部サーバーへ送信しない。`remote-engine` 構成でも、既定では同一マシン上のローカルプロセス(`ws://127.0.0.1`)に接続する。

## ファイルの場所

| 項目 | パス |
| --- | --- |
| 設定 | `~/.config/kikigaki/config.toml` |
| 置換辞書 | `~/.config/kikigaki/replace.toml` |
| モデル | `~/Library/Application Support/kikigaki/models` |
| ログ | `~/Library/Logs/kikigaki/kikigaki.<date>.log` |
| レイテンシ計測 | `~/Library/Logs/kikigaki/latency.jsonl` |

## 設定

設定ファイルは任意。省略した項目はすべて既定値のまま動く。全項目の一覧と既定値は [`docs/configuration.md`](docs/configuration.md) に記載している。

### 置換辞書

`~/.config/kikigaki/replace.toml` に `[[rule]]` を書くと、認識結果を文字列置換できる。ファイルの変更は自動で反映される。

```toml
[[rule]]
from = ["クバネティス", "クーバネティス"]
to = "Kubernetes"
```

一致した認識結果はすべて置換されるため、同音異義語や一般的な単語は避けること。

クバネティス→Kubernetesのように変換するカタカナ語→英語の内蔵辞書も利用できる。既定では無効で、設定画面の「カタカナ語→英語辞書」または設定項目 `builtin_replace_dict` で有効にする。`replace.toml` のユーザー定義ルールと学習済みの修正は、常に内蔵辞書より優先される。

## ロードマップ

- [x] macOS(Apple Silicon)
- [x] プッシュトゥトーク入力
- [x] 日本語の音声認識(ReazonSpeech)
- [x] 自動句読点
- [x] 置換辞書
- [x] カタカナ語の既定置換辞書
- [ ] 認識精度の改善
- [ ] Windows対応

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
| [mecab-ipadic-NEologd](https://github.com/neologd/mecab-ipadic-neologd) | Apache-2.0 | 内蔵カタカナ語→英語辞書のデータ |
| [Wikidata](https://www.wikidata.org) | CC0 1.0 | 内蔵カタカナ語→英語辞書のデータ |
| [japanese-dev-lingo](https://github.com/Wizcorp/japanese-dev-lingo) | MIT | 内蔵カタカナ語→英語辞書のデータ |

静的ライブラリの一覧と追加のライセンス情報は [NOTICE](NOTICE) に記載している。

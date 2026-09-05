#!/usr/bin/env bash
# Interactive real-voice recorder for the kikigaki hotword evaluation.
# Run ON the Mac, in Terminal (Terminal needs microphone permission:
# System Settings > Privacy & Security > Microphone > Terminal).
#
# Usage:
#   bash record-voice.sh              # record all rows from both TSVs
#   bash record-voice.sh --device 1   # pick another avfoundation mic index
#   bash record-voice.sh --list      # list microphones and exit
#   bash record-voice.sh --only t01   # (re-)record a single id
#
# Per sentence: any key to start, any key to stop, then
#   [k]eep / [p]lay / [r]e-record / [s]kip / [q]uit (single keypress, no Enter).
# Output: ~/eval-wavs-real/<id>.wav (16 kHz mono PCM16, ready for the harness).
# Copy this script plus data/target.tsv and data/negative.tsv into one directory
# on the Mac (e.g. ~/eval-recording/) and run it there. Rejects silent takes
# (virtual devices like "Microsoft Teams Audio" record silence: pick a real mic
# with --list / --device N). If ffmpeg ignores SIGINT at stop, the script
# escalates to SIGKILL after 2 s instead of hanging.
set -euo pipefail

DIR="$(cd "$(dirname -- "$0")" && pwd)"
OUT="$HOME/eval-wavs-real"
DEVICE=":0"
ONLY=""

while [ $# -gt 0 ]; do
  case "$1" in
    --device) DEVICE=":$2"; shift 2 ;;
    --list) ffmpeg -hide_banner -f avfoundation -list_devices true -i "" 2>&1 | sed -n '/audio devices/,$p'; exit 0 ;;
    --only) ONLY="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

command -v ffmpeg >/dev/null || { echo "ffmpeg not found (brew install ffmpeg)"; exit 1; }
mkdir -p "$OUT"
FFPID=""
trap '[ -n "$FFPID" ] && kill -INT "$FFPID" 2>/dev/null' EXIT

record_one() {
  local id="$1" text="$2" tmp="$OUT/.take.wav" final="$OUT/$id.wav"
  while true; do
    printf '\n\033[1m[%s]\033[0m  %s\n' "$id" "$text"
    if [ -f "$final" ]; then printf '  (既に録音あり — 1キーで録り直し、s でスキップ)\n'; fi
    printf '  スペース(どれか1キー)で録音開始 / s でスキップ...'
    IFS= read -r -s -n 1 cmd < /dev/tty || return 1
    printf '\n'
    if [ "$cmd" = "s" ]; then echo "  スキップ"; return 0; fi
    printf '  \033[31m● 録音中\033[0m — 読み終えたらどれか1キー\n'
    rm -f "$tmp"
    ffmpeg -nostdin -hide_banner -loglevel info -f avfoundation -i "$DEVICE" \
      -ar 16000 -ac 1 -sample_fmt s16 "$tmp" >> "$OUT/.ffmpeg.log" 2>&1 &
    local pid=$!
    FFPID=$pid
    IFS= read -r -s -n 1 _ < /dev/tty || true
    kill -INT "$pid" 2>/dev/null || true
    local waited=0
    while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 20 ]; do
      sleep 0.1; waited=$((waited + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
      echo '  (ffmpegが応答しないため強制終了 — マイク許可ダイアログが出ていないか確認してください)'
      kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
    FFPID=""
    if [ ! -s "$tmp" ]; then echo '  録音失敗(空ファイル)— やり直します'; continue; fi
    local dur vol
    dur=$(ffprobe -hide_banner -v error -show_entries format=duration -of csv=p=0 "$tmp" 2>/dev/null || echo "0")
    vol=$(ffmpeg -nostdin -i "$tmp" -af volumedetect -f null - 2>&1 | sed -n 's/.*max_volume: \(.*\) dB/\1/p')
    if [ "${dur%%.*}" = "0" ] && [ "$(printf '%s' "$dur" | cut -c1-3)" != "0.9" ] && awk "BEGIN{exit !($dur < 0.5)}"; then
      echo "  ⚠ 短すぎます(${dur}s)— デバイス不良の可能性。$OUT/.ffmpeg.log と --list を確認してください"
    fi
    if [ -z "$vol" ] || awk "BEGIN{exit !(${vol:--91} <= -90)}"; then
      echo "  ⚠ 無音です(max_volume=${vol:-n/a} dB)。マイク設定を確認: bash record-voice.sh --list でデバイス一覧、--device <番号> で指定。ターミナルのマイク許可(システム設定>プライバシー>マイク)も確認"
      rm -f "$tmp"; continue
    fi
    while true; do
      printf '  %s 秒。 [k]採用 / [p]再生 / [r]録り直し / [s]スキップ / [q]終了: ' "$dur"
      IFS= read -r -s -n 1 cmd < /dev/tty || cmd=q
      printf '%s\n' "$cmd"
      case "$cmd" in
        k|"") mv "$tmp" "$final"; echo "  → 保存 $final"; return 0 ;;
        p) afplay "$tmp" ;;
        r) break ;;
        s) rm -f "$tmp"; echo "  スキップ"; return 0 ;;
        q) rm -f "$tmp"; return 1 ;;
        *) ;;
      esac
    done
  done
}

total=0 done_count=0
for tsv in "$DIR/target.tsv" "$DIR/negative.tsv"; do
  [ -f "$tsv" ] || { echo "missing $tsv (copy the data TSVs next to this script)"; exit 1; }
  while IFS=$'\t' read -r id spoken _rest; do
    case "$id" in \#*|"") continue ;; esac
    if [ -n "$ONLY" ] && [ "$id" != "$ONLY" ]; then continue; fi
    total=$((total + 1))
    if record_one "$id" "$spoken"; then done_count=$((done_count + 1)); else
      echo; echo "中断しました($done_count 件保存済み)。再開はもう一度実行してください(録音済みはスキップ可)。"
      exit 0
    fi
  done < "$tsv"
done

echo
recorded=$(ls "$OUT"/*.wav 2>/dev/null | wc -l | tr -d ' ')
echo "完了: 今回 $done_count / 対象 $total。$OUT に合計 $recorded 個の WAV があります。"
echo "45個そろったら dgx-1 側に知らせてください(回収して評価を回します)。"

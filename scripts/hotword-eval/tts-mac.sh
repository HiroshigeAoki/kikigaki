#!/usr/bin/env bash

set -euo pipefail

# shellcheck is not installed on this host; use bash -n as the lint gate.

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
target_manifest="$script_dir/data/target.tsv"
negative_manifest="$script_dir/data/negative.tsv"
out_dir="${HOME}/.cache/kikigaki/eval-wavs"
dry_run=false

usage() {
    echo "Usage: $0 [--out-dir DIR] [--dry-run]" >&2
}

while (($# > 0)); do
    case "$1" in
        --out-dir)
            if (($# < 2)); then
                echo "error: --out-dir requires a directory" >&2
                usage
                exit 2
            fi
            out_dir=$2
            shift 2
            ;;
        --dry-run)
            dry_run=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage
            exit 2
            ;;
    esac
done

preflight_dir=$(mktemp -d)
remote_dir=""
local_stage=""

cleanup() {
    local exit_status=$?

    if [[ -n "$remote_dir" && -n "${KIKIGAKI_MAC:-}" ]]; then
        ssh -o ControlPath=none "$KIKIGAKI_MAC" bash -s -- "$remote_dir" \
            >/dev/null 2>&1 <<'REMOTE_CLEANUP' || true
set -euo pipefail
dir=$1
case "$dir" in
    /tmp/kikigaki-tts.*) rm -rf -- "$dir" ;;
esac
REMOTE_CLEANUP
    fi
    rm -rf -- "$preflight_dir"
    if [[ -n "$local_stage" ]]; then
        rm -rf -- "$local_stage"
    fi
    return "$exit_status"
}
trap cleanup EXIT

for manifest in "$target_manifest" "$negative_manifest"; do
    if [[ ! -f "$manifest" ]]; then
        echo "error: manifest not found: $manifest" >&2
        exit 1
    fi
done

ids_file="$preflight_dir/ids"
LC_ALL=C awk -F '\t' '
    /^[[:space:]]*$/ || /^#/ { next }
    NF != 4 {
        printf "error: %s:%d: expected 4 tab-separated columns, found %d: %s\n", FILENAME, FNR, NF, $0 > "/dev/stderr"
        failed = 1
        next
    }
    $1 !~ /^[A-Za-z0-9][A-Za-z0-9_-]*$/ {
        printf "error: %s:%d: invalid id %s\n", FILENAME, FNR, $1 > "/dev/stderr"
        failed = 1
        next
    }
    $1 in seen {
        printf "error: %s:%d: duplicate id %s (first seen at %s)\n", FILENAME, FNR, $1, seen[$1] > "/dev/stderr"
        failed = 1
        next
    }
    {
        seen[$1] = FILENAME ":" FNR
        print $1
        if (FILENAME == target_file) {
            target_count++
        } else {
            negative_count++
        }
    }
    END {
        if (failed) {
            exit 1
        }
        printf "%d\n%d\n", target_count, negative_count > counts_file
    }
' target_file="$target_manifest" counts_file="$preflight_dir/counts" \
    "$target_manifest" "$negative_manifest" >"$ids_file"

mapfile -t counts <"$preflight_dir/counts"
target_count=${counts[0]}
negative_count=${counts[1]}
total_count=$((target_count + negative_count))

if $dry_run; then
    command_count=$((total_count * 2))
    echo "Dry run: local preflight passed for $target_count target + $negative_count negative rows."
    echo "Planned remote generation commands: $command_count ($total_count say + $total_count afconvert); no SSH commands executed."
    exit 0
fi

if [[ -z "${KIKIGAKI_MAC:-}" ]]; then
    echo "error: KIKIGAKI_MAC is required (expected user@host)" >&2
    exit 1
fi

remote_dir=$(ssh -o ControlPath=none "$KIKIGAKI_MAC" \
    'mktemp -d /tmp/kikigaki-tts.XXXXXX')
if [[ ! "$remote_dir" =~ ^/tmp/kikigaki-tts\.[A-Za-z0-9]+$ ]]; then
    echo "error: unexpected remote staging directory: $remote_dir" >&2
    remote_dir=""
    exit 1
fi

scp -o ControlPath=none "$target_manifest" "$negative_manifest" \
    "$KIKIGAKI_MAC:$remote_dir/"

ssh -o ControlPath=none "$KIKIGAKI_MAC" bash -s -- "$remote_dir" <<'REMOTE_GENERATE'
set -euo pipefail

dir=$1
if ! say -v '?' | grep -q 'Kyoko'; then
    echo "error: the Kyoko voice is not installed on the remote Mac" >&2
    exit 1
fi
if ! command -v afconvert >/dev/null 2>&1; then
    echo "error: afconvert is not installed on the remote Mac" >&2
    exit 1
fi

for manifest in "$dir/target.tsv" "$dir/negative.tsv"; do
    while IFS=$'\t' read -r id spoken; do
        if ! say -v Kyoko -o "$dir/$id.aiff" "$spoken"; then
            echo "error: say failed for id $id" >&2
            exit 1
        fi
        if ! afconvert -f WAVE -d LEI16@16000 -c 1 \
            "$dir/$id.aiff" "$dir/$id.wav"; then
            echo "error: afconvert failed for id $id" >&2
            exit 1
        fi
        rm -f -- "$dir/$id.aiff"
    done < <(awk -F '\t' '!/^[[:space:]]*$/ && !/^#/ { print $1 "\t" $2 }' "$manifest")
done
REMOTE_GENERATE

mkdir -p -- "$(dirname -- "$out_dir")"
local_stage=$(mktemp -d "${out_dir}.staging.XXXXXX")
scp -o ControlPath=none "$KIKIGAKI_MAC:$remote_dir/*.wav" "$local_stage/"

LC_ALL=C sort "$ids_file" >"$preflight_dir/expected"
find "$local_stage" -maxdepth 1 -type f -name '*.wav' -printf '%f\n' \
    | sed 's/\.wav$//' | LC_ALL=C sort >"$preflight_dir/actual"

missing=$(comm -23 "$preflight_dir/expected" "$preflight_dir/actual")
extra=$(comm -13 "$preflight_dir/expected" "$preflight_dir/actual")
if [[ -n "$missing" || -n "$extra" ]]; then
    if [[ -n "$missing" ]]; then
        echo "error: missing WAV ids:" >&2
        printf '  %s\n' $missing >&2
    fi
    if [[ -n "$extra" ]]; then
        echo "error: unexpected WAV ids:" >&2
        printf '  %s\n' $extra >&2
    fi
    exit 1
fi

mkdir -p -- "$out_dir"
find "$out_dir" -maxdepth 1 -type f -name '*.wav' -delete
mv -- "$local_stage"/*.wav "$out_dir/"

echo "$target_count target + $negative_count negative WAVs published to $out_dir"

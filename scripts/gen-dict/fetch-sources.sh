#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
DICT_DIR="$ROOT/scripts/dict"
CACHE_DIR="$DICT_DIR/cache"
LOCK_PATH="$DICT_DIR/sources.lock.json"
QUERY_PATH="$ROOT/scripts/gen-dict/wikidata-query.rq"

NEOLOGD_COMMIT=abc61e33d8be3d0ead202e6b1df064c72d5ccf11
NEOLOGD_SEED_BLOB=4f05d9e235af92c7d37e2abaf5918af5d640b2c0
NEOLOGD_COPYING_BLOB=43d83b44ffb88a799a3c485908ead4ea7f3c9e4a
NEOLOGD_COPYING_SHA256=428ad012b9b7baf3af430fb730998791da30458e4f88fbcd8ef5ac75a0eed81e
LINGO_COMMIT=fc110ae27c489cba3af98d0c05eec07790caa230
WIKIDATA_ENDPOINT=https://query.wikidata.org/sparql

REFRESH_WIKIDATA=0
case "${1:-}" in
  "") ;;
  --refresh-wikidata) REFRESH_WIKIDATA=1 ;;
  -h|--help)
    echo "usage: $0 [--refresh-wikidata]"
    exit 0
    ;;
  *)
    echo "error: unknown argument: $1" >&2
    exit 2
    ;;
esac
if (( $# > 1 )); then
  echo "error: expected at most one argument" >&2
  exit 2
fi

for command_name in gh base64 xz curl python3; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "error: required command is missing: $command_name" >&2
    exit 1
  fi
done

mkdir -p "$CACHE_DIR"
TODAY=$(date -u +%F)
LAST_FETCHED=0

lock_value() {
  local source_id=$1
  local field=$2
  python3 - "$LOCK_PATH" "$source_id" "$field" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
if not path.exists():
    raise SystemExit(0)
try:
    document = json.loads(path.read_text(encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"error: cannot read {path}: {error}")
for source in document.get("sources", []):
    if source.get("id") == sys.argv[2]:
        value = source.get(sys.argv[3], "")
        if value is not None:
            print(value)
        break
PY
}

file_sha256() {
  python3 - "$1" <<'PY'
import hashlib
import pathlib
import sys

digest = hashlib.sha256()
with pathlib.Path(sys.argv[1]).open("rb") as source:
    for block in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(block)
print(digest.hexdigest())
PY
}

verify_expected_hash() {
  local path=$1
  local expected=$2
  local actual
  actual=$(file_sha256 "$path")
  if [[ $actual != "$expected" ]]; then
    echo "error: sha256 mismatch for $path: expected $expected, got $actual" >&2
    exit 1
  fi
}

fetch_blob() {
  local repository=$1
  local blob_sha=$2
  local destination=$3
  local temporary
  temporary=$(mktemp "$CACHE_DIR/.blob.XXXXXX")
  if ! gh api "repos/$repository/git/blobs/$blob_sha" --jq '.content' \
      | tr -d '\r\n' \
      | base64 -d >"$temporary"; then
    rm -f "$temporary"
    echo "error: failed to fetch Git blob $repository@$blob_sha" >&2
    exit 1
  fi
  if [[ ! -s $temporary ]]; then
    rm -f "$temporary"
    echo "error: Git blob $repository@$blob_sha was empty" >&2
    exit 1
  fi
  mv "$temporary" "$destination"
}

ensure_blob() {
  local source_id=$1
  local repository=$2
  local blob_sha=$3
  local destination=$4
  local locked_hash
  locked_hash=$(lock_value "$source_id" sha256)
  LAST_FETCHED=0
  if [[ -n $locked_hash && -f $destination ]]; then
    verify_expected_hash "$destination" "$locked_hash"
    return
  fi
  fetch_blob "$repository" "$blob_sha" "$destination"
  LAST_FETCHED=1
  if [[ -n $locked_hash ]]; then
    verify_expected_hash "$destination" "$locked_hash"
  fi
}

retrieval_date() {
  local source_id=$1
  local fetched=$2
  local previous
  previous=$(lock_value "$source_id" retrieval_date)
  if [[ $fetched == 1 || -z $previous ]]; then
    echo "$TODAY"
  else
    echo "$previous"
  fi
}

NEOLOGD_SEED_PATH="$CACHE_DIR/neologd-seed.csv.xz"
ensure_blob neologd-seed neologd/mecab-ipadic-neologd \
  "$NEOLOGD_SEED_BLOB" "$NEOLOGD_SEED_PATH"
NEOLOGD_SEED_DATE=$(retrieval_date neologd-seed "$LAST_FETCHED")
if [[ $(wc -c <"$NEOLOGD_SEED_PATH") -ne 41116376 ]]; then
  echo "error: unexpected NEologd seed byte length" >&2
  exit 1
fi
xz -t "$NEOLOGD_SEED_PATH"
NEOLOGD_SEED_SHA256=$(file_sha256 "$NEOLOGD_SEED_PATH")

NEOLOGD_COPYING_PATH="$CACHE_DIR/neologd-COPYING"
ensure_blob neologd-copying neologd/mecab-ipadic-neologd \
  "$NEOLOGD_COPYING_BLOB" "$NEOLOGD_COPYING_PATH"
NEOLOGD_COPYING_DATE=$(retrieval_date neologd-copying "$LAST_FETCHED")
verify_expected_hash "$NEOLOGD_COPYING_PATH" "$NEOLOGD_COPYING_SHA256"

LINGO_BLOB=$(lock_value japanese-dev-lingo-readme blob_sha)
if [[ -z $LINGO_BLOB ]]; then
  LINGO_BLOB=$(gh api \
    "repos/Wizcorp/japanese-dev-lingo/git/trees/$LINGO_COMMIT" \
    --jq '.tree[] | select(.path == "ReadMe.md" and .type == "blob") | .sha')
  if [[ ! $LINGO_BLOB =~ ^[0-9a-f]{40}$ ]]; then
    echo "error: could not resolve the ReadMe.md blob at $LINGO_COMMIT" >&2
    exit 1
  fi
fi
LINGO_PATH="$CACHE_DIR/japanese-dev-lingo-ReadMe.md"
ensure_blob japanese-dev-lingo-readme Wizcorp/japanese-dev-lingo \
  "$LINGO_BLOB" "$LINGO_PATH"
LINGO_DATE=$(retrieval_date japanese-dev-lingo-readme "$LAST_FETCHED")
LINGO_SHA256=$(file_sha256 "$LINGO_PATH")

WIKIDATA_PATH="$CACHE_DIR/wikidata.tsv"
WIKIDATA_LOCKED_SHA256=$(lock_value wikidata sha256)
QUERY_SHA256=$(file_sha256 "$QUERY_PATH")
LOCKED_QUERY_SHA256=$(lock_value wikidata query_sha256)
if [[ $REFRESH_WIKIDATA == 0 && -n $LOCKED_QUERY_SHA256 && $QUERY_SHA256 != "$LOCKED_QUERY_SHA256" ]]; then
  echo "error: Wikidata query changed; rerun with --refresh-wikidata" >&2
  exit 1
fi
WIKIDATA_FETCHED=0
if [[ $REFRESH_WIKIDATA == 1 || -z $WIKIDATA_LOCKED_SHA256 || ! -f $WIKIDATA_PATH ]]; then
  WIKIDATA_TEMP=$(mktemp "$CACHE_DIR/.wikidata.XXXXXX")
  if ! curl --fail-with-body --silent --show-error \
      --request POST \
      --header 'Accept: text/tab-separated-values' \
      --header 'User-Agent: kikigaki-dictionary-generator/1.0' \
      --data-urlencode "query@$QUERY_PATH" \
      --output "$WIKIDATA_TEMP" \
      "$WIKIDATA_ENDPOINT"; then
    rm -f "$WIKIDATA_TEMP"
    echo "error: Wikidata endpoint is unreachable; snapshot was not updated" >&2
    exit 1
  fi
  if [[ ! -s $WIKIDATA_TEMP ]]; then
    rm -f "$WIKIDATA_TEMP"
    echo "error: Wikidata returned an empty snapshot" >&2
    exit 1
  fi
  mv "$WIKIDATA_TEMP" "$WIKIDATA_PATH"
  WIKIDATA_FETCHED=1
elif [[ -n $WIKIDATA_LOCKED_SHA256 ]]; then
  verify_expected_hash "$WIKIDATA_PATH" "$WIKIDATA_LOCKED_SHA256"
fi
WIKIDATA_DATE=$(retrieval_date wikidata "$WIKIDATA_FETCHED")
WIKIDATA_SHA256=$(file_sha256 "$WIKIDATA_PATH")
python3 - "$WIKIDATA_PATH" <<'PY'
import csv
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
with path.open(encoding="utf-8", newline="") as source:
    header = next(csv.reader(source, dialect="excel-tab"), [])
if {name.lstrip("?") for name in header} != {"item", "surface", "reading"}:
    raise SystemExit(f"error: {path} is not a Wikidata item/surface/reading TSV snapshot")
PY

python3 - \
  "$LOCK_PATH" \
  "$NEOLOGD_COMMIT" "$NEOLOGD_SEED_BLOB" "$NEOLOGD_SEED_SHA256" "$NEOLOGD_SEED_DATE" \
  "$NEOLOGD_COPYING_BLOB" "$NEOLOGD_COPYING_SHA256" "$NEOLOGD_COPYING_DATE" \
  "$LINGO_COMMIT" "$LINGO_BLOB" "$LINGO_SHA256" "$LINGO_DATE" \
  "$WIKIDATA_ENDPOINT" "$QUERY_SHA256" "$WIKIDATA_SHA256" "$WIKIDATA_DATE" <<'PY'
import json
import os
import pathlib
import sys
import tempfile

(
    lock_name,
    neologd_commit,
    neologd_seed_blob,
    neologd_seed_hash,
    neologd_seed_date,
    neologd_copying_blob,
    neologd_copying_hash,
    neologd_copying_date,
    lingo_commit,
    lingo_blob,
    lingo_hash,
    lingo_date,
    wikidata_endpoint,
    query_hash,
    wikidata_hash,
    wikidata_date,
) = sys.argv[1:]

document = {
    "version": 1,
    "sources": [
        {
            "id": "neologd-seed",
            "url": f"https://github.com/neologd/mecab-ipadic-neologd/blob/{neologd_commit}/seed/mecab-user-dict-seed.20200910.csv.xz",
            "pinned_commit": neologd_commit,
            "blob_sha": neologd_seed_blob,
            "sha256": neologd_seed_hash,
            "license": "Apache-2.0",
            "retrieval_date": neologd_seed_date,
        },
        {
            "id": "neologd-copying",
            "url": f"https://github.com/neologd/mecab-ipadic-neologd/blob/{neologd_commit}/COPYING",
            "pinned_commit": neologd_commit,
            "blob_sha": neologd_copying_blob,
            "sha256": neologd_copying_hash,
            "license": "Apache-2.0",
            "retrieval_date": neologd_copying_date,
        },
        {
            "id": "japanese-dev-lingo-readme",
            "url": f"https://github.com/Wizcorp/japanese-dev-lingo/blob/{lingo_commit}/ReadMe.md",
            "pinned_commit": lingo_commit,
            "blob_sha": lingo_blob,
            "sha256": lingo_hash,
            "license": "MIT",
            "retrieval_date": lingo_date,
        },
        {
            "id": "wikidata",
            "endpoint": wikidata_endpoint,
            "query": "scripts/gen-dict/wikidata-query.rq",
            "query_sha256": query_hash,
            "snapshot_sha256": wikidata_hash,
            "sha256": wikidata_hash,
            "license": "CC0-1.0",
            "retrieval_date": wikidata_date,
        },
    ],
}

lock_path = pathlib.Path(lock_name)
serialized = (json.dumps(document, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
temporary_name = None
try:
    with tempfile.NamedTemporaryFile(dir=lock_path.parent, delete=False) as temporary:
        temporary_name = temporary.name
        temporary.write(serialized)
        temporary.flush()
        os.fsync(temporary.fileno())
    os.replace(temporary_name, lock_path)
    temporary_name = None
finally:
    if temporary_name is not None:
        pathlib.Path(temporary_name).unlink(missing_ok=True)
PY

echo "wrote source lock manifest: $LOCK_PATH"

#!/usr/bin/env bash
# Sync this checkout (main or any worktree) to the Mac and build it there.
#
#   scripts/mac-sync-build.sh            # sync + cargo tauri build
#   KIKIGAKI_MAC=user@host scripts/mac-sync-build.sh
#
# Destination on the Mac: ~/dev/kikigaki-<branch> (slashes in the branch name become '-'),
# so each worktree gets its own tree and target/ cache. The source SHA is written to
# SYNC_SHA in the destination and printed on both sides.
#
# Signing: if the Mac's keychain search list has the self-signed "kikigaki" identity (dedicated
# keychain ~/Library/Keychains/kikigaki.keychain-db, unlocked with KIKIGAKI_KEYCHAIN_PASSWORD,
# the p12 lives in 1Password → Secrets), the bundle is signed with it so TCC
# grants survive rebuilds. Otherwise ad-hoc. Signing from ssh needs the explicit unlock.
set -euo pipefail

HOST=${KIKIGAKI_MAC:?set KIKIGAKI_MAC to user@host of the build Mac}
ROOT=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
BRANCH=$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)
SHA=$(git -C "$ROOT" rev-parse --short HEAD)
DIRTY=$(git -C "$ROOT" status --porcelain | wc -l | tr -d ' ')
DEST="dev/kikigaki-${BRANCH//\//-}"

echo "source: $ROOT ($BRANCH @ $SHA, $DIRTY uncommitted paths)"
echo "dest:   $HOST:~/$DEST"
printf '%s %s dirty=%s\n' "$BRANCH" "$SHA" "$DIRTY" > "$ROOT/SYNC_SHA"

"$ROOT/scripts/fetch-onnxruntime.sh"

# --exclude .git (no trailing slash) drops both the .git directory of a main checkout and the
# .git *file* of a worktree; a copied worktree .git file makes git on the Mac fail outright.
rsync -az --delete --exclude target --exclude .git --exclude .worktrees "$ROOT/" "$HOST:$DEST/"

ssh "$HOST" "export PATH=\$HOME/.cargo/bin:/opt/homebrew/bin:\$PATH; \
  export SHERPA_ONNX_ARCHIVE_DIR=\$HOME/.cache/sherpa-onnx-archives; \
  cd ~/$DEST && echo \"mac: \$(cat SYNC_SHA)\" && \
  if ! cargo tauri --version 2>/dev/null | grep -qx 'tauri-cli 2.11.4'; then \
    cargo install tauri-cli --version =2.11.4 --locked; \
  fi && \
  cd crates/kikigaki && \
  if security find-identity -v -p codesigning | grep -q '\"kikigaki\"'; then \
    security unlock-keychain -p \"\${KIKIGAKI_KEYCHAIN_PASSWORD:?set KIKIGAKI_KEYCHAIN_PASSWORD (see 1Password)}\" ~/Library/Keychains/kikigaki.keychain-db && \
    SIGN_CFG='{\"bundle\":{\"macOS\":{\"signingIdentity\":\"kikigaki\"}}}'; echo 'mac: signing with identity kikigaki'; \
  else SIGN_CFG='{}'; echo 'mac: identity kikigaki not found, ad-hoc signing (TCC grants reset per build)'; fi && \
  cargo tauri build --bundles app --config \"\$SIGN_CFG\" -- --locked --features devtools 2>&1 | tail -40"

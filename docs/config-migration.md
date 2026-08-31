# Phase 2 configuration migration

Phase 2 uses `~/.config/hitorigochi/config.toml` and groups the remote-engine settings under `[remote]`; move the legacy keys as shown below, keep all other settings at the top level, and review the new local-engine, ASR, VAD, punctuation, model, and metrics defaults in the current configuration schema. The application reports the old path but does not migrate it automatically. The `remote.python` value is now an absolute path, `replace_file` is now a path rather than an optional value, and the default metrics path moved from `~/Library/Logs/koe/latency.jsonl` to `~/Library/Logs/hitorigochi/latency.jsonl`.

| Old top-level key | New key |
| --- | --- |
| `ws_url` | `remote.ws_url` |
| `hayamimi_dir` | `remote.hayamimi_dir` |
| `python` | `remote.python` |
| `spawn_sidecar` | `remote.spawn_sidecar` |
| `extra_args` | `remote.extra_args` |
| `connect_timeout_ms` | `remote.connect_timeout_ms` |

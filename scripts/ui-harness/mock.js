'use strict';
// Mocked Tauri bridge for the kikigaki settings window. Fixtures enter through the same
// boundary the app uses (get_snapshot / invoke), never through markup edits.
(() => {
  const params = new URLSearchParams(location.search);
  const DATA = params.get('data') || 'demo';
  const FAIL = params.get('fail') === '1';

  const now = Date.parse('2026-10-31T09:41:00+09:00');
  const at = (minAgo) => new Date(now - minAgo * 60000).toISOString();

  const DEMO_HISTORY = [
    'ありがとうございます。明日の会議は10時からで大丈夫です。',
    'Kubernetes のデプロイが終わったら連絡します',
    '了解です',
    '資料は共有フォルダに置きました。確認お願いします。',
    'お疲れさまです',
  ].map((text, i) => ({ id: 5 - i, at: at(i * 7 + 1), raw: text, text }));

  const WORST_HISTORY_HEAD = [
    // Unbreakable URL dictated into a chat.
    'ログはここです https://github.com/example-org/mobile-app/actions/runs/18234567890/job/52012345678?pr=142#step:7:1834 を見てください',
    // Single character.
    'あ',
    // A long dictated paragraph (a 60-second utterance under the 長文 key).
    '先ほどの件ですが、来週の火曜日に予定していたリリースは、QA チームからの報告で決済画面に再現性のある不具合が見つかったため、木曜日に延期することになりました。影響範囲は iOS と Android の両方で、Web 版は問題ありません。修正 PR はすでに出ていて、レビューが通り次第ステージングに入れて、水曜日の午後に最終確認をする予定です。関係各所への連絡は私のほうでやっておきますので、何か懸念があれば今日中に教えてください。よろしくお願いします。',
    // Emoji + mixed scripts.
    '了解です👍🏻 Claude Code で直しておきます 🙏',
    // Long English with no Japanese.
    'Please see the attached PDF named Q3-revenue-forecast-v12-final-final-FINAL.pdf before the all-hands on Thursday',
    // Markup-looking text must render literally.
    '<b>太字</b> &amp; **強調** は記号のまま表示されるべき',
    // Whitespace-only result after post-processing.
    '　',
  ];
  const WORST_HISTORY = [];
  for (let i = 0; i < 200; i += 1) {
    const text = i < WORST_HISTORY_HEAD.length ? WORST_HISTORY_HEAD[i] : `${200 - i} 件目の聞き取り。昨日の分も混ざっています。`;
    // Spread across ~30 hours so entries from yesterday show only HH:MM.
    WORST_HISTORY.push({ id: 200 - i, at: at(i * 9 + 1), raw: text, text });
  }

  const LONG_PATH_ERROR = 'load whisper model /Users/yamada.taro/Library/Application Support/com.kikigaki.app/models/whisper-large-v3-turbo-q8_0/ggml-large-v3-turbo-q8_0.bin: Failed to create a new whisper context';

  const baseSettings = {
    hotkey: 'Ctrl+Space',
    long_key: true,
    long_hotkey: 'Ctrl+Shift+Space',
    long_state: { kind: 'ready' },
    punct_choice: 'on_strip',
    long_available: true,
    punct_enabled: true,
    strip_trailing_period: true,
    builtin_replace_dict: true,
    transcript_log: false,
    transcript_dir: '/Users/yamada.taro/Library/Application Support/com.kikigaki.app/transcripts',
    paste_method: 'clipboard',
  };
  const baseStatus = {
    state: 'idle', level: 'ok', phase: 'ready', message: null,
    hotkey: 'Ctrl+Space', punct_enabled: true, strip_trailing_period: true, long_hotkey: 'Ctrl+Shift+Space',
  };

  function snap(over = {}) {
    const settings = { ...baseSettings, ...(over.settings || {}) };
    const status = { ...baseStatus, hotkey: settings.hotkey, long_hotkey: settings.long_key ? settings.long_hotkey : null, ...(over.status || {}) };
    return {
      revision: 1,
      status,
      settings,
      autostart: over.autostart || { enabled: true, available: true },
      onboarding: over.onboarding === undefined ? null : over.onboarding,
      history: over.history || DEMO_HISTORY,
      bootstrap_error: over.bootstrap_error || null,
    };
  }

  const onb = (download, extra = {}) => ({
    microphone: 'authorized', accessibility_trusted: true, models_installed: false,
    download, consent_copy: '438 MB のダウンロード（インストール後 73 MB）', ...extra,
  });
  const dl = (model, done, total, bytes, totalBytes, failed = false) => ({ model, done, total, bytes, total_bytes: totalBytes, failed });

  const WORST_HOTKEYS = { hotkey: 'Cmd+Alt+Ctrl+Shift+Space', long_hotkey: 'Cmd+Alt+Ctrl+Shift+ArrowRight' };

  const FIXTURES = {
    demo: snap(),
    worst: snap({
      settings: { ...WORST_HOTKEYS, long_state: { kind: 'failed', message: LONG_PATH_ERROR }, transcript_log: true },
      status: {
        state: 'disconnected', level: 'error', phase: 'failed',
        message: `Startup failed — ${LONG_PATH_ERROR}`,
      },
      autostart: { enabled: false, available: false },
      history: WORST_HISTORY,
    }),
    empty: snap({ settings: { long_key: false, long_state: null }, history: [] }),
    one: snap({ history: [DEMO_HISTORY[2]] }),
    'hotkeys-long': snap({ settings: WORST_HOTKEYS }),
    'status-warning': snap({ status: { level: 'warning', message: 'Microphone permission denied — System Settings › Privacy' } }),
    'status-paste': snap({ status: { level: 'warning', message: 'Accessibility permission denied — System Settings › Privacy' } }),
    'status-installing': snap({ status: { phase: 'installing', message: 'Downloading reazonspeech-ja-en-2025-01-17 … 37%' } }),
    'status-engine-failed': snap({ status: { state: 'disconnected', level: 'error', phase: 'failed', message: 'Engine restart failed — sherpa-onnx: Failed to load model at /Users/yamada.taro/Library/Application Support/com.kikigaki.app/models/reazonspeech-ja-en-2025-01-17/encoder-epoch-99-avg-1.int8.onnx: Protobuf parsing failed' } }),
    'status-long-failed': snap({ settings: { long_state: { kind: 'failed', message: LONG_PATH_ERROR } }, status: { level: 'warning', message: '長文エンジンが停止しました — 設定から再試行できます' } }),
    'status-long-install-failed': snap({ settings: { long_state: { kind: 'failed', message: 'download whisper-large-v3-turbo-q8_0: error sending request for url (https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q8_0.bin): connection closed before message completed' } }, status: { level: 'warning', message: '長文キーのモデルを準備できませんでした — もう一度お試しください / download whisper-large-v3-turbo-q8_0: error sending request for url (https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q8_0.bin): connection closed before message completed' } }),
    // Shape after the Rust wording change: short Japanese title plus raw detail.
    'status-with-detail': snap({ status: { state: 'disconnected', level: 'error', phase: 'failed', message: '音声エンジンを起動できませんでした', detail: `Startup failed — ${LONG_PATH_ERROR}` } }),
    'status-recording': snap({ status: { state: 'recording' } }),
    'long-installing-0': snap({ settings: { long_state: { kind: 'installing', percent: 0 } } }),
    'long-installing-100': snap({ settings: { long_state: { kind: 'installing', percent: 100 } } }),
    'long-loading': snap({ settings: { long_state: { kind: 'loading' } } }),
    'long-off': snap({ settings: { long_key: false, long_state: null } }),
    'long-unavailable': snap({ settings: { long_key: false, long_state: null, long_available: false } }),
    'onb-fresh': snap({ onboarding: { ...onb(null), microphone: 'not_determined', accessibility_trusted: false } }),
    'onb-mic-denied': snap({ onboarding: { ...onb(null), microphone: 'denied', accessibility_trusted: false } }),
    'onb-dl-0': snap({ onboarding: onb(dl('reazonspeech-ja-en-2025-01-17', 0, 3, 0, 0)) }),
    'onb-dl-mid': snap({ onboarding: onb(dl('reazonspeech-ja-en-2025-01-17', 0, 3, 162_000_000, 438_000_000)) }),
    'onb-dl-100': snap({ onboarding: onb(dl('mojicast-punct', 3, 3, 438_000_000, 438_000_000)) }),
    'onb-dl-over': snap({ onboarding: onb(dl('silero-vad', 2, 3, 2_400_000, 2_300_000)) }),
    'onb-dl-small': snap({ onboarding: onb(dl('silero-vad', 1, 3, 400_000, 2_300_000)) }),
    'onb-dl-failed': snap({ onboarding: onb(dl('reazonspeech-ja-en-2025-01-17', 0, 3, 91_000_000, 438_000_000, true)) }),
    'config-error': snap({ bootstrap_error: { code: 'config_error', message: 'failed to parse /Users/yamada.taro/Library/Application Support/com.kikigaki.app/config.toml: TOML parse error at line 3, column 10\n  |\n3 | hotkey = Cmd+Shift+Space\n  |          ^\nstring values must be quoted, expected literal string' } }),
  };

  const fixture = FIXTURES[DATA] || FIXTURES.demo;
  window.__HARNESS__ = { DATA, names: Object.keys(FIXTURES), fixture };

  const LONG_COMMAND_ERROR = {
    code: 'apply_failed',
    message: 'failed to write /Users/yamada.taro/Library/Application Support/com.kikigaki.app/config.toml: Permission denied (os error 13)',
  };

  window.__TAURI__ = {
    core: {
      invoke(cmd) {
        if (cmd === 'get_snapshot') return new Promise((r) => setTimeout(() => r(structuredClone(window.__HARNESS__.fixture)), 0));
        if (FAIL) return Promise.reject(LONG_COMMAND_ERROR);
        if (cmd === 'apply_settings') return Promise.resolve(structuredClone(fixture.settings));
        if (cmd === 'end_hotkey_capture') return Promise.resolve(structuredClone(fixture.settings));
        return Promise.resolve(null);
      },
    },
    event: { listen: () => Promise.resolve(() => {}) },
  };
})();

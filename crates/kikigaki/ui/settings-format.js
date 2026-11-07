'use strict';
(function (root) {
  function statusDotClass(status) {
    if (status.state === 'recording') return 'rec';
    if (status.level === 'error') return 'err';
    if (status.level === 'warning') return 'warn';
    return status.phase !== 'ready' ? 'load' : '';
  }

  function historyTimeLabel(at, now) {
    const date = new Date(at);
    if (Number.isNaN(date.getTime())) return '';
    const time = [date.getHours(), date.getMinutes()].map((value) => String(value).padStart(2, '0')).join(':');
    const sameDay = (other) => date.getFullYear() === other.getFullYear()
      && date.getMonth() === other.getMonth() && date.getDate() === other.getDate();
    if (sameDay(now)) return time;
    const yesterday = new Date(now);
    yesterday.setDate(yesterday.getDate() - 1);
    if (sameDay(yesterday)) return `昨日 ${time}`;
    return `${date.getMonth() + 1}/${date.getDate()} ${time}`;
  }

  function visibleHistory(entries) {
    return entries.filter((entry) => entry.text.trim() !== '');
  }

  function downloadLabel(progress) {
    if (!(progress.total > 0)) return { percent: null, text: '準備中…' };
    const item = Math.min(progress.done + 1, progress.total);
    if (!progress.total_bytes) {
      return { percent: null, text: `${item} / ${progress.total} 個目を準備中…` };
    }
    const percent = Math.min(100, Math.round(progress.bytes / progress.total_bytes * 100));
    const mb = (bytes) => (bytes / 1e6).toFixed(bytes < 10e6 ? 1 : 0);
    return {
      percent,
      text: `${item} / ${progress.total} 個目 · ${mb(Math.min(progress.bytes, progress.total_bytes))} / ${mb(progress.total_bytes)} MB（${percent}%）`,
    };
  }

  const MODIFIERS = ['Ctrl', 'Alt', 'Shift', 'Cmd'];
  const SYMBOLS = ['⌃', '⌥', '⇧', '⌘'];
  const KEY_LABELS = {
    BracketLeft: '[', BracketRight: ']', Backquote: '`', Backslash: '\\',
    Comma: ',', Equal: '=', Minus: '-', Period: '.', Quote: "'", Semicolon: ';', Slash: '/',
    ArrowLeft: '←', ArrowUp: '↑', ArrowRight: '→', ArrowDown: '↓',
  };

  // Physical key names the shortcut parser accepts and formatChord can display.
  function isNamedKey(name) {
    return /^F(?:[1-9]|1[0-2])$|^(?:Enter|Tab|Escape|Backspace|Delete|Home|End|PageUp|PageDown|Insert)$/.test(name)
      || Object.hasOwn(KEY_LABELS, name);
  }

  function formatChord(chord) {
    if (typeof chord !== 'string' || !chord) return chord;
    const parts = chord.split('+');
    const key = parts.pop();
    if (!key || MODIFIERS.includes(key) || parts.some((part) => !MODIFIERS.includes(part))
      || new Set(parts).size !== parts.length
      || !(/^[A-Za-z0-9]$|^Space$/.test(key) || isNamedKey(key))) return chord;
    return MODIFIERS.map((name, i) => parts.includes(name) ? SYMBOLS[i] : '').join('')
      + (KEY_LABELS[key] || key);
  }

  function longStateView(state, retrying) {
    if (retrying) return { tone: 'busy', text: '再試行中…', percent: null };
    const kind = state && state.kind;
    return {
      tone: kind === 'ready' ? 'ok' : kind === 'failed' ? 'warn'
        : kind === 'installing' || kind === 'loading' ? 'busy' : 'off',
      text: longStateText(state),
      percent: kind === 'installing' && Number.isFinite(state.percent) && state.percent < 100
        ? Math.max(0, state.percent) : null,
    };
  }

  function statusKeysLine(settings) {
    const standard = formatChord(settings.hotkey);
    const keys = settings.long_key && settings.long_available && settings.long_hotkey
      ? `標準 ${standard} ／ 長文 ${formatChord(settings.long_hotkey)}` : standard;
    return `${keys} を押している間だけ聞き取ります`;
  }

  function chordFromKeyEvent({ code, key, modifiers }) {
    let chordKey = key;
    if (/^Key[A-Z]$/.test(code)) chordKey = code.slice(3);
    else if (/^Digit[0-9]$/.test(code)) chordKey = code.slice(5);
    else if (code === 'Space') chordKey = 'Space';
    else if (isNamedKey(code)) chordKey = code;
    else if (key.length === 1) chordKey = key.toUpperCase();
    return [...MODIFIERS.filter((name) => modifiers.includes(name)), chordKey].join('+');
  }

  function longStateText(state) {
    switch (state && state.kind) {
      case 'installing': return state.percent >= 100 ? '展開中…' : `ダウンロード中 ${state.percent}%`;
      case 'loading': return '準備中…';
      case 'ready': return '準備完了';
      case 'failed': return '停止中 — モデルを読み込めませんでした';
      default: return '';
    }
  }

  const KikigakiFormat = {
    formatChord, longStateView, statusKeysLine, statusDotClass, historyTimeLabel, visibleHistory, downloadLabel, chordFromKeyEvent, longStateText,
    HISTORY_PAGE: 20,
  };
  if (typeof module !== 'undefined' && module.exports) module.exports = KikigakiFormat;
  else root.KikigakiFormat = KikigakiFormat;
})(typeof window !== 'undefined' ? window : globalThis);

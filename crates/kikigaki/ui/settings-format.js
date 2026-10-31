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

  function chordFromKeyEvent({ code, key, modifiers }) {
    let chordKey = key;
    if (/^Key[A-Z]$/.test(code)) chordKey = code.slice(3);
    else if (/^Digit[0-9]$/.test(code)) chordKey = code.slice(5);
    else if (code === 'Space') chordKey = 'Space';
    else if (key.length === 1) chordKey = key.toUpperCase();
    return [...modifiers, chordKey].join('+');
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
    statusDotClass, historyTimeLabel, visibleHistory, downloadLabel, chordFromKeyEvent, longStateText,
    HISTORY_PAGE: 20,
  };
  if (typeof module !== 'undefined' && module.exports) module.exports = KikigakiFormat;
  else root.KikigakiFormat = KikigakiFormat;
})(typeof window !== 'undefined' ? window : globalThis);

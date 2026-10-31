'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const f = require('../ui/settings-format.js');

test('statusDotClass prioritizes recording, error, warning, loading, ready', () => {
  for (const [status, expected] of [
    [{ state: 'recording', level: 'error', phase: 'failed' }, 'rec'],
    [{ level: 'error', phase: 'failed' }, 'err'],
    [{ level: 'warning', phase: 'starting' }, 'warn'],
    [{ level: 'info', phase: 'starting' }, 'load'],
    [{ level: 'info', phase: 'ready' }, ''],
  ]) assert.equal(f.statusDotClass(status), expected);
});

test('historyTimeLabel uses local days, midnight, year rollover and invalid dates', () => {
  const now = new Date(2026, 0, 1, 0, 5);
  assert.equal(f.historyTimeLabel(new Date(2026, 0, 1, 0, 1), now), '00:01');
  assert.equal(f.historyTimeLabel(new Date(2025, 11, 31, 23, 59), now), '昨日 23:59');
  assert.equal(f.historyTimeLabel(new Date(2025, 11, 30, 9, 2).toISOString(), now), '12/30 09:02');
  assert.equal(f.historyTimeLabel('invalid', now), '');
});

test('visibleHistory filters empty, ASCII and ideographic whitespace without changing entries', () => {
  const entry = { text: '  声  ' };
  const entries = [{ text: '' }, { text: ' \t\n' }, { text: '\u3000' }, entry];
  assert.deepEqual(f.visibleHistory(entries), [entry]);
  assert.equal(entries.length, 4);
});

test('downloadLabel handles unknown totals and clamps file number', () => {
  assert.deepEqual(f.downloadLabel({ done: 0, total: 3, bytes: 0 }), {
    percent: null, text: '1 / 3 個目を準備中…',
  });
  assert.deepEqual(f.downloadLabel({ done: 3, total: 3, total_bytes: 0 }), {
    percent: null, text: '3 / 3 個目を準備中…',
  });
  assert.deepEqual(f.downloadLabel({ done: 0, total: 0, bytes: 0, total_bytes: 0 }), {
    percent: null, text: '準備中…',
  });
});

test('downloadLabel formats small and large MB, rounds percent and clamps completed bytes', () => {
  assert.deepEqual(f.downloadLabel({ done: 0, total: 2, bytes: 400000, total_bytes: 162000000 }), {
    percent: 0, text: '1 / 2 個目 · 0.4 / 162 MB（0%）',
  });
  assert.deepEqual(f.downloadLabel({ done: 1, total: 2, bytes: 5500000, total_bytes: 10000000 }), {
    percent: 55, text: '2 / 2 個目 · 5.5 / 10 MB（55%）',
  });
  assert.deepEqual(f.downloadLabel({ done: 2, total: 2, bytes: 20000000, total_bytes: 10000000 }), {
    percent: 100, text: '2 / 2 個目 · 10 / 10 MB（100%）',
  });
});

test('chordFromKeyEvent uses physical letters and digits, Space and fallback keys in modifier order', () => {
  assert.equal(f.chordFromKeyEvent({ code: 'KeyA', key: 'å', modifiers: ['Alt'] }), 'Alt+A');
  assert.equal(f.chordFromKeyEvent({ code: 'KeyZ', key: 'z', modifiers: ['Cmd', 'Alt', 'Ctrl', 'Shift'] }), 'Ctrl+Alt+Shift+Cmd+Z');
  assert.equal(f.chordFromKeyEvent({ code: 'Digit9', key: '(', modifiers: ['Shift'] }), 'Shift+9');
  assert.equal(f.chordFromKeyEvent({ code: 'Space', key: ' ', modifiers: ['Ctrl'] }), 'Ctrl+Space');
  assert.equal(f.chordFromKeyEvent({ code: 'BracketLeft', key: 'é', modifiers: ['Alt'] }), 'Alt+BracketLeft');
  assert.equal(f.chordFromKeyEvent({ code: 'ArrowLeft', key: 'ArrowLeft', modifiers: ['Cmd'] }), 'Cmd+ArrowLeft');
});

test('longStateText covers absent, installing, extracting, loading, ready and failed states', () => {
  for (const [state, expected] of [
    [null, ''], [{ kind: 'installing', percent: 42 }, 'ダウンロード中 42%'],
    [{ kind: 'installing', percent: 100 }, '展開中…'],
    [{ kind: 'installing', percent: 101 }, '展開中…'],
    [{ kind: 'loading' }, '準備中…'], [{ kind: 'ready' }, '準備完了'],
    [{ kind: 'failed' }, '停止中 — モデルを読み込めませんでした'],
  ]) assert.equal(f.longStateText(state), expected);
});

test('HISTORY_PAGE is twenty', () => {
  assert.equal(f.HISTORY_PAGE, 20);
});

 test('formatChord canonicalizes symbols and readable physical keys, preserving invalid input', () => {
  for (const [input, expected] of [
    ['Alt+Ctrl+K', '⌃⌥K'], ['Ctrl+Alt+Space', '⌃⌥Space'],
    ['Alt+BracketLeft', '⌥['], ['Cmd+Shift+Alt+Ctrl+ArrowRight', '⌃⌥⇧⌘→'],
    ['Alt+BracketRight', '⌥]'], ['Ctrl+Backquote', '⌃`'], ['Ctrl+Backslash', '⌃\\'],
    ['Ctrl+Comma', '⌃,'], ['Ctrl+Equal', '⌃='], ['Ctrl+Minus', '⌃-'],
    ['Ctrl+Period', '⌃.'], ['Ctrl+Quote', "⌃'"], ['Ctrl+Semicolon', '⌃;'],
    ['Ctrl+Slash', '⌃/'], ['Ctrl+ArrowLeft', '⌃←'], ['Ctrl+ArrowUp', '⌃↑'],
    ['Ctrl+ArrowDown', '⌃↓'], ['Alt+F12', '⌥F12'], ['Ctrl+9', '⌃9'],
    ['', ''], ['garbage', 'garbage'], ['Ctrl++K', 'Ctrl++K'], ['Ctrl+Alt', 'Ctrl+Alt'],
  ]) assert.equal(f.formatChord(input), expected);
});

test('capture falls back to key only without a physical code', () => {
  assert.equal(f.chordFromKeyEvent({code: '', key: 'a', modifiers: ['Cmd', 'Ctrl']}), 'Ctrl+Cmd+A');
  assert.equal(f.chordFromKeyEvent({code: 'F12', key: 'F12', modifiers: ['Alt']}), 'Alt+F12');
});

test('longStateView exposes safe tones and progress only during download', () => {
  for (const [state, tone, percent] of [[null, 'off', null], [{kind:'ready'}, 'ok', null],
    [{kind:'loading'}, 'busy', null], [{kind:'installing',percent:42}, 'busy', 42],
    [{kind:'installing',percent:100}, 'busy', null],
    [{kind:'failed', message:'/private/raw/path'}, 'warn', null]]) {
    assert.deepEqual(f.longStateView(state, false), {tone, text:f.longStateText(state), percent});
  }
  assert.deepEqual(f.longStateView({kind:'failed'}, true), {tone:'busy', text:'再試行中…', percent:null});
});

test('statusKeysLine includes the long chord only when enabled and available', () => {
  const settings = {hotkey:'Ctrl+Alt+Space', long_hotkey:'Ctrl+Shift+Space', long_key:true, long_available:true};
  assert.equal(f.statusKeysLine(settings), '標準 ⌃⌥Space ／ 長文 ⌃⇧Space を押している間だけ聞き取ります');
  for (const patch of [{long_key:false}, {long_available:false}, {long_hotkey:''}])
    assert.equal(f.statusKeysLine({...settings, ...patch}), '⌃⌥Space を押している間だけ聞き取ります');
});

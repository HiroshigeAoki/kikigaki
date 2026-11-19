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

test('chordFromKeyEvent only keeps code names that formatChord can display', () => {
  // JIS-only and numpad codes fall back to the typed key instead of leaking raw code names.
  assert.equal(f.chordFromKeyEvent({ code: 'IntlYen', key: '¥', modifiers: ['Ctrl'] }), 'Ctrl+¥');
  assert.equal(f.chordFromKeyEvent({ code: 'Numpad1', key: '1', modifiers: ['Ctrl'] }), 'Ctrl+1');
  assert.equal(f.chordFromKeyEvent({ code: 'Home', key: 'Home', modifiers: ['Ctrl'] }), 'Ctrl+Home');
});

test('asrModelNote distinguishes saved and applied models', () => {
  for (const model of ['reazonspeech', 'haas'])
    assert.equal(f.asrModelNote(model, model), '聞き取りに使うモデル');
  assert.equal(f.asrModelNote('haas', 'reazonspeech'), '次の起動から切り替わります。Haas は初回に約 480 MB をダウンロードします');
  assert.equal(f.asrModelNote('reazonspeech', 'haas'), '次の起動から切り替わります');
});

test('asrModelFooter describes the applied model', () => {
  assert.equal(f.asrModelFooter('reazonspeech'), 'ReazonSpeech ja-en · Silero VAD · mojicast');
  assert.equal(f.asrModelFooter('haas'), 'Haas-ja v1.0 · Silero VAD');
});

test('statusKeysLine shows the standard chord', () => {
  assert.equal(f.statusKeysLine({hotkey:'Ctrl+Alt+Space'}), '⌃⌥Space を押している間だけ聞き取ります');
});

test('platform chord labels preserve macOS fallback and the wire vocabulary', () => {
  for (const platform of ['macos', undefined, null, 'unsupported']) {
    assert.equal(f.formatChord('Ctrl+Alt+Shift+Cmd+Space', platform), '⌃⌥⇧⌘Space');
  }
  assert.equal(f.formatChord('Cmd+Alt+Ctrl+Shift+Space', 'windows'), 'Ctrl+Alt+Shift+Win+Space');
  assert.equal(f.statusKeysLine({ hotkey: 'Ctrl+Space' }, 'windows'), 'Ctrl+Space を押している間だけ聞き取ります');
  assert.equal(f.formatChord('Ctrl++K', 'windows'), 'Ctrl++K');
  assert.equal(f.chordFromKeyEvent({ code: 'KeyK', key: 'k', modifiers: ['Cmd', 'Ctrl'] }), 'Ctrl+Cmd+K');
});

test('onboarding steps and completion follow the host platform', () => {
  const state = { microphone: 'authorized', accessibility_trusted: false, models_installed: false };
  for (const platform of ['macos', undefined, null, 'unsupported']) {
    assert.deepEqual(f.onboardingLayout(state, platform), {
      showAccessibility: true, stepCount: 3, modelsReady: false, complete: false,
    });
    assert.equal(f.onboardingLayout({ ...state, models_installed: true }, platform).complete, false);
  }
  assert.deepEqual(f.onboardingLayout(state, 'windows'), {
    showAccessibility: false, stepCount: 2, modelsReady: true, complete: false,
  });
  assert.equal(f.onboardingLayout({ ...state, models_installed: true }, 'windows').complete, true);
  assert.equal(f.onboardingLayout({ ...state, microphone: 'denied', models_installed: true }, 'windows').complete, false);
});


test('settings DOM uses the snapshot platform, restores macOS steps, and opens microphone settings', async () => {
  const vm = require('node:vm');
  const fs = require('node:fs');
  const elements = new Map();
  const calls = [];
  function element(id) {
    if (!elements.has(id)) elements.set(id, {
      id, hidden: false, children: [], handlers: {},
      classList: { toggle() {}, add() {}, remove() {} },
      addEventListener(name, handler) { this.handlers[name] = handler; },
      replaceChildren() { this.children = []; },
      appendChild(child) { this.children.push(child); },
      setAttribute() {}, removeAttribute() {},
    });
    return elements.get(id);
  }
  const scenes = ['scene-onboarding', 'scene-settings', 'scene-config-error'].map(element);
  const context = vm.createContext({
    window: { addEventListener() {}, KikigakiFormat: f, __TAURI__: {
      core: { invoke(command, args) {
        calls.push([command, args]);
        return command === 'get_snapshot' ? new Promise(() => {}) : Promise.resolve();
      } },
      event: { listen: () => Promise.resolve() },
    } },
    document: { getElementById: element, createElement: () => ({}), addEventListener() {},
      querySelectorAll: (selector) => selector === '.scene' ? scenes : [] },
  });
  vm.runInContext(fs.readFileSync(require.resolve('../ui/settings.js'), 'utf8'), context);
  const run = (source) => vm.runInContext(source, context);
  run("renderChord('Ctrl+Cmd+Space')");
  assert.equal(element('hkKbd').children[0].textContent, '⌃⌘Space');
  for (const platform of ['windows', 'macos', 'unknown', undefined]) {
    run(`currentSnapshot = ${JSON.stringify({ platform })}`);
    run("renderChord('Ctrl+Alt+Cmd+Space')");
    assert.equal(element('hkKbd').children[0].textContent,
      platform === 'windows' ? 'Ctrl+Alt+Win+Space' : '⌃⌥⌘Space');
    run("showOnboarding({ microphone: 'authorized', accessibility_trusted: false, models_installed: false })");
    assert.equal(element('stepAx').hidden, platform === 'windows');
    assert.equal(element('onboardingHeroStepCount').textContent, platform === 'windows' ? 2 : 3);
    assert.equal(element('stepModelsNumber').textContent, platform === 'windows' ? '2' : '3');
    run("showOnboarding({ microphone: 'authorized', accessibility_trusted: false, models_installed: true })");
    assert.equal(element('scene-settings').hidden, platform !== 'windows');
  }
  run("currentSnapshot = { platform: 'windows' }; showOnboarding({ microphone: 'denied', accessibility_trusted: false, models_installed: false })");
  await element('stepMicButton').handlers.click({ currentTarget: element('stepMicButton') });
  for (const state of ['not_determined', 'busy', 'unavailable']) {
    run(`showOnboarding({ microphone: '${state}', accessibility_trusted: false, models_installed: false })`);
    const copy = f.microphoneStepCopy(state, 'windows');
    assert.equal(element('stepMicButton').textContent, copy.button);
    assert.equal(element('stepMicDetail').textContent, copy.detail);
    assert.equal(element('stepMicButton').disabled, false);
    assert.equal(element('stepMicButton').hidden, false);
    const start = calls.length;
    await element('stepMicButton').handlers.click({ currentTarget: element('stepMicButton') });
    assert.equal(calls[start][0], 'request_microphone_access');
    assert.equal(element('stepMicButton').disabled, false);
  }
  const settingsCall = calls.find(([command]) => command === 'open_settings_pane');
  assert.ok(settingsCall);
  assert.equal(settingsCall[1].pane, 'microphone');
});

test('microphone step copy preserves macOS and describes Windows probe failures', () => {
  for (const platform of ['macos', undefined, null, 'unsupported', 'windows']) {
    for (const state of ['denied', 'restricted']) {
      assert.deepEqual(f.microphoneStepCopy(state, platform), {
        button: platform === 'windows' ? 'Windows の設定を開く' : 'システム設定を開く',
        detail: platform === 'windows' ? 'Windows の設定で許可してください。' : 'システム設定で許可してください。',
      });
    }
    for (const state of ['authorized', 'not_determined', 'unknown', 'busy', 'unavailable']) {
      let detail = state === 'authorized' ? '許可済み' : '文字起こしのために必要です。';
      let button = '許可する';
      if (platform === 'windows' && ['busy', 'unavailable'].includes(state)) {
        button = 'もう一度確認';
        detail = state === 'busy'
          ? 'ほかのアプリがマイクを使っています。閉じてから、もう一度確認してください。'
          : 'マイクが見つかりません。接続してから、もう一度確認してください。';
      }
      assert.deepEqual(f.microphoneStepCopy(state, platform), { button, detail });
    }
  }
});

test('Windows harness fixtures use the default shortcut and cover microphone states', () => {
  const vm = require('node:vm');
  const fs = require('node:fs');
  const source = fs.readFileSync(require.resolve('../../../scripts/ui-harness/mock.js'), 'utf8');
  for (const [name, microphone] of [
    ['windows', null], ['windows-onboarding', 'authorized'], ['windows-mic-denied', 'denied'],
    ['windows-mic-fresh', 'not_determined'], ['windows-mic-busy', 'busy'], ['windows-mic-unavailable', 'unavailable'],
  ]) {
    const context = { window: {}, location: { search: `?data=${name}` }, URLSearchParams };
    vm.runInNewContext(source, context);
    const fixture = context.window.__HARNESS__.fixture;
    assert.equal(context.window.__HARNESS__.names.includes(name), true);
    assert.equal(fixture.platform, 'windows');
    assert.equal(fixture.settings.hotkey, 'Ctrl+Alt+Space');
    assert.equal(fixture.status.hotkey, 'Ctrl+Alt+Space');
    assert.equal(fixture.onboarding?.microphone ?? null, microphone);
  }
});

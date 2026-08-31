'use strict';

// Surface script failures in the window itself: the packaged WebView has no devtools.
window.addEventListener('error', (event) => {
  const sub = document.getElementById('statusSub');
  if (sub) sub.textContent = `script error: ${event.message}`;
});
window.addEventListener('unhandledrejection', (event) => {
  const sub = document.getElementById('statusSub');
  const reason = event.reason && event.reason.message ? event.reason.message : String(event.reason);
  if (sub) sub.textContent = `promise error: ${reason}`;
});

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const EVENT_NAME = 'kikigaki://event';
const MAX_CORRECTION_CHARS = 500;
const MODIFIER_NAMES = ['Meta', 'Alt', 'Control', 'Shift'];
const MODIFIER_SYMBOLS = { Meta: 'Cmd', Alt: 'Alt', Control: 'Ctrl', Shift: 'Shift' };

let appliedRevision = -1;
let bufferedEvents = [];
let hydrated = false;
let currentSnapshot = null;
let capturingHotkey = false;
let autostartPending = false;
let builtinReplaceDictPending = false;
let currentOnboarding = null;
const inlineErrors = new WeakMap();

function errorMessage(error) {
  if (error && typeof error.message === 'string') return error.message;
  return String(error);
}

function showScene(id) {
  for (const scene of document.querySelectorAll('.scene')) {
    scene.hidden = scene.id !== id;
  }
}

function showSettings() {
  showScene('scene-settings');
}

function showConfigError(error) {
  showScene('scene-config-error');
  document.getElementById('configErrorMessage').textContent = errorMessage(error);
}

function statusMainText(status) {
  if (status.message) return status.message;
  switch (status.state) {
    case 'recording':
    case 'finalizing':
      return '聞き取り中';
    case 'disconnected':
      return status.phase === 'starting' ? '準備中' : 'エラー';
    default:
      return status.phase === 'ready' ? '待機中' : '準備中';
  }
}

function renderChord(chord) {
  const container = document.getElementById('hkKbd');
  container.replaceChildren();
  for (const part of chord.split('+').filter(Boolean)) {
    const key = document.createElement('kbd');
    key.textContent = part;
    container.appendChild(key);
  }
}

function applyStatus(status) {
  const dot = document.getElementById('statusDot');
  dot.className = 'dot';
  if (status.state === 'recording') {
    dot.classList.add('rec');
  } else if (status.phase !== 'ready') {
    dot.classList.add('load');
  }
  document.getElementById('statusMain').textContent = statusMainText(status);
  document.getElementById('statusSub').textContent = status.message || `${status.hotkey} を押している間だけ聞き取ります`;
  const strip = status.punct_enabled && status.strip_trailing_period ? '（文末の「。」は除く）' : '';
  document.getElementById('statusHint').textContent = `${status.engine} · 句読点 ${status.punct_enabled ? 'on' : 'off'}${strip}`;
  renderChord(status.hotkey);
}

function applyPunctSeg(settings) {
  const value = !settings.punct_enabled ? 'off' : settings.strip_trailing_period ? 'on_strip' : 'on';
  for (const button of document.querySelectorAll('#punctSeg button')) {
    button.setAttribute('aria-pressed', String(button.dataset.value === value));
  }
}

function applyBuiltinReplaceDict(settings) {
  const control = document.getElementById('builtinReplaceDictSwitch');
  control.setAttribute('aria-checked', String(settings.builtin_replace_dict));
  control.disabled = builtinReplaceDictPending;
}

function cacheSettings(settings) {
  if (currentSnapshot) currentSnapshot = { ...currentSnapshot, settings };
}

function applyAutostart(autostart) {
  const control = document.getElementById('autostartSwitch');
  control.setAttribute('aria-checked', String(autostart.enabled));
  control.disabled = autostartPending || !autostart.available;
  control.title = autostart.available ? '' : '/Applications 以外にインストールされているため設定できません';
}

function formatHistoryTime(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '';
  return new Intl.DateTimeFormat('ja-JP', { hour: '2-digit', minute: '2-digit', hour12: false }).format(date);
}

function emptyMessage(text) {
  const node = document.createElement('div');
  node.className = 'empty';
  node.textContent = text;
  return node;
}

function historyTimeElement(entry) {
  const time = document.createElement('time');
  time.dateTime = entry.at;
  time.textContent = formatHistoryTime(entry.at);
  return time;
}

function createHistoryRow(entry) {
  const row = document.createElement('div');
  row.className = 'item';

  const time = historyTimeElement(entry);

  const text = document.createElement('div');
  text.className = 'txt';
  text.textContent = entry.text;

  const fixButton = document.createElement('button');
  fixButton.className = 'btn small';
  fixButton.type = 'button';
  fixButton.textContent = '直す';
  fixButton.addEventListener('click', () => openCorrectionEditor(entry, row));

  row.append(time, text, fixButton);
  return row;
}

function applyHistory(entries) {
  const list = document.getElementById('historyList');
  list.replaceChildren();
  if (entries.length === 0) {
    list.appendChild(emptyMessage('最近の聞き取りはありません'));
    return;
  }
  for (const entry of entries) list.appendChild(createHistoryRow(entry));
}

function appendDiffPart(container, tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  node.textContent = text;
  container.appendChild(node);
}

function renderCorrectionPreview(container, correction, saveButton) {
  container.replaceChildren();
  saveButton.disabled = correction.kind === 'none';
  if (correction.kind === 'none') {
    appendDiffPart(container, 'span', '', correction.message);
    saveButton.textContent = 'この置き換えを覚える';
    return;
  }

  appendDiffPart(
    container,
    'span',
    '',
    correction.kind === 'sentence' ? '文全体を置き換え:' : '覚える置き換え:',
  );
  appendDiffPart(container, 'mark', '', correction.from);
  appendDiffPart(container, 'span', 'arrow', '→');
  appendDiffPart(container, 'mark', 'to', correction.to);
  saveButton.textContent = correction.kind === 'sentence' ? '文全体を置き換える' : 'この置き換えを覚える';
}

function openCorrectionEditor(entry, existingRow) {
  const row = document.createElement('div');
  row.className = 'item edit';

  const time = historyTimeElement(entry);

  const body = document.createElement('div');
  body.className = 'txt';
  const heard = document.createElement('div');
  heard.className = 'heard';
  heard.textContent = `聞き取り: ${entry.text}`;
  const input = document.createElement('input');
  input.type = 'text';
  input.value = entry.text;
  input.setAttribute('aria-label', '正しい文');
  const diff = document.createElement('div');
  diff.className = 'diff';
  diff.setAttribute('aria-live', 'polite');

  const actions = document.createElement('div');
  actions.className = 'actions';
  const cancelButton = document.createElement('button');
  cancelButton.className = 'btn small';
  cancelButton.type = 'button';
  cancelButton.textContent = 'キャンセル';
  const saveButton = document.createElement('button');
  saveButton.className = 'btn small primary';
  saveButton.type = 'button';
  saveButton.textContent = 'この置き換えを覚える';
  saveButton.disabled = true;
  actions.append(cancelButton, saveButton);
  body.append(heard, input, diff, actions);
  row.append(time, body);
  existingRow.replaceWith(row);
  input.focus();
  input.select();

  let preview = null;
  let previewTimer = null;
  let previewSequence = 0;

  const requestPreview = () => {
    previewSequence += 1;
    const sequence = previewSequence;
    clearTimeout(previewTimer);
    preview = null;
    saveButton.disabled = true;
    diff.replaceChildren();
    const corrected = input.value;
    if (corrected === entry.text) {
      renderCorrectionPreview(diff, { kind: 'none', message: '変更はありません。' }, saveButton);
      return;
    }
    if ([...corrected].length > MAX_CORRECTION_CHARS) {
      renderCorrectionPreview(diff, { kind: 'none', message: `修正は${MAX_CORRECTION_CHARS}文字までです` }, saveButton);
      return;
    }
    previewTimer = setTimeout(async () => {
      try {
        const result = await invoke('preview_correction', { entryId: entry.id, corrected });
        if (!row.isConnected || sequence !== previewSequence) return;
        preview = result;
        renderCorrectionPreview(diff, result, saveButton);
      } catch (error) {
        if (!row.isConnected || sequence !== previewSequence) return;
        renderCorrectionPreview(diff, { kind: 'none', message: errorMessage(error) }, saveButton);
      }
    }, 180);
  };

  input.addEventListener('input', requestPreview);
  cancelButton.addEventListener('click', () => {
    clearTimeout(previewTimer);
    if (currentSnapshot) applyHistory(currentSnapshot.history);
  });
  saveButton.addEventListener('click', async () => {
    if (!preview || (preview.kind !== 'word' && preview.kind !== 'sentence')) return;
    saveButton.disabled = true;
    input.disabled = true;
    cancelButton.disabled = true;
    try {
      await invoke('remember_correction', { entryId: entry.id, corrected: input.value });
      if (currentSnapshot) applyHistory(currentSnapshot.history);
      showInlineError(document.getElementById('historyList'), { message: '覚えました。' }, false);
    } catch (error) {
      input.disabled = false;
      cancelButton.disabled = false;
      saveButton.disabled = false;
      showInlineError(row, error);
    }
  });
  requestPreview();
}

function applyLearnedRules(rules) {
  const list = document.getElementById('learnedList');
  list.replaceChildren();
  if (rules.length === 0) {
    list.appendChild(emptyMessage('覚えた置き換えはありません'));
    return;
  }
  for (const rule of rules) {
    const row = document.createElement('div');
    row.className = 'item';
    const spacer = document.createElement('span');
    const text = document.createElement('div');
    text.className = 'txt';
    text.textContent = `${rule.from.join(' / ')} → ${rule.to}`;
    const deleteButton = document.createElement('button');
    deleteButton.className = 'btn small';
    deleteButton.type = 'button';
    deleteButton.textContent = '削除';
    deleteButton.addEventListener('click', async () => {
      deleteButton.disabled = true;
      try {
        await invoke('delete_learned_rule', { id: rule.id });
      } catch (error) {
        deleteButton.disabled = false;
        showInlineError(row, error);
      }
    });
    row.append(spacer, text, deleteButton);
    list.appendChild(row);
  }
}

function setStepState(stepId, numberId, number, done, current) {
  const step = document.getElementById(stepId);
  step.classList.toggle('done', done);
  step.classList.toggle('cur', current && !done);
  document.getElementById(numberId).textContent = done ? '✓' : String(number);
}

function applyDownloadProgress(progress) {
  const bar = document.getElementById('stepModelsBar');
  const detail = document.getElementById('stepModelsProg');
  bar.hidden = false;
  bar.value = Math.round((progress.bytes / Math.max(progress.total_bytes, 1)) * 100);
  detail.hidden = false;
  detail.textContent = `${progress.model} (${progress.done} / ${progress.total}) · ${(progress.bytes / 1e6).toFixed(0)} / ${(progress.total_bytes / 1e6).toFixed(0)} MB`;
}

function showOnboarding(state) {
  currentOnboarding = state;
  showScene('scene-onboarding');
  const micDone = state.microphone === 'authorized';
  const micBlocked = state.microphone === 'denied' || state.microphone === 'restricted';
  setStepState('stepMic', 'stepMicNumber', 1, micDone, !micDone);
  setStepState('stepAx', 'stepAxNumber', 2, state.accessibility_trusted, micDone && !state.accessibility_trusted);
  setStepState('stepModels', 'stepModelsNumber', 3, state.models_installed, micDone && state.accessibility_trusted && !state.models_installed);

  const micButton = document.getElementById('stepMicButton');
  micButton.textContent = micBlocked ? 'システム設定を開く' : '許可する';
  micButton.disabled = micDone;
  document.getElementById('stepMicDetail').textContent = micDone ? '許可済み' : micBlocked ? 'システム設定で許可してください。' : '文字起こしのために必要です。';

  const axButton = document.getElementById('stepAxButton');
  axButton.disabled = state.accessibility_trusted;
  document.getElementById('stepAxDetail').textContent = state.accessibility_trusted ? '許可済み' : '文字を貼り付けるために必要です。';

  const modelsButton = document.getElementById('stepModelsButton');
  const bar = document.getElementById('stepModelsBar');
  const progress = document.getElementById('stepModelsProg');
  bar.hidden = true;
  progress.hidden = true;
  document.getElementById('stepModelsDetail').textContent = state.models_installed ? 'インストール済み' : state.consent_copy || '';
  if (state.models_installed) {
    modelsButton.textContent = 'インストール済み';
    modelsButton.disabled = true;
  } else if (state.download && state.download.failed) {
    progress.hidden = false;
    progress.textContent = `ダウンロードに失敗しました（${state.download.model}）。もう一度お試しください。`;
    modelsButton.textContent = '再試行';
    modelsButton.disabled = false;
  } else if (state.download) {
    applyDownloadProgress(state.download);
    modelsButton.textContent = 'ダウンロード中';
    modelsButton.disabled = true;
  } else {
    modelsButton.textContent = 'ダウンロード';
    modelsButton.disabled = false;
  }

  if (micDone && state.accessibility_trusted && state.models_installed) showSettings();
}

function applySnapshot(snapshot) {
  currentSnapshot = snapshot;
  applyStatus(snapshot.status);
  applyPunctSeg(snapshot.settings);
  applyBuiltinReplaceDict(snapshot.settings);
  applyAutostart(snapshot.autostart);
  applyHistory(snapshot.history);
  applyLearnedRules(snapshot.learned_rules);
  if (snapshot.bootstrap_error) {
    showConfigError(snapshot.bootstrap_error);
  } else if (snapshot.onboarding) {
    showOnboarding(snapshot.onboarding);
  } else {
    currentOnboarding = null;
    showSettings();
  }
}

function applyRevisionedSnapshot(snapshot) {
  if (snapshot.revision <= appliedRevision) return;
  appliedRevision = snapshot.revision;
  applySnapshot(snapshot);
}

function applyEvent(payload) {
  if (!payload || payload.kind === 'shutdown') return;
  // `UiEventKind` is an internally tagged enum: the snapshot's fields sit next to `kind`.
  if (payload.kind === 'snapshot' && payload.status) applyRevisionedSnapshot(payload);
}

function showInlineError(element, error, isError = true) {
  const anchor = element.closest('.row') || element.closest('.item') || element;
  const previous = inlineErrors.get(anchor);
  if (previous) {
    clearTimeout(previous.timer);
    previous.note.remove();
  }
  const note = document.createElement('p');
  note.className = 'inline-err';
  if (!isError) note.classList.add('ok');
  note.textContent = errorMessage(error);
  anchor.after(note);
  const timer = setTimeout(() => {
    note.remove();
    inlineErrors.delete(anchor);
  }, 5000);
  inlineErrors.set(anchor, { note, timer });
}

const hotkeyButton = document.getElementById('hkButton');
const hotkeyNote = document.getElementById('hkNote');

function finishHotkeyCaptureUi() {
  capturingHotkey = false;
  hotkeyButton.classList.remove('recording');
  document.getElementById('hkCap').textContent = '変更';
}

async function cancelHotkeyCapture() {
  if (!capturingHotkey) return;
  finishHotkeyCaptureUi();
  try {
    const settings = await invoke('end_hotkey_capture', { newChord: null });
    cacheSettings(settings);
    renderChord(settings.hotkey);
  } catch (error) {
    hotkeyNote.textContent = errorMessage(error);
    if (currentSnapshot) renderChord(currentSnapshot.settings.hotkey);
  }
}

hotkeyButton.addEventListener('click', async () => {
  if (capturingHotkey) return;
  try {
    await invoke('begin_hotkey_capture');
    capturingHotkey = true;
    hotkeyButton.classList.add('recording');
    document.getElementById('hkCap').textContent = 'キーを押してください…';
    hotkeyButton.focus();
  } catch (error) {
    hotkeyNote.textContent = errorMessage(error);
  }
});

hotkeyButton.addEventListener('keydown', async (event) => {
  if (!capturingHotkey) return;
  event.preventDefault();
  if (event.key === 'Escape') {
    await cancelHotkeyCapture();
    return;
  }
  if (MODIFIER_NAMES.includes(event.key)) return;
  const modifiers = MODIFIER_NAMES.filter((name) => event.getModifierState(name));
  if (modifiers.length === 0) {
    hotkeyNote.textContent = '修飾キーを 1 つ以上含めてください。';
    return;
  }
  const key = event.code === 'Space' ? 'Space' : event.key.length === 1 ? event.key.toUpperCase() : event.key;
  const chord = [...modifiers.map((name) => MODIFIER_SYMBOLS[name]), key].join('+');
  finishHotkeyCaptureUi();
  try {
    const settings = await invoke('end_hotkey_capture', { newChord: chord });
    cacheSettings(settings);
    renderChord(settings.hotkey);
    hotkeyNote.textContent = '保存しました。すぐに使えます。';
  } catch (error) {
    hotkeyNote.textContent = errorMessage(error);
    if (currentSnapshot) renderChord(currentSnapshot.settings.hotkey);
  }
});

hotkeyButton.addEventListener('blur', () => {
  void cancelHotkeyCapture();
});

for (const button of document.querySelectorAll('#punctSeg button')) {
  button.addEventListener('click', async () => {
    if (button.disabled) return;
    const buttons = [...document.querySelectorAll('#punctSeg button')];
    for (const control of buttons) control.disabled = true;
    try {
      const settings = await invoke('apply_settings', { patch: { punctuation: button.dataset.value } });
      cacheSettings(settings);
      applyPunctSeg(settings);
    } catch (error) {
      if (currentSnapshot) applyPunctSeg(currentSnapshot.settings);
      showInlineError(document.getElementById('punctSeg'), error);
    } finally {
      for (const control of buttons) control.disabled = false;
    }
  });
}

document.getElementById('builtinReplaceDictSwitch').addEventListener('click', async (event) => {
  const control = event.currentTarget;
  if (control.disabled || builtinReplaceDictPending) return;
  const enabled = control.getAttribute('aria-checked') !== 'true';
  builtinReplaceDictPending = true;
  control.disabled = true;
  try {
    // SettingsPatch is camelCase on the wire, while returned snapshot settings stay snake_case.
    const settings = await invoke('apply_settings', { patch: { builtinReplaceDict: enabled } });
    cacheSettings(settings);
    applyBuiltinReplaceDict(settings);
  } catch (error) {
    if (currentSnapshot) applyBuiltinReplaceDict(currentSnapshot.settings);
    showInlineError(control, error);
  } finally {
    builtinReplaceDictPending = false;
    if (currentSnapshot) applyBuiltinReplaceDict(currentSnapshot.settings);
  }
});

document.getElementById('autostartSwitch').addEventListener('click', async (event) => {
  const control = event.currentTarget;
  if (control.disabled) return;
  const previous = control.getAttribute('aria-checked') === 'true';
  autostartPending = true;
  control.disabled = true;
  control.setAttribute('aria-checked', String(!previous));
  try {
    await invoke('set_launch_at_login', { enabled: !previous });
    if (currentSnapshot) {
      currentSnapshot = {
        ...currentSnapshot,
        autostart: { ...currentSnapshot.autostart, enabled: !previous },
      };
    }
  } catch (error) {
    control.setAttribute('aria-checked', String(previous));
    showInlineError(control, error);
  } finally {
    autostartPending = false;
    control.disabled = currentSnapshot ? !currentSnapshot.autostart.available : false;
  }
});

document.getElementById('openConfigButton').addEventListener('click', () => {
  invoke('open_config').catch((error) => showInlineError(document.getElementById('openConfigButton'), error));
});

document.getElementById('quitButton').addEventListener('click', () => {
  invoke('quit').catch((error) => showInlineError(document.getElementById('quitButton'), error));
});

document.getElementById('clearHistoryButton').addEventListener('click', async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  try {
    await invoke('clear_history');
  } catch (error) {
    showInlineError(document.getElementById('historyList'), error);
  } finally {
    button.disabled = false;
  }
});

document.addEventListener('keydown', (event) => {
  if (event.metaKey && event.key.toLowerCase() === 'q') {
    invoke('quit').catch((error) => showInlineError(document.getElementById('quitButton'), error));
  }
});

document.getElementById('stepMicButton').addEventListener('click', async (event) => {
  if (!currentOnboarding) return;
  const button = event.currentTarget;
  button.disabled = true;
  try {
    if (currentOnboarding.microphone === 'denied' || currentOnboarding.microphone === 'restricted') {
      await invoke('open_settings_pane', { pane: 'microphone' });
    } else {
      await invoke('request_microphone_access');
    }
  } catch (error) {
    showInlineError(document.getElementById('stepMic'), error);
  } finally {
    if (currentOnboarding) button.disabled = currentOnboarding.microphone === 'authorized';
  }
});

document.getElementById('stepAxButton').addEventListener('click', async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  try {
    await invoke('open_settings_pane', { pane: 'accessibility' });
  } catch (error) {
    showInlineError(document.getElementById('stepAx'), error);
  } finally {
    if (currentOnboarding) button.disabled = currentOnboarding.accessibility_trusted;
  }
});

document.getElementById('stepModelsButton').addEventListener('click', async (event) => {
  if (!currentOnboarding || currentOnboarding.models_installed || (currentOnboarding.download && !currentOnboarding.download.failed)) return;
  const button = event.currentTarget;
  button.disabled = true;
  try {
    if (currentOnboarding.download && currentOnboarding.download.failed) {
      await invoke('retry_download');
    } else {
      await invoke('start_download');
    }
  } catch (error) {
    button.disabled = false;
    showInlineError(document.getElementById('stepModels'), error);
  }
});

document.getElementById('configErrorOpenButton').addEventListener('click', () => {
  invoke('open_config').catch((error) => {
    const note = document.getElementById('configErrorNote');
    note.hidden = false;
    note.textContent = errorMessage(error);
  });
});

document.getElementById('configErrorRetryButton').addEventListener('click', async (event) => {
  const button = event.currentTarget;
  const note = document.getElementById('configErrorNote');
  button.disabled = true;
  note.hidden = true;
  try {
    // Re-runs bootstrap in Rust; a fresh snapshot (error or ready) arrives over the event stream.
    await invoke('retry_bootstrap');
  } catch (error) {
    note.hidden = false;
    note.textContent = errorMessage(error);
  } finally {
    button.disabled = false;
  }
});

async function bootstrap() {
  await listen(EVENT_NAME, (event) => {
    if (!hydrated) {
      bufferedEvents.push(event.payload);
      return;
    }
    applyEvent(event.payload);
  });

  const snapshot = await invoke('get_snapshot');
  appliedRevision = snapshot.revision;
  applySnapshot(snapshot);
  hydrated = true;
  const replay = bufferedEvents.sort((left, right) => left.revision - right.revision);
  bufferedEvents = [];
  for (const payload of replay) applyEvent(payload);
}

bootstrap().catch((error) => showConfigError(error));

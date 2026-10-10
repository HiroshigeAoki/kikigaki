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
const MODIFIER_NAMES = ['Meta', 'Alt', 'Control', 'Shift'];
const MODIFIER_SYMBOLS = { Meta: 'Cmd', Alt: 'Alt', Control: 'Ctrl', Shift: 'Shift' };

let appliedRevision = -1;
let bufferedEvents = [];
let hydrated = false;
let currentSnapshot = null;
let capturing = null;
let capturePending = null;
let captureEnding = false;
let captureCancelled = false;
let longKeyPending = false;
let retryLongPending = false;
let autostartPending = false;
let builtinReplaceDictPending = false;
let transcriptLogPending = false;
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

function renderChord(key, chord) {
  const container = document.getElementById(key === 'long' ? 'longHkKbd' : 'hkKbd');
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
  document.getElementById('statusHint').textContent = `句読点 ${status.punct_enabled ? 'on' : 'off'}${strip}`;
  renderChord('standard', status.hotkey);
  if (status.long_hotkey) renderChord('long', status.long_hotkey);
}

function applyPunctSeg(settings) {
  for (const button of document.querySelectorAll('#punctSeg button')) {
    button.setAttribute('aria-pressed', String(button.dataset.value === settings.punct_choice));
  }
}

function applyHotkeys(settings) {
  document.getElementById('longHkRow').hidden = !settings.long_key;
  document.getElementById('longHkNote').hidden = !settings.long_key;
  renderChord('standard', settings.hotkey);
  renderChord('long', settings.long_hotkey);
  updateCaptureControls();
}

function applyLongKey(settings) {
  document.getElementById('longKeyRow').hidden = !settings.long_available;
  const control = document.getElementById('longKeySwitch');
  control.setAttribute('aria-checked', String(settings.long_key));
  control.disabled = longKeyPending || !settings.long_available || Boolean(capturing || capturePending || captureEnding);
  const state = settings.long_state;
  document.getElementById('longStateRow').hidden = !settings.long_key || !state;
  let text = '';
  switch (state && state.kind) {
    case 'installing': text = `ダウンロード中 ${state.percent}%`; break;
    case 'loading': text = '準備中…'; break;
    case 'ready': text = '準備完了'; break;
    case 'failed': text = '停止中'; break;
  }
  document.getElementById('longState').textContent = text;
  const retry = document.getElementById('retryLongEngineButton');
  retry.hidden = !settings.long_key || !state || state.kind !== 'failed';
  retry.disabled = retryLongPending || longKeyPending;
}

function applyBuiltinReplaceDict(settings) {
  const control = document.getElementById('builtinReplaceDictSwitch');
  control.setAttribute('aria-checked', String(settings.builtin_replace_dict));
  control.disabled = builtinReplaceDictPending;
}

function applyTranscriptLog(settings) {
  const control = document.getElementById('transcriptLogSwitch');
  control.setAttribute('aria-checked', String(settings.transcript_log));
  control.disabled = transcriptLogPending;
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

  row.append(time, text);
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
  applyHotkeys(snapshot.settings);
  applyLongKey(snapshot.settings);
  applyPunctSeg(snapshot.settings);
  applyBuiltinReplaceDict(snapshot.settings);
  applyTranscriptLog(snapshot.settings);
  applyAutostart(snapshot.autostart);
  applyHistory(snapshot.history);
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

const hotkeyControls = {
  standard: { button: document.getElementById('hkButton'), note: document.getElementById('hkNote'), cap: document.getElementById('hkCap') },
  long: { button: document.getElementById('longHkButton'), note: document.getElementById('longHkNote'), cap: document.getElementById('longHkCap') },
};

function updateCaptureControls() {
  for (const [key, control] of Object.entries(hotkeyControls)) {
    control.button.disabled = Boolean((capturePending && capturePending !== key) || captureEnding || (capturing && capturing !== key) || longKeyPending
      || (key === 'long' && currentSnapshot && !currentSnapshot.settings.long_available));
  }
  if (currentSnapshot) applyLongKey(currentSnapshot.settings);
}

function finishHotkeyCaptureUi() {
  if (!capturing) return;
  const control = hotkeyControls[capturing];
  capturing = null;
  control.button.classList.remove('recording');
  control.cap.textContent = '変更';
}

async function endHotkeyCapture(key, newChord) {
  captureEnding = true;
  finishHotkeyCaptureUi();
  updateCaptureControls();
  const control = hotkeyControls[key];
  try {
    const settings = await invoke('end_hotkey_capture', { key, newChord });
    cacheSettings(settings);
    applyHotkeys(settings);
    if (newChord) control.note.textContent = '保存しました。すぐに使えます。';
  } catch (error) {
    control.note.textContent = errorMessage(error);
    if (currentSnapshot) applyHotkeys(currentSnapshot.settings);
  } finally {
    captureEnding = false;
    updateCaptureControls();
  }
}

async function cancelHotkeyCapture(key) {
  if (capturePending === key) {
    captureCancelled = true;
    return;
  }
  if (capturing === key) await endHotkeyCapture(key, null);
}

for (const [key, control] of Object.entries(hotkeyControls)) {
  control.button.addEventListener('click', async () => {
    if (control.button.disabled || capturing || capturePending || captureEnding) return;
    capturePending = key;
    captureCancelled = false;
    updateCaptureControls();
    try {
      await invoke('begin_hotkey_capture', { key });
      capturing = key;
      capturePending = null;
      updateCaptureControls();
      if (captureCancelled) {
        await cancelHotkeyCapture(key);
      } else {
        control.button.classList.add('recording');
        control.cap.textContent = 'キーを押してください…';
        control.button.focus();
      }
    } catch (error) {
      control.note.textContent = errorMessage(error);
    } finally {
      capturePending = null;
      updateCaptureControls();
    }
  });

  control.button.addEventListener('keydown', async (event) => {
    if (capturing !== key) return;
    event.preventDefault();
    if (event.key === 'Escape') {
      await cancelHotkeyCapture(key);
      return;
    }
    if (MODIFIER_NAMES.includes(event.key)) return;
    const modifiers = MODIFIER_NAMES.filter((name) => event.getModifierState(name));
    if (modifiers.length === 0) {
      control.note.textContent = '修飾キーを 1 つ以上含めてください。';
      return;
    }
    const chordKey = event.code === 'Space' ? 'Space' : event.key.length === 1 ? event.key.toUpperCase() : event.key;
    const chord = [...modifiers.map((name) => MODIFIER_SYMBOLS[name]), chordKey].join('+');
    await endHotkeyCapture(key, chord);
  });

  control.button.addEventListener('blur', () => {
    void cancelHotkeyCapture(key);
  });
}

// Window focus can disappear before the begin command has replied.
window.addEventListener('blur', () => {
  const key = capturing || capturePending;
  if (key) void cancelHotkeyCapture(key);
});

document.getElementById('longKeySwitch').addEventListener('click', async (event) => {
  const control = event.currentTarget;
  if (control.disabled || longKeyPending) return;
  const enabled = control.getAttribute('aria-checked') !== 'true';
  longKeyPending = true;
  updateCaptureControls();
  try {
    const settings = await invoke('apply_settings', { patch: { longKey: enabled } });
    cacheSettings(settings);
    applyHotkeys(settings);
  } catch (error) {
    showInlineError(control, error);
  } finally {
    longKeyPending = false;
    updateCaptureControls();
  }
});

document.getElementById('retryLongEngineButton').addEventListener('click', async (event) => {
  const control = event.currentTarget;
  if (control.disabled || retryLongPending) return;
  retryLongPending = true;
  if (currentSnapshot) applyLongKey(currentSnapshot.settings);
  try {
    await invoke('retry_long_engine');
  } catch (error) {
    showInlineError(control, error);
  } finally {
    retryLongPending = false;
    if (currentSnapshot) applyLongKey(currentSnapshot.settings);
  }
});

for (const button of document.querySelectorAll('#punctSeg button')) {
  button.addEventListener('click', async () => {
    if (button.disabled) return;
    const buttons = [...document.querySelectorAll('#punctSeg button')];
    for (const control of buttons) control.disabled = true;
    try {
      const patch = { punctuation: button.dataset.value };
      const settings = await invoke('apply_settings', { patch });
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
  } catch (error) {
    showInlineError(control, error);
  } finally {
    builtinReplaceDictPending = false;
    if (currentSnapshot) applyBuiltinReplaceDict(currentSnapshot.settings);
  }
});

document.getElementById('transcriptLogSwitch').addEventListener('click', async (event) => {
  const control = event.currentTarget;
  if (control.disabled || transcriptLogPending) return;
  const enabled = control.getAttribute('aria-checked') !== 'true';
  transcriptLogPending = true;
  control.disabled = true;
  try {
    const settings = await invoke('apply_settings', { patch: { transcriptLog: enabled } });
    cacheSettings(settings);
  } catch (error) {
    showInlineError(control, error);
  } finally {
    transcriptLogPending = false;
    if (currentSnapshot) applyTranscriptLog(currentSnapshot.settings);
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

document.getElementById('openTranscriptDirButton').addEventListener('click', () => {
  invoke('open_transcript_dir').catch((error) => showInlineError(document.getElementById('openTranscriptDirButton'), error));
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

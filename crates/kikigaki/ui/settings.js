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

const { microphoneStepCopy, onboardingLayout, asrModelNote, asrModelFooter, statusDotClass, historyTimeLabel, visibleHistory, downloadLabel, chordFromKeyEvent, formatChord, statusKeysLine, HISTORY_PAGE } = window.KikigakiFormat;

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
let currentOnboarding = null;
// Rows the user has expanded to with さらに表示; kept across snapshots so a new utterance
// does not collapse the list back to the first page.
let historyShown = 0;
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
  document.getElementById('configErrorMessage').textContent = '設定ファイル（config.toml）を読み込めませんでした。「設定ファイルを開く」で該当行を直してから「再試行」を押してください。';
  document.getElementById('configErrorDetail').textContent = errorMessage(error);
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

function currentPlatform() {
  return currentSnapshot?.platform === 'windows' ? 'windows' : 'macos';
}

function renderChord(chord) {
  const container = document.getElementById('hkKbd');
  container.replaceChildren();
  const cap = document.createElement('kbd');
  cap.textContent = formatChord(chord, currentPlatform());
  container.appendChild(cap);
}

function applyStatus(status) {
  const dot = document.getElementById('statusDot');
  dot.className = 'dot';
  const dotClass = statusDotClass(status);
  if (dotClass) dot.classList.add(dotClass);
  const detail = document.getElementById('statusDetail');
  const detailText = typeof status.detail === 'string' ? status.detail : '';
  detail.hidden = detailText.length === 0;
  detail.querySelector('pre').textContent = detailText;
  document.getElementById('statusMain').textContent = statusMainText(status);
  document.getElementById('statusSub').textContent = statusKeysLine(currentSnapshot.settings, currentPlatform());
  renderChord(status.hotkey);
}

function applyAsrModel(settings) {
  for (const button of document.querySelectorAll('#asrModelSeg button')) {
    button.setAttribute('aria-pressed', String(button.dataset.value === settings.asr_model));
  }
  document.getElementById('asrModelNote').textContent = asrModelNote(settings.asr_model, settings.asr_model_applied);
  document.getElementById('asrModelFooter').textContent = asrModelFooter(settings.asr_model_applied);
}

function applyPunctSeg(settings) {
  for (const button of document.querySelectorAll('#punctSeg button')) {
    button.setAttribute('aria-pressed', String(button.dataset.value === settings.punct_choice));
  }
}

function applyHotkeys(settings) {
  document.getElementById('statusSub').textContent = statusKeysLine(settings, currentPlatform());
  renderChord(settings.hotkey);
  updateCaptureControls();
}

function applyBuiltinReplaceDict(settings) {
  const control = document.getElementById('builtinReplaceDictSwitch');
  if (control.getAttribute('aria-busy') !== 'true') control.setAttribute('aria-checked', String(settings.builtin_replace_dict));
}

function applyTranscriptLog(settings) {
  const control = document.getElementById('transcriptLogSwitch');
  if (control.getAttribute('aria-busy') !== 'true') control.setAttribute('aria-checked', String(settings.transcript_log));
}

// `sinceRevision` is the snapshot revision seen when the command was sent; a snapshot that
// arrived while it was in flight is newer than the command's reply, so the reply is dropped.
function cacheSettings(settings, sinceRevision = appliedRevision) {
  if (currentSnapshot && appliedRevision === sinceRevision) currentSnapshot = { ...currentSnapshot, settings };
}

function applyAutostart(autostart) {
  const control = document.getElementById('autostartSwitch');
  if (control.getAttribute('aria-busy') !== 'true') control.setAttribute('aria-checked', String(autostart.enabled));
  control.disabled = !autostart.available;
  control.title = autostart.available ? '' : '/Applications 以外にインストールされているため設定できません';
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
  time.textContent = historyTimeLabel(entry.at, new Date());
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
  const visible = visibleHistory(entries);
  list.replaceChildren();
  if (visible.length === 0) {
    historyShown = 0;
    list.appendChild(emptyMessage('最近の聞き取りはありません'));
    return;
  }
  let shown = 0;
  const more = document.createElement('button');
  more.className = 'btn small more';
  more.type = 'button';
  const renderUpTo = (target) => {
    more.remove();
    const end = Math.min(target, visible.length);
    for (; shown < end; shown++) list.appendChild(createHistoryRow(visible[shown]));
    historyShown = Math.max(shown, HISTORY_PAGE);
    if (shown < visible.length) {
      more.textContent = `さらに表示（残り ${visible.length - shown} 件）`;
      list.appendChild(more);
    }
  };
  more.addEventListener('click', () => renderUpTo(shown + HISTORY_PAGE));
  renderUpTo(Math.max(historyShown, HISTORY_PAGE));
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
  const label = downloadLabel(progress);
  if (label.percent === null) bar.removeAttribute('value');
  else bar.value = label.percent;
  detail.hidden = false;
  detail.textContent = label.text;
}

function showOnboarding(state) {
  currentOnboarding = state;
  showScene('scene-onboarding');
  const micDone = state.microphone === 'authorized';
  const micCopy = microphoneStepCopy(state.microphone, currentPlatform());
  const layout = onboardingLayout(state, currentPlatform());
  document.getElementById('stepAx').hidden = !layout.showAccessibility;
  document.getElementById('onboardingHeroStepCount').textContent = layout.stepCount;
  document.getElementById('onboardingSigningNote').hidden = currentPlatform() === 'windows';
  setStepState('stepMic', 'stepMicNumber', 1, micDone, !micDone);
  setStepState('stepAx', 'stepAxNumber', 2, state.accessibility_trusted, micDone && !state.accessibility_trusted);
  setStepState('stepModels', 'stepModelsNumber', layout.stepCount, state.models_installed, layout.modelsReady && !state.models_installed);

  const micButton = document.getElementById('stepMicButton');
  micButton.textContent = micCopy.button;
  micButton.disabled = micDone;
  micButton.hidden = micDone;
  document.getElementById('stepMicDetail').textContent = micCopy.detail;

  const axButton = document.getElementById('stepAxButton');
  axButton.disabled = state.accessibility_trusted;
  axButton.hidden = state.accessibility_trusted;
  document.getElementById('stepAxDetail').textContent = state.accessibility_trusted ? '許可済み' : '文字を貼り付けるために必要です。';

  const modelsButton = document.getElementById('stepModelsButton');
  modelsButton.hidden = state.models_installed;
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
    progress.textContent = 'ダウンロードに失敗しました。通信を確認して「再試行」を押してください。';
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

  if (layout.complete) showSettings();
}

function applySnapshot(snapshot) {
  currentSnapshot = snapshot;
  applyStatus(snapshot.status);
  applyHotkeys(snapshot.settings);
  applyAsrModel(snapshot.settings);
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

function noteAnchor(element) {
  return element.closest('.row') || element.closest('.item') || element;
}

function clearInlineNote(element) {
  const anchor = noteAnchor(element);
  const previous = inlineErrors.get(anchor);
  if (!previous) return;
  clearTimeout(previous.timer);
  previous.note.classList.add('leaving');
  setTimeout(() => previous.note.remove(), 100);
  inlineErrors.delete(anchor);
}

function showInlineError(element, error, isError = true, lifetime = 8000) {
  const anchor = noteAnchor(element);
  clearInlineNote(element);
  const note = document.createElement('p');
  note.className = 'inline-err';
  if (!isError) note.classList.add('ok');
  note.textContent = errorMessage(error);
  anchor.after(note);
  if (typeof note.scrollIntoView === 'function') note.scrollIntoView({ block: 'nearest' });
  const timer = setTimeout(() => clearInlineNote(element), lifetime);
  inlineErrors.set(anchor, { note, timer });
}

// A new action clears feedback belonging to that row before its handler runs.
document.addEventListener('click', (event) => {
  if (event.target.closest('button')) clearInlineNote(event.target);
}, true);

const hotkeyControls = {
  standard: { button: document.getElementById('hkButton'), note: document.getElementById('hkNote'), cap: document.getElementById('hkCap') },
};

function updateCaptureControls() {
  for (const [key, control] of Object.entries(hotkeyControls)) {
    control.button.disabled = Boolean((capturePending && capturePending !== key) || captureEnding || (capturing && capturing !== key));
  }
}

function finishHotkeyCaptureUi() {
  if (!capturing) return;
  const control = hotkeyControls[capturing];
  capturing = null;
  control.note.hidden = true;
  control.button.classList.remove('recording');
  control.cap.textContent = '変更';
}

async function endHotkeyCapture(key, newChord) {
  captureEnding = true;
  finishHotkeyCaptureUi();
  updateCaptureControls();
  const control = hotkeyControls[key];
  const sinceRevision = appliedRevision;
  try {
    const settings = await invoke('end_hotkey_capture', { key, newChord });
    cacheSettings(settings, sinceRevision);
    if (currentSnapshot) applyHotkeys(currentSnapshot.settings);
    if (newChord) showInlineError(control.button, '✓ 保存しました', false, 3000);
  } catch (error) {
    showInlineError(control.button, error);
    if (currentSnapshot) applyHotkeys(currentSnapshot.settings);
  } finally {
    captureEnding = false;
    updateCaptureControls();
    // The button was disabled while ending, which drops keyboard focus to the body.
    if (document.activeElement === document.body && !control.button.disabled) control.button.focus();
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
        control.cap.textContent = '入力中';
        control.note.textContent = '新しいキーを押してください（Esc で取り消し）';
        control.note.hidden = false;
        control.button.focus();
      }
    } catch (error) {
      showInlineError(control.button, error);
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
    if (MODIFIER_NAMES.includes(event.key) || event.repeat) return;
    const modifiers = MODIFIER_NAMES.filter((name) => event.getModifierState(name));
    if (modifiers.length === 0) {
      showInlineError(control.button, '修飾キーを 1 つ以上含めてください。');
      return;
    }
    const chord = chordFromKeyEvent({
      code: event.code, key: event.key, modifiers: modifiers.map((name) => MODIFIER_SYMBOLS[name]),
    });
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

function bindSwitch(control, patchKey, onSettings) {
  control.addEventListener('click', async () => {
    if (control.disabled || control.getAttribute('aria-busy') === 'true') return;
    const previous = control.getAttribute('aria-checked') === 'true';
    const enabled = !previous;
    control.setAttribute('aria-checked', String(enabled));
    control.setAttribute('aria-busy', 'true');
    const sinceRevision = appliedRevision;
    try {
      if (patchKey === 'autostart') {
        await invoke('set_launch_at_login', { enabled });
        if (currentSnapshot) currentSnapshot = { ...currentSnapshot,
          autostart: { ...currentSnapshot.autostart, enabled } };
      } else {
        const settings = await invoke('apply_settings', { patch: { [patchKey]: enabled } });
        cacheSettings(settings, sinceRevision);
      }
    } catch (error) {
      control.setAttribute('aria-checked', String(previous));
      showInlineError(control, error);
    } finally {
      control.removeAttribute('aria-busy');
      if (currentSnapshot) onSettings(currentSnapshot);
      updateCaptureControls();
    }
  });
}

bindSwitch(document.getElementById('builtinReplaceDictSwitch'), 'builtinReplaceDict', (snapshot) => applyBuiltinReplaceDict(snapshot.settings));
bindSwitch(document.getElementById('transcriptLogSwitch'), 'transcriptLog', (snapshot) => applyTranscriptLog(snapshot.settings));
bindSwitch(document.getElementById('autostartSwitch'), 'autostart', (snapshot) => applyAutostart(snapshot.autostart));

for (const button of document.querySelectorAll('#asrModelSeg button')) {
  button.addEventListener('click', async () => {
    if (button.disabled) return;
    const buttons = [...document.querySelectorAll('#asrModelSeg button')];
    for (const control of buttons) control.disabled = true;
    const sinceRevision = appliedRevision;
    try {
      const settings = await invoke('apply_settings', { patch: { asrModel: button.dataset.value } });
      cacheSettings(settings, sinceRevision);
      applyAsrModel(currentSnapshot ? currentSnapshot.settings : settings);
    } catch (error) {
      if (currentSnapshot) applyAsrModel(currentSnapshot.settings);
      showInlineError(document.getElementById('asrModelSeg'), error);
    } finally {
      for (const control of buttons) control.disabled = false;
    }
  });
}

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

// Recheck after returning from Windows Settings. Focus and visibility may fire together.
let micRecheckPending = false;
async function recheckWindowsMicrophone() {
  if (currentPlatform() !== 'windows' || document.visibilityState === 'hidden'
      || !currentOnboarding || currentOnboarding.microphone === 'authorized' || micRecheckPending) return;
  micRecheckPending = true;
  try {
    await invoke('request_microphone_access');
  } catch (error) {
    showInlineError(document.getElementById('stepMic'), error);
  } finally {
    micRecheckPending = false;
  }
}
window.addEventListener('focus', recheckWindowsMicrophone);
document.addEventListener('visibilitychange', recheckWindowsMicrophone);

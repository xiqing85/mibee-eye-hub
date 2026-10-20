// App bootstrap: auth state machine → capability discovery → view wiring.

import { api, onSessionExpired, refreshCapabilities } from './api.js';
import { store, setLang, hasCap } from './store.js';
import { $, toast, confirmDlg } from './ui.js';
import { t, applyLang } from './i18n.js';
import { icon, initIcons } from './icons.js';
import { initTheme } from './theme.js';
import { AuthState, detectAuthState, setAuthMode, initAuth, handleLogout } from './auth.js';
import { connectEvents, disconnectEvents, dispatchTestEvent } from './sse.js';
import { initLive, startLive, stopLive, refreshCameraSelect, renderDetections } from './live.js';
import { initAi, handleModelChanged } from './ai.js';
import { handleAlarmEvent, handleAlarmDescription, handleVoiceTranscript } from './alarm.js';
import { initZones, updateZonesVisibility, refreshZones, renderZonesOverlay } from './zones.js';
import { initChat, updateChatVisibility, handleChatReplyEvent, refreshChatHistory, handleConversationEvent } from './chat.js';
import { initWaveform, updateWaveformVisibility, handleAudioLevel } from './waveform.js';
import { initThink, handleAgentStep, handleTurnFinished, showTurnThinking } from './think.js';
import { initTools, refreshTools, startToolsPolling } from './tools.js';
import { initPtz, fetchPtz, updatePtzVisibility, handlePtzEvent } from './ptz.js';
import { initImaging, handleParamChanged } from './imaging.js';
import { refreshCameras, renderCameras, initCameras, stopCameras, announceRecording } from './cameras.js';
import { loadConfig, initSettings } from './settings.js';
import { checkApi, refreshStatus, initStatus, startStatusPolling } from './status.js';
import { initModelMetrics, startModelMetricsPolling } from './modelmetrics.js';
import { initTraces, startTracesPolling, refreshTraces, tracesCap } from './traces.js';
import { renderDevices, initDevices } from './devices.js';
import { renderRecords, renderSpeakers, renderMeetings, initRecords, updateRecordsVisibility, recordsSseHook } from './records.js';
import { initAway, updateAwayVisibility, refreshAwayStatus, renderAwayEvents, handleAwayEvent, handleAwayState } from './away.js';
import { renderModels, initModels, updateModelsVisibility, handleModelTask } from './models.js';

// Routable views. settings/status/devices are sub-views of the System
// destination; #/cameras from the pre-redesign layout lands on Live.
const VIEWS = ['preview', 'assistant', 'records', 'models', 'settings', 'status', 'devices'];
const SYS_VIEWS = ['settings', 'status', 'devices'];
const SYSTEM_TAB = 'system';

function currentView() {
  const name = (location.hash || '').replace(/^#\/?/, '');
  if (name === 'cameras') return 'preview'; // pre-redesign hash
  return VIEWS.includes(name) && viewExists(name) ? name : 'preview';
}

function viewExists(name) {
  return name === SYSTEM_TAB || !!$('view-' + name) || SYS_VIEWS.includes(name);
}

/// The nav tab that highlights for a routable view.
function primaryTab(name) {
  return SYS_VIEWS.includes(name) ? SYSTEM_TAB : name;
}

export function showView(name) {
  if (name === 'login') { teardownApp(); return; }
  if (!viewExists(name)) name = 'preview';
  document.querySelectorAll('.view').forEach((v) => v.classList.remove('active'));
  const container = SYS_VIEWS.includes(name) ? $('view-' + SYSTEM_TAB) : $('view-' + name);
  if (container) container.classList.add('active');
  if (SYS_VIEWS.includes(name)) {
    document.querySelectorAll('#system-subnav .subnav-btn').forEach((btn) => {
      btn.classList.toggle('active', btn.dataset.subview === name);
    });
    document.querySelectorAll('.sys-pane').forEach((pane) => {
      pane.classList.toggle('active', pane.id === 'sys-' + name);
    });
  }
  const primary = primaryTab(name);
  document.querySelectorAll('.nav-tab').forEach((tab) => {
    tab.classList.toggle('active', tab.dataset.view === primary);
  });
  $('app').classList.remove('hidden');
  store.view = name;
  // Keep the view across reloads (F5 lands back on the same view, not live).
  history.replaceState(null, '', '#' + name);

  if (name === 'preview') {
    startLive();
    // The camera tiles stream MJPEG while the Live destination is open —
    // stop them on the way out to free the browser's per-origin sockets.
    if (hasCap('multi_camera')) refreshCameras().then(renderCameras);
  } else {
    stopLive();
    stopCameras();
  }
  if (name === 'settings') loadConfig();
  if (name === 'models') renderModels();
  if (name === 'status') { checkApi(); refreshStatus(); }
  if (name === 'devices') renderDevices();
  if (name === 'records') { refreshAwayStatus().then(renderAwayEvents); renderRecords(); renderMeetings(); }
  if (name === 'assistant') {
    renderSpeakers();
    refreshChatHistory().then((turns) => {
      if (turns && turns.length) showTurnThinking(turns[0]);
    });
    refreshTools();
  }
}

function teardownApp() {
  stopLive();
  stopCameras();
  disconnectEvents();
  document.querySelectorAll('.view').forEach((v) => v.classList.remove('active'));
  $('view-login').classList.add('active');
  $('app').classList.add('hidden');
}

async function enterApp() {
  await refreshCapabilities();
  await refreshCameras();
  applyLang();
  updateZonesVisibility();
  updateAssistantVisibility();
  updateChatVisibility();
  updateWaveformVisibility();
  initNav();
  initLive();
  initPtz();
  initImaging();
  initCameras();
  initSettings();
  initStatus();
  initDevices();
  initRecords();
  updateRecordsVisibility();
  initAway();
  updateAwayVisibility();
  refreshAwayStatus();
  initModels();
  updateModelsVisibility();
  initAi();
  initZones();
  initChat();
  initWaveform();
  initModelMetrics();
  initTraces();
  initThink();
  initTools();
  refreshCameraSelect();
  if (store.ptzEnabled) fetchPtz();
  if (hasCap('zones')) refreshZones();

  connectEvents({
    ai_detection: (p) => { renderDetections(p.detections || []); renderZonesOverlay(); },
    ai_model_changed: handleModelChanged,
    model_task: handleModelTask,
    audio_level: handleAudioLevel,
    alarm_description: handleAlarmDescription,
    // Hearing records view refreshes itself when a record lands while open.
    alarm: (p) => { handleAlarmEvent(p); recordsSseHook('alarm', p); },
    voice_transcript: (p) => { handleVoiceTranscript(p); recordsSseHook('voice_transcript', p); },
    chat_reply: handleChatReplyEvent,
    conversation: (turn) => {
      handleConversationEvent(turn);
      handleTurnFinished(turn);
    },
    agent_step: handleAgentStep,
    away_event: handleAwayEvent,
    away_state: handleAwayState,
    zone_event: () => { /* zone events ride the alarm/toast path */ },
    param_changed: handleParamChanged,
    recording: (p) => {
      if (p) announceRecording(!!p.active, 'info');
    },
    ptz_status: handlePtzEvent,
    camera_added: () => { refreshCameras().then(() => { renderCameras(); refreshCameraSelect(); }); },
    camera_offlined: () => { refreshCameras().then(() => { renderCameras(); refreshCameraSelect(); }); },
    status: () => { /* status view polls on its own */ },
  });

  startStatusPolling();
  startModelMetricsPolling();
  startTracesPolling();
  startToolsPolling();
  if (tracesCap()) refreshTraces();
  // Restore the last view after a reload; default to live view.
  showView(currentView());
}

function initNav() {
  if (initNav._done) return;
  initNav._done = true;
  const guard = async (target) => {
    if (!target || target === 'login') return false;
    if (store.configDirty && target !== 'settings') {
      const ok = await confirmDlg({
        message: t('unsavedConfirm'),
        okText: t('confirm'),
        cancelText: t('cancel'),
      });
      if (!ok) return false;
      store.configDirty = false;
    }
    return true;
  };
  document.querySelectorAll('.nav-tab').forEach((btn) => {
    btn.addEventListener('click', async () => {
      const target = btn.dataset.view === SYSTEM_TAB
        ? (SYS_VIEWS.includes(store.view) ? store.view : 'settings')
        : btn.dataset.view;
      if (await guard(target)) showView(target);
    });
  });
  document.querySelectorAll('#system-subnav .subnav-btn').forEach((btn) => {
    btn.addEventListener('click', async () => {
      if (await guard(btn.dataset.subview)) showView(btn.dataset.subview);
    });
  });
}

/// The Assistant destination exists when any of its features does:
/// chat, face enrollment, or speaker voiceprints.
function updateAssistantVisibility() {
  const show = hasCap('chat') || hasCap('face') || hasCap('voice_speakers') || hasCap('conversations');
  document.querySelectorAll('.nav-tab[data-view="assistant"]').forEach((tab) => {
    tab.classList.toggle('hidden', !show);
  });
}

function initShell() {
  initIcons();
  applyLang();
  initTheme();

  document.querySelectorAll('.lang-btn').forEach((b) => {
    b.addEventListener('click', () => {
      setLang(store.lang === 'zh' ? 'en' : 'zh');
      applyLang();
    });
  });

  document.addEventListener('click', (e) => {
    const btn = e.target.closest('.password-toggle');
    if (!btn) return;
    const input = document.getElementById(btn.dataset.target);
    if (!input) return;
    const show = input.type === 'password';
    input.type = show ? 'text' : 'password';
    btn.innerHTML = icon(show ? 'eye-off' : 'eye', 18);
    btn.classList.toggle('revealed', show);
    btn.setAttribute('aria-label', show ? t('hidePassword') : t('showPassword'));
  });

  onSessionExpired(() => {
    // A 401 while signed in means the session died — say so. The probe
    // 401 before the app is ever entered is just the login flow; toasting
    // "错误" there is noise on every fresh visit.
    if (!$('app').classList.contains('hidden')) toast(t('error'), 'error');
    showView('login');
    setAuthMode(AuthState.LOGIN);
  });
}

// Surface any uncaught error on the page — invaluable on headless devices.
// Test seam (SPEC appendix A #46 assertions inject SSE samples): the
// waveform's capture-tag lanes are driven through the same dispatch the
// live connection uses.
window.sse = { dispatchTestEvent };

window.addEventListener('error', (e) => {
  const box = document.createElement('div');
  box.style.cssText = 'position:fixed;bottom:44px;left:12px;z-index:9999;max-width:80%;'
    + 'padding:8px 12px;border-radius:8px;background:rgba(255,80,80,.92);color:#fff;'
    + 'font:12px/1.5 monospace;white-space:pre-wrap;';
  box.textContent = 'JS error: ' + (e.message || e.type) + (e.filename ? ' @ ' + e.filename + ':' + e.lineno : '');
  document.body.appendChild(box);
});
window.addEventListener('unhandledrejection', (e) => {
  const box = document.createElement('div');
  box.style.cssText = 'position:fixed;bottom:44px;left:12px;z-index:9999;max-width:80%;'
    + 'padding:8px 12px;border-radius:8px;background:rgba(255,80,80,.92);color:#fff;'
    + 'font:12px/1.5 monospace;white-space:pre-wrap;';
  box.textContent = 'Unhandled rejection: ' + (e.reason && (e.reason.stack || e.reason.message) || e.reason);
  document.body.appendChild(box);
});

document.addEventListener('DOMContentLoaded', async () => {
  initShell();
  initAuth(enterApp);
  document.querySelectorAll('.logout-btn').forEach((b) => {
    b.addEventListener('click', () => {
      // Drop tile streams BEFORE the logout POST — each hidden MJPEG <img>
      // pins one of the browser's 6 per-origin connections, and enough of
      // them starve the POST into never landing (logout looks dead).
      stopCameras();
      handleLogout(async () => {
        setAuthMode(AuthState.LOGIN);
        showView('login');
      });
    });
  });

  const state = await detectAuthState();
  if (state === AuthState.SIGNED_IN) {
    await enterApp();
  } else {
    setAuthMode(state);
    $('view-login').classList.add('active');
  }
});

// Back/forward and manual hash edits switch views without a reload.
window.addEventListener('hashchange', () => {
  if (!$('app').classList.contains('hidden')) showView(currentView());
});

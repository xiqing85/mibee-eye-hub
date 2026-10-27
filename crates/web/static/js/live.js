// Live view: adaptive player + AI overlay + stream HUD.
//
// Transport fallback chain (SPEC §4.1, driven by capabilities):
//   MSE over chunked HTTP  →  MJPEG <img>  →  snapshot polling
// The MSE engine self-heals (stall timer, exponential backoff, active
// SourceBuffer pruning) and keeps the playhead pinned near live.

import { api } from './api.js';
import { store, cameraId, hasCap } from './store.js';
import { icon } from './icons.js';
import { $, toast } from './ui.js';
import { t } from './i18n.js';
import { beginDeviceRestart } from './restart.js';

const MAX_BACKOFF_MS = 8000;
const STALL_TIMEOUT_MS = 10000;
const MAX_BUFFER_SECS = 8;

let engine = null;       // active transport engine (mse | mjpeg | poll)
let quality = localStorage.getItem('mibee_quality') === 'sub' ? 'sub' : 'main';

/// MSE endpoint for the current quality (SPEC appendix A #20): the
/// low-bandwidth substream lives at stream.sub.mse.
function mseEndpoint() {
  return quality === 'sub' ? 'stream.sub.mse' : 'stream.mse';
}

function updateQualityButton() {
  const btn = $('btn-quality');
  if (!btn) return;
  const show = hasCap('substream') && hasCap('mse');
  btn.classList.toggle('hidden', !show);
  btn.textContent = quality === 'sub' ? 'SD' : 'HD';
  btn.setAttribute('aria-pressed', String(quality === 'sub'));
  const label = quality === 'sub' ? t('qualitySub') : t('qualityMain');
  btn.title = t('qualityBtn', { q: quality === 'sub' ? 'SD' : 'HD' });
  btn.setAttribute('aria-label', label);
}
let pollTimer = null;
let liveDotTimer = null;
let healthStamp = 0;

export function mseSupported() {
  if (!window.MediaSource || !MediaSource.isTypeSupported) return false;
  return MediaSource.isTypeSupported('video/mp4; codecs="avc1.42E01E"') ||
         MediaSource.isTypeSupported('video/mp4; codecs="avc1.4D401F"') ||
         MediaSource.isTypeSupported('video/mp4; codecs="avc1.640028"');
}

export function initLive() {
  $('btn-fullscreen').addEventListener('click', () => {
    const wrapper = document.querySelector('.stream-wrapper');
    if (document.fullscreenElement) document.exitFullscreen();
    else if (wrapper && wrapper.requestFullscreen) wrapper.requestFullscreen();
  });
  // Swap expand/compress glyphs with the fullscreen state.
  document.addEventListener('fullscreenchange', () => {
    const btn = $('btn-fullscreen');
    if (!btn) return;
    const fs = !!document.fullscreenElement;
    btn.querySelector('.icon-expand')?.classList.toggle('hidden', fs);
    btn.querySelector('.icon-compress')?.classList.toggle('hidden', !fs);
  });
  $('btn-snapshot').addEventListener('click', captureSnapshot);
  $('btn-hflip').addEventListener('click', () => {
    store.hflip = !store.hflip;
    localStorage.setItem('mibee_hflip', store.hflip ? '1' : '0');
    applyTransform();
  });
  $('btn-vflip').addEventListener('click', () => {
    store.vflip = !store.vflip;
    localStorage.setItem('mibee_vflip', store.vflip ? '1' : '0');
    applyTransform();
  });
  const rotateBtn = $('btn-rotate');
  if (rotateBtn) rotateBtn.addEventListener('click', rotateDeviceFromLive);
  const qualityBtn = $('btn-quality');
  if (qualityBtn) qualityBtn.addEventListener('click', () => {
    // Toggle main ↔ sub; rebuild the MSE connection (stop→start cycle —
    // a different SPS means a fresh decoder configuration).
    quality = quality === 'sub' ? 'main' : 'sub';
    localStorage.setItem('mibee_quality', quality);
    updateQualityButton();
    if (engine) { stopLive(); startLive(); }
  });
  $('stream-retry').addEventListener('click', startLive);
  const sel = $('live-camera-select');
  if (sel) sel.addEventListener('change', () => {
    store.currentCameraId = sel.value || null;
    startLive();
  });
  // Camera selector only exists for multi-camera devices.
  $('live-camera-field').classList.toggle('hidden', !hasCap('multi_camera'));

  // Live clock
  setInterval(() => {
    const clock = $('stream-clock');
    if (clock) clock.textContent = new Date().toLocaleTimeString('en-GB', { hour12: false });
  }, 1000);
  applyTransform();
}

/// Populate the camera <select> from store.cameras (multi-camera only).
/// Hidden entirely when the device exposes fewer than two cameras — a
/// selector with a single entry is noise, not a control.
export function refreshCameraSelect() {
  const field = $('live-camera-field');
  const sel = $('live-camera-select');
  if (!sel || !hasCap('multi_camera')) return;
  if (!store.cameras || store.cameras.length < 2) {
    if (field) field.classList.add('hidden');
    return;
  }
  if (field) field.classList.remove('hidden');
  const prev = cameraId();
  sel.innerHTML = '';
  for (const c of store.cameras) {
    const opt = document.createElement('option');
    opt.value = c.id;
    opt.textContent = c.name || c.id;
    if (c.id === prev) opt.selected = true;
    sel.appendChild(opt);
  }
  store.currentCameraId = sel.value || store.currentCameraId;
}

// ── Device-level rotation from the live toolbar (SPEC appendix A #19) ──────
// Unlike the display-only flip buttons this one bakes the rotation into
// the encoded stream for every viewer. Dialect split:
//   notebook (camera_management): per-camera config + stream stop→start;
//   Pi dialects: camera.rotation via /api/config — go (config_apply.auto)
//   SIGTERMs itself, rs needs the explicit §5.1 restart.

/// Current device/camera rotation in degrees (0 when unknown).
function currentDeviceRotation() {
  if (hasCap('camera_management')) {
    const cam = (store.cameras || []).find((c) => c.id === cameraId());
    return Number((cam && cam.config && cam.config.rotation)) || 0;
  }
  const camera = (store.config && store.config.camera) || {};
  return Number(camera.rotation) || 0;
}

/// Show/hide the live-page rotate button and reflect the current angle.
function updateRotateButton() {
  const btn = $('btn-rotate');
  if (!btn) return;
  const piDialect = !!(store.config && store.config.camera
    && 'rotation' in store.config.camera);
  const show = hasCap('camera_management') || piDialect;
  btn.classList.toggle('hidden', !show);
  const deg = currentDeviceRotation();
  btn.setAttribute('aria-pressed', String(deg !== 0));
  btn.title = t('rotateBtn') + ' · ' + deg + '°';
  btn.setAttribute('aria-label', btn.title);
}

async function rotateDeviceFromLive() {
  const next = (currentDeviceRotation() + 90) % 360;
  if (hasCap('camera_management')) {
    const cam = (store.cameras || []).find((c) => c.id === cameraId());
    if (!cam) { toast(t('fetchError'), 'error'); return; }
    const cfg = { ...(cam.config || {}), rotation: next };
    const r = await api.put(`/api/cameras/${cam.id}`, { config: cfg });
    if (!r.ok) { toast(r.message || t('fetchError'), 'error'); return; }
    // Rotation applies on stream (re)start — same cycle as the card
    // button, plus re-establish this live view. Drop the live engine
    // first (its MJPEG/MSE connections hog the browser's per-origin
    // slots and die with the camera anyway), then give V4L2 a moment to
    // actually release the device (USB cameras lag; an immediate start
    // can lose the race and leave the source dead while the status
    // still says running).
    stopLive();
    await api.post(`/api/cameras/${cam.id}/stop`);
    await new Promise((r) => setTimeout(r, 1500));
    await api.post(`/api/cameras/${cam.id}/start`);
    cam.config = cfg;
    updateRotateButton();
    await startLive();
    toast(t('rotateApplied', { deg: next }), 'success');
    return;
  }
  const r = await api.put('/api/config', { camera: { rotation: next } });
  if (!r.ok) { toast(r.message || r.error || t('fetchError'), 'error'); return; }
  if (r.data && r.data.applied === 'camera_restart') {
    // Go dialect, geometry-preserving angle (0↔180, 90↔270): the device
    // rebuilt the camera pipeline in place — the process (and this page's
    // session) is alive. Re-establish this live view so the MSE feed picks
    // up the fresh stream instead of waiting on a restart that isn't
    // coming (SPEC 附录A #19).
    stopLive();
    await new Promise((res) => setTimeout(res, 1500));
    await startLive();
    updateRotateButton();
    toast(t('rotateApplied', { deg: next }), 'success');
    return;
  }
  const apply = (store.caps && store.caps.config_apply) || {};
  if (apply.auto) {
    // Go dialect: the PUT already SIGTERMed the service — ride the shared
    // restart flow (toast + health poll + single reload).
    beginDeviceRestart();
    return;
  }
  if (hasCap('restart')) {
    // rs dialect: saving only persists — apply via the explicit §5.1
    // restart so the rotation takes effect immediately.
    try { await api.post('/api/system/restart'); } catch (_) { /* dies mid-exit */ }
    beginDeviceRestart();
    return;
  }
  updateRotateButton();
  toast(t('rotateApplied', { deg: next }), 'success');
}

export async function startLive() {
  stopLive();
  // Device-level rotation is baked into the stream (SPEC appendix A #19);
  // no CSS rotation here — the config doc is fetched once per session only
  // to state the live-page rotate button (Pi dialects expose
  // camera.rotation via /api/config; notebook keeps it per camera).
  if (!store.config) {
    const cfg = await api.get('/api/config').catch(() => null);
    if (cfg && cfg.ok) store.config = cfg.data;
  }
  // The rotate button reads per-camera rotation on multi-camera devices;
  // the cameras list may not be loaded yet when the live view is the
  // landing page.
  if (hasCap('camera_management') && !store.cameras) {
    const cams = await api.get('/api/cameras').catch(() => null);
    if (cams && cams.ok) store.cameras = cams.data;
  }
  updateRotateButton();
  updateQualityButton();
  applyTransform();
  $('stream-error').classList.add('hidden');
  // Camera-less devices (no camera configured, or a mic/talk form where
  // the form profile gates video capture off): an engine against
  // camera '0' would retry a 404 forever behind the loading overlay —
  // show the honest empty state instead.
  const camsKnownEmpty = hasCap('camera_management') && Array.isArray(store.cameras)
    && store.cameras.length === 0;
  if (!hasCap('camera_management') || camsKnownEmpty) {
    $('stream-loading').classList.add('hidden');
    const iconHost = $('stream-empty-icon');
    if (iconHost && !iconHost.innerHTML) iconHost.innerHTML = icon('video', 40);
    $('stream-empty').classList.remove('hidden');
    return;
  }
  $('stream-empty').classList.add('hidden');
  $('stream-loading').classList.remove('hidden');
  setLoadingLabel(false);
  $('mjpeg-fallback-badge').classList.add('hidden');

  const id = cameraId();
  if (hasCap('mse') && mseSupported()) {
    engine = startMse(id, () => {
      // MSE gave up — fall through the chain.
      if (hasCap('mjpeg')) engine = startMjpeg(id);
      else engine = startPolling(id);
    });
  } else if (hasCap('mjpeg')) {
    engine = startMjpeg(id);
  } else {
    engine = startPolling(id);
  }
}

export function stopLive() {
  if (engine) { engine.stop(); engine = null; }
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

// ─── MSE engine ────────────────────────────────────────────────────
// The engine separates the PLAYER (video element + MediaSource +
// SourceBuffer) from the CONNECTION (one chunked fetch). Transport
// hiccups — a server cutoff, a Wi-Fi blip, a half-open TCP — only kill
// the connection: it is transparently refetched and goes on appending to
// the SAME SourceBuffer. The server hands the fMP4 timeline from
// connection to connection (SPEC §4.1), so the decoder never notices; a
// small corner chip is the only visible trace. A full teardown (black
// loading overlay) is reserved for decoder-level failures: append
// errors, a frozen playhead with data available, or a SourceBuffer that
// wedged.
function startMse(cameraIdArg, onGiveUp) {
  const video = $('stream-video');
  const state = { dead: false, failures: 0 };
  let ms = null, sb = null, queue = [], abort = null, stall = null, pruning = false;
  let watchdog = null, lastCt = -1, stuckTicks = 0;
  let refetching = false;
  // fetchStream is declared inside onOpen (buildPlayer); the
  // transport-recovery backoff below runs in this scope, so it must
  // go through the slot onOpen fills — calling the name directly is
  // a ReferenceError and the seamless refetch never fired.
  let fetchStreamFn = null;

  function abortFetch() {
    if (abort) { try { abort.abort(); } catch (_) { /* already closed */ } abort = null; }
  }

  function clearTimers() {
    if (stall) { clearTimeout(stall); stall = null; }
    if (watchdog) { clearInterval(watchdog); watchdog = null; }
  }

  function destroyPlayer() {
    clearTimers();
    if (sb) { try { sb.onupdateend = null; sb.onerror = null; } catch (_) { /* detached */ } }
    if (ms && ms.readyState === 'open') { try { ms.endOfStream(); } catch (_) { /* ignore */ } }
    ms = null; sb = null; queue = []; pruning = false;
  }

  // stop() is final (page navigation, engine swap); internal recovery
  // paths never set the dead flag — checking it AFTER stop() made every
  // internal reconnect a suicide in an earlier iteration.
  function stop() {
    state.dead = true;
    abortFetch();
    destroyPlayer();
    hideReconnectChip();
  }

  // Transport-level recovery: drop the dead fetch, keep the player.
  function refetch() {
    if (state.dead || refetching) return;
    refetching = true;
    abortFetch();
    if (stall) { clearTimeout(stall); stall = null; }
    state.failures += 1;
    if (state.failures > 12) { onGiveUp(); return; }
    showReconnectChip();
    const backoff = Math.min(MAX_BACKOFF_MS, 250 * 2 ** Math.min(state.failures - 1, 4));
    setTimeout(() => {
      refetching = false;
      if (!state.dead && fetchStreamFn) fetchStreamFn();
    }, backoff);
  }

  // Player-level recovery: the decoder pipeline itself is wedged — tear
  // it down, show the full overlay, rebuild from scratch.
  function hardReconnect() {
    if (state.dead) return;
    abortFetch();
    destroyPlayer();
    state.failures += 1;
    if (state.failures > 12) { onGiveUp(); return; }
    const backoff = Math.min(MAX_BACKOFF_MS, 500 * 2 ** Math.min(state.failures - 1, 4));
    setLoadingLabel(true);
    $('stream-loading').classList.remove('hidden');
    setTimeout(() => { if (!state.dead) buildPlayer(); }, backoff);
  }

  function resetStall() {
    if (stall) clearTimeout(stall);
    // No network bytes for the budget → the fetch is dead (half-open TCP
    // never errors on its own). That is a transport problem: refetch
    // without touching the player.
    stall = setTimeout(() => refetch(), STALL_TIMEOUT_MS);
  }

  function startWatchdog() {
    // Watch the playhead itself: a stalled decoder or a SourceBuffer
    // stuck in `updating` keep bytes flowing while the picture freezes.
    if (watchdog) clearInterval(watchdog);
    lastCt = -1; stuckTicks = 0;
    watchdog = setInterval(() => {
      if (state.dead) return;
      // Background tabs pause rendering and throttle timers — a frozen
      // playhead there is the browser, not the stream. Skipping the
      // freeze detector while hidden prevents reconnect loops the user
      // only ever sees as a flash on returning to the tab.
      if (document.hidden) { lastCt = video.currentTime; stuckTicks = 0; return; }
      if (video.ended) { hardReconnect(); return; }
      if (video.currentTime === lastCt) {
        // Buffer gap ahead (a refetch skipped real time while the server
        // held the timeline): jump to the next buffered range instead of
        // freezing at the edge.
        const b = video.buffered;
        for (let i = 0; i < b.length; i++) {
          if (b.start(i) > video.currentTime && b.start(i) - video.currentTime <= 10) {
            video.currentTime = b.start(i) + 0.05;
            stuckTicks = 0;
            return;
          }
        }
        stuckTicks += 1;
        if (video.paused) video.play().catch(() => { /* retried next tick */ });
        if (stuckTicks >= 4) { stuckTicks = 0; hardReconnect(); }
      } else {
        stuckTicks = 0;
      }
      lastCt = video.currentTime;
    }, 2000);
  }

  function buildPlayer() {
    resetStall();
    startWatchdog();
    try { ms = new MediaSource(); } catch (_) { onGiveUp(); return; }
    video.src = URL.createObjectURL(ms);
    video.load(); // force the element to pick up the new src on reconnect
    ms.addEventListener('sourceopen', onOpen, { once: true });
  }

  function onOpen() {
    if (state.dead || !ms) return;
    // Init segment arrives first; codec string is parsed from its avcC box.
    sb = null;
    const initBuffer = [];
    let initDone = false;
    let sourceBuffer = null;

    async function pump() {
      if (state.dead) return;
      if (!sourceBuffer || sourceBuffer.updating) return;
      if (!ms || ms.readyState !== 'open') { hardReconnect(); return; }

      // Prune behind the playhead so the buffer never saturates.
      if (!pruning && video.buffered.length > 0) {
        const cur = video.currentTime;
        for (let i = 0; i < video.buffered.length; i++) {
          const s = video.buffered.start(i), e = video.buffered.end(i);
          if (cur >= s && cur <= e && cur - s > MAX_BUFFER_SECS) {
            const to = cur - MAX_BUFFER_SECS / 2;
            if (to > s) {
              pruning = true;
              try { sourceBuffer.remove(s, to); return; } catch (_) { pruning = false; }
            }
            break;
          }
        }
      }

      const chunk = queue.shift();
      if (!chunk) return;
      try {
        sourceBuffer.appendBuffer(chunk);
        bumpLiveDot();
        state.failures = 0;
        hideReconnectChip();
        if (initDone) {
          $('stream-loading').classList.add('hidden');
          updateHealth();
        }
        // Pin the playhead near live.
        if (video.buffered.length > 0) {
          const first = video.buffered.start(0);
          const end = video.buffered.end(video.buffered.length - 1);
          // The stream's media timestamps need not begin at 0 — if the
          // playhead sits before any buffered data it can never start.
          if (video.currentTime < first) video.currentTime = first + 0.05;
          const lag = end - video.currentTime;
          updateHealth(lag);
          if (lag > MAX_BUFFER_SECS) video.currentTime = end - 0.3;
          else if (lag > 0.5) video.playbackRate = Math.min(1.5, 1.0 + lag * 0.2);
          else video.playbackRate = 1.0;
        }
        // Muted autoplay is allowed, but some browsers still need a nudge.
        if (video.paused) video.play().catch(() => { /* stall timer will retry */ });
      } catch (e) {
        if (e.name === 'QuotaExceededError') {
          queue.unshift(chunk);
          if (sourceBuffer.buffered.length > 0 && !pruning) {
            pruning = true;
            const s0 = sourceBuffer.buffered.start(0);
            const to = sourceBuffer.buffered.end(sourceBuffer.buffered.length - 1) - 2;
            if (to > s0) { try { sourceBuffer.remove(s0, to); return; } catch (_) { pruning = false; } }
          }
        } else {
          hardReconnect();
        }
      }
    }

    async function fetchStream() {
      resetStall(); // covers the fetch setup + first-byte (IDR) wait too
      const controller = new AbortController();
      abort = controller;
      try {
        const resp = await fetch(`/api/cameras/${cameraIdArg}/${mseEndpoint()}`, {
          credentials: 'same-origin',
          signal: controller.signal,
        });
        if (!resp.ok || !resp.body) { refetch(); return; }
        const reader = resp.body.getReader();
        for (;;) {
          if (state.dead) return;
          const { done, value } = await reader.read();
          if (done) { refetch(); return; }
          if (!value || !value.length) continue;
          resetStall();
          if (!sourceBuffer) {
            initBuffer.push(value);
            // First chunk(s) = init segment; parse avcC for the codec string.
            const merged = concat(initBuffer);
            const codec = codecFromInit(merged) || 'video/mp4; codecs="avc1.42E01E"';
            try {
              sourceBuffer = ms.addSourceBuffer(codec);
            } catch (_) {
              try { sourceBuffer = ms.addSourceBuffer('video/mp4; codecs="avc1.4D401F"'); }
              catch (_) { onGiveUp(); return; }
            }
            sb = sourceBuffer;
            sourceBuffer.mode = 'segments';
            sourceBuffer.addEventListener('updateend', () => { pruning = false; pump(); });
            sourceBuffer.addEventListener('error', () => hardReconnect());
            queue.push(merged);
            initDone = true;
            pump();
          } else {
            // A refetched connection re-sends the init segment before its
            // first keyframe — appending it again mid-stream is the
            // spec-blessed way to refresh the decoder configuration.
            queue.push(value);
            pump();
          }
        }
      } catch (e) {
        if (state.dead || e.name === 'AbortError') return;
        refetch();
      }
    }

    fetchStreamFn = fetchStream;
    fetchStream();
  }

  buildPlayer();
  return { stop };
}

function concat(chunks) {
  let len = 0;
  for (const c of chunks) len += c.length;
  const out = new Uint8Array(len);
  let off = 0;
  for (const c of chunks) { out.set(c, off); off += c.length; }
  return out;
}

/// Derive the avc1.PPCCLL codec string from an fMP4 init segment's avcC box.
function codecFromInit(data) {
  for (let i = 0; i < data.length - 12; i++) {
    if (data[i + 4] === 0x61 && data[i + 5] === 0x76 && data[i + 6] === 0x63 && data[i + 7] === 0x43) {
      const hex = (v) => v.toString(16).padStart(2, '0');
      return 'video/mp4; codecs="avc1.' + hex(data[i + 9]) + hex(data[i + 10]) + hex(data[i + 11]) + '"';
    }
  }
  return null;
}

// ─── MJPEG fallback ────────────────────────────────────────────────
function startMjpeg(cameraIdArg) {
  const video = $('stream-video');
  const img = $('stream-img');
  video.classList.add('hidden');
  img.classList.add('hidden'); // hidden until the first frame decodes
  $('mjpeg-fallback-badge').classList.remove('hidden');
  $('stream-loading').classList.add('hidden');
  const load = () => { img.src = `/api/cameras/${cameraIdArg}/live?_=${Date.now()}`; };
  img.onload = () => { img.classList.remove('hidden'); hideReconnectChip(); };
  img.onerror = () => {
    // Keep the last decoded frame on screen — a blank flash on every
    // transport blip is worse than a briefly stale one.
    showReconnectChip();
    setTimeout(load, 2000);
  };
  load();
  bumpLiveDot();
  return {
    stop() {
      img.onload = img.onerror = null;
      img.src = '';
      img.classList.add('hidden');
      hideReconnectChip();
      video.classList.remove('hidden');
    },
  };
}

// ─── Snapshot polling (last resort, e.g. Go without MJPEG) ─────────
function startPolling(cameraIdArg) {
  const video = $('stream-video');
  const img = $('stream-img');
  video.classList.add('hidden');
  img.classList.add('hidden');
  const tick = () => { img.src = `/api/cameras/${cameraIdArg}/snapshot?_=${Date.now()}`; };
  img.onload = () => { img.classList.remove('hidden'); hideReconnectChip(); };
  img.onerror = () => showReconnectChip(); // keep the stale frame visible
  tick();
  pollTimer = setInterval(tick, 5000);
  return {
    stop() {
      if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
      img.onload = img.onerror = null;
      img.src = '';
      img.classList.add('hidden');
      hideReconnectChip();
      video.classList.remove('hidden');
    },
  };
}

// ─── HUD / helpers ─────────────────────────────────────────────────
// The reconnect chip is the low-key UI for TRANSPORT-level recovery (the
// full-screen loading overlay stays reserved for decoder-level rebuilds
// and the initial load): picture stays up, one small badge says why it
// briefly stalled.
function showReconnectChip() {
  const chip = $('stream-reconnecting');
  if (chip) chip.classList.remove('hidden');
}

function hideReconnectChip() {
  const chip = $('stream-reconnecting');
  if (chip) chip.classList.add('hidden');
}

function bumpLiveDot() {
  const dot = $('stream-live-dot');
  if (!dot) return;
  dot.classList.add('active');
  clearTimeout(liveDotTimer);
  liveDotTimer = setTimeout(() => dot.classList.remove('active'), 1500);
}

function updateHealth(lag) {
  const now = Date.now();
  if (now - healthStamp < 1000) return;
  healthStamp = now;
  const el = $('stream-health');
  const video = $('stream-video');
  if (!el || !video) return;
  const w = video.videoWidth || 0, h = video.videoHeight || 0;
  el.textContent = (w && h ? w + '\u00d7' + h : '--\u00d7--') +
    (lag !== undefined ? ' \u00b7 ' + lag.toFixed(1) + 's' : '');
  el.className = 'stream-health ' + (lag === undefined ? '' : lag < 0.5 ? 'health-good' : lag < 2.0 ? 'health-warn' : 'health-bad');
}

function setLoadingLabel(reconnecting) {
  const label = $('stream-loading-label');
  if (label) label.textContent = reconnecting ? t('reconnecting') : t('loading');
}

export function showStreamError(msgKey) {
  const label = $('stream-error-label');
  if (label) label.textContent = t(msgKey || 'streamError');
  $('stream-error').classList.remove('hidden');
  $('stream-loading').classList.add('hidden');
}

export function applyTransform() {
  const video = $('stream-video');
  if (!video) return;
  const parts = [];
  if (store.hflip) parts.push('scaleX(-1)');
  if (store.vflip) parts.push('scaleY(-1)');
  video.style.transform = parts.length ? parts.join(' ') : '';
  const overlay = $('detection-overlay');
  if (overlay) overlay.style.transform = video.style.transform;
  const hBtn = $('btn-hflip'), vBtn = $('btn-vflip');
  if (hBtn) hBtn.setAttribute('aria-pressed', String(store.hflip));
  if (vBtn) vBtn.setAttribute('aria-pressed', String(store.vflip));
}

function captureSnapshot() {
  const video = $('stream-video');
  const img = $('stream-img');
  const source = (!video.classList.contains('hidden') && video.videoWidth) ? video : null;
  if (!source && (img.classList.contains('hidden') || !img.naturalWidth)) {
    toast(t('snapshotFail'), 'error');
    return;
  }
  const w = source ? source.videoWidth : img.naturalWidth;
  const h = source ? source.videoHeight : img.naturalHeight;
  const canvas = document.createElement('canvas');
  canvas.width = w; canvas.height = h;
  const ctx = canvas.getContext('2d');
  ctx.translate(canvas.width / 2, canvas.height / 2);
  if (store.hflip) ctx.scale(-1, 1);
  if (store.vflip) ctx.scale(1, -1);
  ctx.drawImage(source || img, -w / 2, -h / 2);
  const a = document.createElement('a');
  a.download = 'mibee-' + Date.now() + '.jpg';
  try {
    a.href = canvas.toDataURL('image/jpeg', 0.9);
    a.click();
  } catch (_) {
    toast(t('snapshotFail'), 'error');
  }
}

// ─── AI detection overlay (extension: ai) ──────────────────────────
let detectionClearTimer = null;

export function renderDetections(dets) {
  const canvas = $('detection-overlay');
  const video = $('stream-video');
  const wrapper = document.querySelector('.stream-wrapper');
  if (!canvas || !video || !wrapper || !video.videoWidth) return;
  // When the MJPEG/polling fallback is active there is no <video> geometry —
  // map onto the <img> instead.
  const media = video.classList.contains('hidden') ? $('stream-img') : video;
  if (!media) return;

  const dpr = window.devicePixelRatio || 1;
  const w = wrapper.clientWidth, h = wrapper.clientHeight;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  const vw = media.videoWidth || media.naturalWidth || 0;
  const vh = media.videoHeight || media.naturalHeight || 0;
  if (!vw || !vh) return;
  const scale = Math.min(w / vw, h / vh);
  const ox = (w - vw * scale) / 2, oy = (h - vh * scale) / 2;

  const color = getComputedStyle(canvas).getPropertyValue('--detection-color').trim() || '#00c8a0';
  const labelBg = getComputedStyle(canvas).getPropertyValue('--detection-label-bg').trim() || 'rgba(0,0,0,.55)';
  const mono = getComputedStyle(document.documentElement).getPropertyValue('--font-mono').trim() || 'monospace';
  ctx.lineWidth = 2;
  ctx.strokeStyle = color;
  ctx.font = '11px ' + mono;

  for (const d of dets) {
    const b = d.bbox || [];
    if (b.length < 4) continue;
    const x = ox + b[0] * scale, y = oy + b[1] * scale;
    const bw = b[2] * scale, bh = b[3] * scale;
    ctx.strokeRect(x, y, bw, bh);
    const label = (d.label || '?') + ' ' + Math.round((d.confidence || 0) * 100) + '%';
    const tw = ctx.measureText(label).width;
    ctx.fillStyle = labelBg;
    ctx.fillRect(x, y - 16, tw + 8, 16);
    ctx.fillStyle = color;
    ctx.fillText(label, x + 4, y - 4);
  }

  clearTimeout(detectionClearTimer);
  detectionClearTimer = setTimeout(() => ctx.clearRect(0, 0, w, h), 2000);
}

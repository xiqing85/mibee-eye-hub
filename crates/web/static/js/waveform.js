// Voiceprint hero waveform (SPEC §6 `audio_level` + `agent_step` states +
// appendix A #46 capture context: passive/active + whose voice).
// A full-width scrolling oscilloscope in the Assistant hero card: the real
// mic level (≤10 Hz, perceptually scaled −45…−5 dBFS → 0…1) is the energy
// envelope; the display interpolates it to 60 fps and modulates it with
// per-layer oscillators so it reads as a live voiceprint rather than a
// VU meter. States: idle (breathing baseline), listening (real amplitude),
// thinking (a pulse traveling along a flat line), answering (synthetic
// syllabic modulation — and on voice devices the mic picks up the real
// TTS playback anyway).

import { hasCap } from './store.js';
import { $ } from './ui.js';
import { t } from './i18n.js';

const POINTS = 160;          // horizontal resolution of the trace
const LAYERS = [             // amplitude scale / alpha / oscillator speed
  { scale: 1.0, alpha: 0.55, speed: 1.0 },
  { scale: 0.62, alpha: 0.30, speed: -1.45 },
  { scale: 0.34, alpha: 0.16, speed: 2.1 },
];
const LEVEL_EASE = 0.18;     // per-frame easing toward the target level
const SPEAKING_LEVEL = 0.3;
const SPEAKING_HANG_MS = 900;
const DATA_STALE_MS = 6000;

let env = new Float32Array(POINTS);   // scrolled envelope (per-frame push)
let curLevel = 0;                     // eased toward last audio_level sample
let lastSampleAt = 0;
let speakingUntil = 0;
let state = 'idle';                   // idle | listening | thinking | answering
let captureTag = { mode: null, source: null };  // SPEC #46 labels
let thinkPhase = 0;                   // 0..1 position of the traveling pulse
let rafId = null;
let ctx = null;
let heroEl = null;
let dpr = 1;

function cssVar(name, fallback) {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export function initWaveform() {
  const canvas = $('agent-waveform');
  if (!canvas) return;
  ctx = canvas.getContext('2d');
  heroEl = canvas.closest('.agent-hero');
  dpr = Math.min(window.devicePixelRatio || 1, 2);
  resize();
  if (window.ResizeObserver && heroEl) {
    new ResizeObserver(resize).observe(heroEl);
  }
  updateWaveformVisibility();
  if (rafId === null) rafId = requestAnimationFrame(frame);
}

function resize() {
  if (!ctx || !heroEl) return;
  const cssW = Math.max(120, heroEl.clientWidth - 2); // card padding handled by CSS
  const cssH = 110;
  ctx.canvas.width = Math.round(cssW * dpr);
  ctx.canvas.height = Math.round(cssH * dpr);
  ctx.canvas.style.width = cssW + 'px';
  ctx.canvas.style.height = cssH + 'px';
}

export function updateWaveformVisibility() {
  const hero = $('agent-hero');
  // Real levels need `voice`; a chat-only device still shows the synthetic
  // thinking/answering states.
  if (hero) hero.classList.toggle('hidden', !(hasCap('voice') || hasCap('chat')));
}

/// One SSE audio_level sample: becomes the envelope target.
export function handleAudioLevel(p) {
  const level = Math.max(0, Math.min(1, Number(p && p.level) || 0));
  lastSampleAt = performance.now();
  // SPEC appendix A #46: capture context rides every sample (additive;
  // older devices omit both — the tag then stays hidden).
  captureTag.mode = p && (p.capture_mode === 'active' || p.capture_mode === 'passive') ? p.capture_mode : null;
  captureTag.source = p && typeof p.source === 'string' ? p.source : null;
  if (state === 'idle' || state === 'listening') state = 'listening';
  if (level >= SPEAKING_LEVEL) speakingUntil = lastSampleAt + SPEAKING_HANG_MS;
}

/// External state overrides (agent phases, chat round-trips). Callers own
/// the semantics; `listening` re-asserts itself on the next audio sample.
export function setWaveformState(next) {
  if (next === 'idle' || next === 'listening' || next === 'thinking' || next === 'answering') {
    state = next;
  }
}

function pushEnvelope(now) {
  // Effective amplitude for this frame by state.
  let target = 0;
  if (state === 'listening') {
    const stale = now - lastSampleAt > DATA_STALE_MS;
    target = stale ? 0 : 0.05;
  }
  curLevel += (target - curLevel) * LEVEL_EASE;
  env.copyWithin(0, 1);
  const last = env.length - 1;
  const t = now / 1000;
  if (state === 'thinking') {
    thinkPhase = (thinkPhase + 0.006) % 1;
    env[last] = 0.015; // flat line — the pulse is drawn on top per-x
  } else if (state === 'answering') {
    // Synthetic syllabic bursts so chat (text) answers also breathe; on
    // voice devices the real mic level overlays this naturally.
    const burst = Math.max(0, Math.sin(t * 5.2)) * (0.55 + 0.45 * Math.sin(t * 1.3));
    env[last] = 0.04 + burst * 0.5;
  } else if (state === 'listening') {
    // Perceptual display curve: the device's level is already dBFS-mapped
    // (−45…−5 → 0…1), but distant speech still lands ~0.1-0.3 — a linear
    // trace reads as "barely moving". A 0.6 gamma + floor keeps quiet
    // speech clearly visible without clipping loud input.
    env[last] = 0.06 + Math.pow(curLevel, 0.6) * 0.85;
  } else {
    // Idle: subtle breathing so the hero never looks dead.
    env[last] = 0.018 + 0.014 * (0.5 + 0.5 * Math.sin(t * 0.9));
  }
}

function frame() {
  rafId = null;
  if (!ctx) return;
  const now = performance.now();
  // Drop out of listening back to idle when the room goes quiet.
  if (state === 'listening' && now - lastSampleAt > SPEAKING_HANG_MS + 1500) state = 'idle';
  pushEnvelope(now);
  draw(now);
  updateStateBadge(now);
  rafId = requestAnimationFrame(frame);
}

function draw(now) {
  const canvas = ctx.canvas;
  const w = canvas.width;
  const h = canvas.height;
  const mid = h / 2;
  const t = now / 1000;
  ctx.clearRect(0, 0, w, h);

  const accent = cssVar('--accent', '#00c8a0');
  const speaking = now < speakingUntil || state === 'answering';
  // SPEC #46 visual semantics: active capture keeps the accent highlight;
  // passive ambience desaturates to the faint tone; the device's own
  // voice (self) renders in its dedicated channel.
  const isSelf = captureTag.source === 'self';
  let tint = state === 'answering' ? cssVar('--warning', '#f0b45c') : accent;
  if (isSelf) tint = cssVar('--wave-self', '#b48cff');
  else if (captureTag.mode === 'passive') tint = cssVar('--text-faint', '#5c6c80');

  for (let li = 0; li < LAYERS.length; li++) {
    const layer = LAYERS[li];
    ctx.beginPath();
    // Upper half left→right, mirrored lower half right→left — a closed
    // voiceprint band around the midline.
    for (let i = 0; i < POINTS; i++) {
      const x = (i / (POINTS - 1)) * w;
      const y = mid - amp(i, layer, t) * mid;
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    }
    for (let i = POINTS - 1; i >= 0; i--) {
      const x = (i / (POINTS - 1)) * w;
      ctx.lineTo(x, mid + amp(i, layer, t) * mid);
    }
    ctx.closePath();
    ctx.globalAlpha = layer.alpha;
    ctx.fillStyle = tint;
    ctx.fill();
    ctx.globalAlpha = Math.min(1, layer.alpha + 0.35);
    ctx.strokeStyle = tint;
    ctx.lineWidth = Math.max(1, 1.2 * dpr);
    ctx.stroke();
  }

  // Thinking: a bright pulse sweeping the flat line.
  if (state === 'thinking') {
    const px = thinkPhase * w;
    const grad = ctx.createLinearGradient(px - 60 * dpr, 0, px + 60 * dpr, 0);
    grad.addColorStop(0, 'transparent');
    grad.addColorStop(0.5, accent);
    grad.addColorStop(1, 'transparent');
    ctx.globalAlpha = 0.9;
    ctx.strokeStyle = grad;
    ctx.lineWidth = Math.max(2, 2.2 * dpr);
    ctx.beginPath();
    ctx.moveTo(px - 60 * dpr, mid);
    ctx.lineTo(px + 60 * dpr, mid);
    ctx.stroke();
    // Glow dot at the pulse head.
    ctx.globalAlpha = 1;
    ctx.fillStyle = accent;
    ctx.beginPath();
    ctx.arc(px, mid, 3 * dpr, 0, Math.PI * 2);
    ctx.fill();
  }
  ctx.globalAlpha = 1;

  // Faint always-on midline: a near-silent mic must still read as
  // "listening, quiet" rather than a dead canvas.
  ctx.globalAlpha = 0.18;
  ctx.strokeStyle = tint;
  ctx.lineWidth = 1 * dpr;
  ctx.beginPath();
  ctx.moveTo(0, mid);
  ctx.lineTo(w, mid);
  ctx.stroke();
  ctx.globalAlpha = 1;

  // Midline glows while speaking.
  if (speaking) {
    ctx.globalAlpha = 0.25;
    ctx.strokeStyle = tint;
    ctx.lineWidth = 1 * dpr;
    ctx.beginPath();
    ctx.moveTo(0, mid);
    ctx.lineTo(w, mid);
    ctx.stroke();
    ctx.globalAlpha = 1;
  }
}

function amp(i, layer, t) {
  const base = env[i];
  const osc = 0.5 + 0.5 * Math.sin(i * 0.55 + t * 6.5 * layer.speed);
  const fine = 0.35 + 0.65 * osc;
  return Math.min(1, base * layer.scale * fine + 0.006);
}

const STATE_LABEL_KEY = {
  idle: 'agentStateIdle', listening: 'agentStateListening',
  thinking: 'agentStateThinking', answering: 'agentStateAnswering',
};

/// The badge next to the canvas tracks the same state machine (labels are
/// applied by i18n keys so language switching keeps working).
function updateStateBadge(now) {
  const badgeEl = $('agent-state');
  if (!badgeEl) return;
  updateCaptureTag();
  let eff = state;
  if (state === 'listening' && now < speakingUntil) eff = 'answering';
  if (badgeEl.dataset.state !== eff) {
    badgeEl.dataset.state = eff;
    badgeEl.classList.remove('idle', 'listening', 'thinking', 'answering');
    badgeEl.classList.add(eff);
    const label = $('agent-state-label');
    if (label) {
      label.setAttribute('data-i18n', STATE_LABEL_KEY[eff]);
      label.textContent = t(STATE_LABEL_KEY[eff]);
    }
  }
}

const CAPTURE_KEY = {
  passive: 'capturePassive', active: 'captureActive', self: 'captureSelf', others: 'captureOthers',
};

/// The capture-context tag (SPEC #46): passive/active + whose voice.
/// `speaker:<name>` shows the enrolled name; anything unknown hides the
/// tag (old devices without the fields never see it).
function updateCaptureTag() {
  const el = $('agent-capture');
  const label = $('agent-capture-label');
  if (!el || !label) return;
  let text = null;
  let cls = 'agent-capture';
  if (captureTag.source === 'self') {
    text = t('captureSelf'); cls += ' self active';
  } else if (captureTag.mode === 'active') {
    if (captureTag.source && captureTag.source.startsWith('speaker:')) {
      text = captureTag.source.slice(8); cls += ' active';
    } else if (captureTag.source === 'others') {
      text = t('captureOthers'); cls += ' active';
    } else {
      text = t('captureActive'); cls += ' active';
    }
  } else if (captureTag.mode === 'passive') {
    text = t('capturePassive');
  }
  if (text === null) {
    el.classList.add('hidden');
    return;
  }
  el.className = cls;
  label.textContent = text;
}

/// Test hooks: reset state and expose the canvas for assertions.
export function resetWaveformForTests() {
  env = new Float32Array(POINTS);
  curLevel = 0;
  lastSampleAt = 0;
  speakingUntil = 0;
  state = 'idle';
  captureTag = { mode: null, source: null };
  thinkPhase = 0;
  const el = $('agent-capture');
  if (el) el.classList.add('hidden');
}

export function waveformStateForTests() {
  return state;
}

export function captureTagForTests() {
  return { ...captureTag };
}

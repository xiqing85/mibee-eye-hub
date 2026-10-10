// Away mode (SPEC §3.6, notebook dialect appendix A #44): arm/disarm +
// the event record list. While armed the device watches its own AI
// detection stream — person events are greeted and asked who they are
// over the speaker (known faces by name), and every anomaly lands here
// as a record with a snapshot, a VLM description (when the VLM is up)
// and the visitor's spoken answer. `away_event` SSE upserts rows by id;
// `away_state` keeps every tab's armed badge in sync.

import { api } from './api.js';
import { store } from './store.js';
import { $, el, confirmDlg, toast } from './ui.js';
import { t } from './i18n.js';

let armed = false;
let sinceMs = null;
let voiceCap = false;
let events = [];

/// The away surface exists when capabilities.away.available is true
/// (object capability — hasCap() alone would be fooled by {available:false}).
export function awayCapable() {
  return !!(store.caps && store.caps.away && store.caps.away.available);
}

export function updateAwayVisibility() {
  const card = $('away-card');
  if (card) card.classList.toggle('hidden', !awayCapable());
}

export function initAway() {
  updateAwayVisibility();
  const btn = $('away-toggle');
  if (btn) btn.addEventListener('click', toggleAway);
  const clear = $('away-clear');
  if (clear) clear.addEventListener('click', clearAway);
}

async function toggleAway() {
  const btn = $('away-toggle');
  if (btn) btn.disabled = true;
  const r = await api.post('/api/away', { active: !armed });
  if (btn) btn.disabled = false;
  if (!r.ok) {
    toast(r.message || t('awayArmFailed'), 'error');
    return;
  }
  applyState(r.data);
  toast(armed ? t('awayArmedToast') : t('awayDisarmedToast'), 'info');
  // Disarm summary (SPEC §3.6 additive): what happened while armed.
  if (!armed && r.data && r.data.summary) toast(r.data.summary, 'info');
}

export async function refreshAwayStatus() {
  if (!awayCapable()) return;
  const r = await api.get('/api/away');
  if (r.ok) applyState(r.data);
}

function applyState(data) {
  if (!data) return;
  armed = !!data.active;
  sinceMs = armed ? (data.since_ms || null) : null;
  voiceCap = !!data.voice;
  renderStatus();
}

function renderStatus() {
  const status = $('away-status');
  if (status) {
    status.classList.toggle('armed', armed);
    status.textContent = armed
      ? t('awayArmedSince', { time: sinceMs ? fmtTime(sinceMs) : '' })
      : t('awayDisarmed');
  }
  const btn = $('away-toggle');
  if (btn) {
    btn.textContent = armed ? t('awayDisarm') : t('awayArm');
    btn.classList.toggle('btn-danger-solid', armed);
  }
  const hint = $('away-voice-hint');
  if (hint) hint.textContent = armed && !voiceCap ? t('awayNoVoice') : '';
}

export async function renderAwayEvents() {
  const list = $('away-list');
  if (!list || !awayCapable()) return;
  const r = await api.get('/api/away/events?limit=200');
  if (!r.ok) return;
  events = r.data && r.data.events ? r.data.events : [];
  drawList();
}

function drawList() {
  const list = $('away-list');
  if (!list) return;
  list.innerHTML = '';
  if (!events.length) {
    list.appendChild(el('div', { className: 'record-empty', textContent: t('awayEmpty') }));
    return;
  }
  for (const ev of events) list.appendChild(row(ev));
}

function row(ev) {
  const isPerson = ev.kind === 'person';
  const children = [
    el('span', { className: 'record-time mono', textContent: fmtTime(ev.started_ms) }),
    el('span', {
      className: 'record-kind ' + (isPerson ? 'kind-voice' : 'kind-sound'),
      textContent: isPerson ? t('awayKindPerson') : t('awayKindActivity'),
    }),
  ];
  if (ev.face_name) {
    children.push(el('span', { className: 'record-kind kind-speaker', textContent: ev.face_name, title: t('awayFaceKnown') }));
  }
  children.push(el('span', {
    className: 'away-state away-state-' + ev.state,
    textContent: t('awayState_' + ev.state),
  }));
  if (ev.labels) children.push(el('span', { className: 'record-text', textContent: ev.labels }));
  if (ev.visitor_reply) {
    children.push(el('span', { className: 'away-reply', textContent: '💬 ' + ev.visitor_reply, title: t('awayReplyTitle') }));
  }
  if (ev.description) children.push(el('span', { className: 'away-desc', textContent: ev.description }));
  if (ev.snapshot) {
    const img = el('img', { className: 'away-thumb', alt: t('awaySnapshotAlt'), title: t('awaySnapshot') });
    img.src = '/api/away/events/' + ev.id + '/snapshot';
    children.push(img);
  }
  return el('div', { className: 'record-row away-row', dataset: { id: String(ev.id) } }, children);
}

/// SSE `away_event` (SPEC §6): created or state-migrated record —
/// upsert by id so in-flight rows (greeting → listening → answered)
/// update in place.
export function handleAwayEvent(ev) {
  if (!ev || !awayCapable() || store.view !== 'records') return;
  const i = events.findIndex((e) => e.id === ev.id);
  if (i >= 0) events[i] = ev;
  else events.unshift(ev);
  drawList();
}

/// SSE `away_state` (SPEC §6): any client armed/disarmed the mode.
export function handleAwayState(p) {
  if (!p) return;
  applyState(p);
}

async function clearAway() {
  const ok = await confirmDlg({
    message: t('awayClearConfirm'),
    okText: t('confirm'),
    cancelText: t('cancel'),
    danger: true,
  });
  if (!ok) return;
  const r = await api.del('/api/away/events');
  if (r.ok) {
    events = [];
    drawList();
    toast(t('awayCleared', { n: r.data ? r.data.removed : 0 }), 'info');
  } else {
    toast(r.message || t('error'), 'error');
  }
}

function fmtTime(ms) {
  const d = new Date(ms);
  const pad = (n) => String(n).padStart(2, '0');
  return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()) +
    ' ' + pad(d.getHours()) + ':' + pad(d.getMinutes()) + ':' + pad(d.getSeconds());
}

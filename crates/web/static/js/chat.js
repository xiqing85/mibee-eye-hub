// Assistant chat (SPEC appendix A #22, grounding #29, agent tools §3.5).
// WeChat-style bubbles with per-turn tool-call cards and a collapsible
// thinking drawer (SPEC §3.4 entries). When the device keeps conversation
// records (`conversations` capability) the history is restored from
// GET /api/conversations and live turns arrive via the SSE `conversation`
// event — the POST /api/chat response renders immediately and the matching
// record event is de-duplicated by user text.

import { api } from './api.js';
import { store, hasCap } from './store.js';
import { $, toast, setBtnLoading, confirmDlg } from './ui.js';
import { t } from './i18n.js';
import { setWaveformState } from './waveform.js';

const MAX_HISTORY = 20;   // turns kept for context (device truncates anyway)
const RESTORE_LIMIT = 50;
let history = [];         // [{role, content}] fed to the device
let vision = localStorage.getItem('mibee_chat_vision') === '1'; // explicit VLM Q&A
const seenTurnIds = new Set();   // SSE conversation turns already rendered
let lastHttpUserText = '';       // dedupe the record event of own sends

export function initChat() {
  renderChatHints();
  const send = $('chat-send');
  if (send) send.addEventListener('click', sendChat);
  const clear = $('chat-clear');
  if (clear) clear.addEventListener('click', clearChat);
  const eye = $('chat-vision');
  if (eye) eye.addEventListener('click', () => {
    vision = !vision;
    localStorage.setItem('mibee_chat_vision', vision ? '1' : '0');
    updateVisionButton();
  });
  const input = $('chat-input');
  if (input) {
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.isComposing) sendChat();
    });
  }
  updateVisionButton();
  updateChatVisibility();
}

export function updateChatVisibility() {
  if (!hasCap('chat')) {
    const card = $('chat-log');
    if (card) card.closest('.chat-card')?.classList.add('hidden');
  }
  updateVisionButton();
}

/// Restore the persisted dialogue (SPEC §3.4) as bubbles + API context.
/// Returns the turns (newest-first) for callers that also want the latest
/// one for the thinking panel.
export async function refreshChatHistory() {
  if (!hasCap('conversations')) return null;
  const r = await api.get('/api/conversations?limit=' + RESTORE_LIMIT);
  if (!r.ok || !r.data || !Array.isArray(r.data.conversations)) return null;
  const log = $('chat-log');
  if (!log) return r.data.conversations;
  log.querySelectorAll('.chat-row').forEach((n) => n.remove());
  history = [];
  for (const turn of r.data.conversations.slice().reverse()) {
    seenTurnIds.add(String(turn.id));
    appendTurn(turn);
  }
  renderChatHints();
  updateCount();
  log.scrollTop = log.scrollHeight;
  return r.data.conversations;
}

/// One conversation record → user bubble + (reply bubble | no-reply pill)
/// + tool cards + thinking drawer. Feeds the API history when a reply
/// exists.
function appendTurn(turn) {
  const log = $('chat-log');
  if (!log) return;
  log.querySelectorAll('.chat-hints').forEach((n) => n.remove());
  const thinking = Array.isArray(turn.thinking) ? turn.thinking : [];
  // Adapt SPEC §3.4 tool thinking entries to the tool-card call shape.
  const tools = thinking
    .filter((x) => x && x.source === 'tool')
    .map((x) => ({ name: x.model, ok: true, result: x.note, duration_ms: x.duration_ms }));
  appendBubble('user', turn.user_text || '', { origin: turn.origin });
  if (turn.reply_text) {
    appendBubble('assistant', turn.reply_text, {
      engine: turn.engine,
      thinking,
      tools,
      interrupted: !!turn.interrupted,
    });
    history.push({ role: 'user', content: turn.user_text || '' });
    history.push({ role: 'assistant', content: turn.reply_text });
  } else if (turn.interrupted) {
    appendInterrupted(turn, thinking);
  } else {
    appendNoReply(turn);
  }
  if (history.length > MAX_HISTORY) history = history.slice(-MAX_HISTORY);
  updateCount();
}

function appendNoReply(turn) {
  const log = $('chat-log');
  if (!log) return;
  const row = document.createElement('div');
  row.className = 'chat-row theirs';
  const wrap = document.createElement('div');
  wrap.className = 'chat-bubble-wrap';
  const pill = document.createElement('span');
  pill.className = 'state-pill off';
  pill.textContent = t('convNoReply');
  wrap.appendChild(pill);
  const thinking = Array.isArray(turn.thinking) ? turn.thinking : [];
  if (thinking.length) attachThinking(wrap, thinking);
  row.appendChild(avatar('theirs'));
  row.appendChild(wrap);
  log.appendChild(row);
  log.scrollTop = log.scrollHeight;
}

/// Interrupted before any sentence played (SPEC §3.4 `interrupted`,
/// reply_text null): show the interrupt badge + thinking drawer.
function appendInterrupted(turn, thinking) {
  const log = $('chat-log');
  if (!log) return;
  log.querySelectorAll('.chat-hints').forEach((n) => n.remove());
  const row = document.createElement('div');
  row.className = 'chat-row theirs';
  const wrap = document.createElement('div');
  wrap.className = 'chat-bubble-wrap';
  const badge = document.createElement('span');
  badge.className = 'state-pill off';
  badge.textContent = t('chatInterrupted');
  badge.title = t('chatInterruptedTitle');
  wrap.appendChild(badge);
  if (thinking.length) attachThinking(wrap, thinking);
  row.appendChild(avatar('theirs'));
  row.appendChild(wrap);
  log.appendChild(row);
  log.scrollTop = log.scrollHeight;
  updateCount();
}

function avatar(side) {
  const a = document.createElement('span');
  a.className = 'chat-avatar ' + side;
  a.textContent = side === 'mine' ? t('chatAvatarMe') : t('chatAvatarAi');
  return a;
}

function appendBubble(role, text, opts = {}) {
  const log = $('chat-log');
  if (!log) return;
  log.querySelectorAll('.chat-hints').forEach((n) => n.remove());
  const mine = role === 'user';
  const row = document.createElement('div');
  row.className = 'chat-row ' + (mine ? 'mine' : 'theirs');
  const wrap = document.createElement('div');
  wrap.className = 'chat-bubble-wrap';
  const bubble = document.createElement('div');
  bubble.className = 'chat-bubble ' + (mine ? 'mine' : 'theirs');
  bubble.textContent = text;
  if (mine && opts.origin === 'voice') {
    const ear = document.createElement('span');
    ear.className = 'chat-badge voice';
    ear.textContent = t('chatHeardBadge');
    bubble.appendChild(ear);
  }
  if (!mine && opts.grounded === 'vlm') {
    const badge = document.createElement('span');
    badge.className = 'chat-badge vlm';
    badge.textContent = t('chatGroundedVlm');
    badge.title = t('chatVisionTitle');
    bubble.appendChild(badge);
  } else if (!mine && opts.grounded === 'scene') {
    const badge = document.createElement('span');
    badge.className = 'chat-badge scene';
    badge.textContent = t('chatGroundedScene');
    badge.title = t('chatGroundedSceneTitle');
    bubble.appendChild(badge);
  }
  if (!mine && opts.interrupted) {
    const badge = document.createElement('span');
    badge.className = 'chat-badge vlm';
    badge.textContent = t('chatInterrupted');
    badge.title = t('chatInterruptedTitle');
    bubble.appendChild(badge);
  }
  if (!mine && opts.engine) {
    const pill = document.createElement('span');
    pill.className = 'chat-engine mono';
    pill.textContent = String(opts.engine);
    bubble.appendChild(pill);
  }
  wrap.appendChild(bubble);
  // Tool cards (SPEC §3.5): the executed tool sequence of this turn —
  // from the POST response `tool_calls` or the record's thinking entries.
  const tools = opts.tools || [];
  if (tools.length) {
    const strip = document.createElement('div');
    strip.className = 'chat-tools';
    for (const call of tools) strip.appendChild(toolCard(call));
    wrap.appendChild(strip);
  }
  if (!mine && opts.thinking && opts.thinking.length) {
    attachThinking(wrap, opts.thinking);
  }
  row.appendChild(avatar(mine ? 'mine' : 'theirs'));
  row.appendChild(wrap);
  log.appendChild(row);
  log.scrollTop = log.scrollHeight;
}

/// Collapsible thinking drawer (SPEC §3.4 entries) under an AI bubble.
function attachThinking(wrap, thinking) {
  const toggle = document.createElement('button');
  toggle.type = 'button';
  toggle.className = 'chat-think-toggle';
  toggle.textContent = t('convThinking');
  const body = document.createElement('div');
  body.className = 'chat-think hidden';
  for (const entry of thinking) {
    const row = document.createElement('div');
    row.className = 'chat-think-entry';
    const src = document.createElement('span');
    src.className = 'chat-think-src mono';
    src.textContent = entry.source === 'tool'
      ? String(entry.model || 'tool')
      : String(entry.source || '?');
    if (entry.source === 'tool') src.classList.add('tool');
    row.appendChild(src);
    if (entry.note) {
      const note = document.createElement('span');
      note.className = 'chat-think-note';
      note.textContent = entry.note;
      row.appendChild(note);
    }
    if (entry.duration_ms !== undefined && entry.duration_ms !== null) {
      const dur = document.createElement('span');
      dur.className = 'chat-think-dur mono';
      dur.textContent = (entry.duration_ms / 1000).toFixed(1) + 's';
      row.appendChild(dur);
    }
    body.appendChild(row);
  }
  toggle.addEventListener('click', () => {
    const open = body.classList.toggle('hidden');
    toggle.textContent = open ? t('convThinking') : t('convHideThinking');
  });
  wrap.appendChild(toggle);
  wrap.appendChild(body);
}

/// One tool-call card: name, args, state, result, duration (SPEC §3.5).
export function toolCard(call) {
  const div = document.createElement('div');
  div.className = 'chat-tool';
  if (call.state) div.classList.add(call.state === 'running' ? 'running' : call.state);
  const head = document.createElement('div');
  head.className = 'chat-tool-head';
  const name = document.createElement('span');
  name.className = 'chat-tool-name mono';
  name.textContent = String(call.name || '?');
  head.appendChild(name);
  const state = document.createElement('span');
  state.className = 'state-pill ' + (call.ok === false || call.state === 'error' ? 'off' : 'on');
  state.textContent = call.state === 'running' ? t('toolRunning')
    : (call.ok === false || call.state === 'error') ? t('toolError') : t('toolDone');
  head.appendChild(state);
  if (call.duration_ms !== undefined && call.duration_ms !== null) {
    const dur = document.createElement('span');
    dur.className = 'chat-tool-dur mono';
    dur.textContent = (Number(call.duration_ms) / 1000).toFixed(1) + 's';
    head.appendChild(dur);
  }
  div.appendChild(head);
  if (call.args && Object.keys(call.args).length) {
    const args = document.createElement('div');
    args.className = 'chat-tool-args mono';
    args.textContent = JSON.stringify(call.args);
    div.appendChild(args);
  }
  if (call.result) {
    const res = document.createElement('div');
    res.className = 'chat-tool-result';
    res.textContent = String(call.result);
    div.appendChild(res);
  }
  return div;
}

/// Empty-state hint chips: teach what the assistant can actually answer.
function renderChatHints() {
  const log = $('chat-log');
  if (!log || log.querySelector('.chat-row')) return;
  let row = log.querySelector('.chat-hints');
  if (row) row.remove();
  row = document.createElement('div');
  row.className = 'chat-hints';
  for (const key of ['chatHint1', 'chatHint2', 'chatHint3']) {
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'chat-hint';
    chip.setAttribute('data-i18n', key);
    chip.textContent = t(key);
    chip.addEventListener('click', () => {
      const input = $('chat-input');
      if (!input) return;
      input.value = chip.textContent;
      sendChat();
    });
    row.appendChild(chip);
  }
  log.appendChild(row);
}

function updateVisionButton() {
  const eye = $('chat-vision');
  if (!eye) return;
  const show = hasCap('chat') && hasCap('vlm');
  eye.classList.toggle('hidden', !show);
  eye.classList.toggle('on', vision);
  eye.setAttribute('aria-pressed', String(vision));
}

function updateCount() {
  const bar = $('chat-toolbar');
  const cnt = $('chat-count');
  if (!bar || !cnt) return;
  const turns = $('chat-log') ? $('chat-log').querySelectorAll('.chat-row').length : 0;
  bar.classList.toggle('hidden', !hasCap('conversations'));
  cnt.textContent = turns ? t('chatTurns', { n: Math.ceil(turns / 2) }) : '';
}

async function clearChat() {
  const okBtn = await confirmDlg({
    message: t('convClearConfirm'),
    okText: t('confirm'),
    cancelText: t('cancel'),
    danger: true,
  });
  if (!okBtn) return;
  await api.del('/api/conversations');
  seenTurnIds.clear();
  history = [];
  const log = $('chat-log');
  if (log) log.querySelectorAll('.chat-row').forEach((n) => n.remove());
  renderChatHints();
  updateCount();
  // Let the thinking panel follow suit (kept decoupled via a DOM event —
  // think.js imports toolCard from here, a direct import would cycle).
  window.dispatchEvent(new CustomEvent('mibee:conv-cleared'));
  toast(t('convCleared'), 'info');
}

async function sendChat() {
  const input = $('chat-input');
  const btn = $('chat-send');
  if (!input) return;
  const text = input.value.trim();
  if (!text) return;
  input.value = '';
  appendBubble('user', text);
  history.push({ role: 'user', content: text });
  lastHttpUserText = text;
  setBtnLoading(btn, true);
  setWaveformState('thinking');
  const useVision = vision && hasCap('vlm');
  if (useVision) toast(t('chatVisionSlow'), 'info');
  try {
    const res = await api.post('/api/chat', {
      text,
      history: history.slice(0, -1),
      vision: useVision,
    });
    setWaveformState('answering');
    const reply = res.ok && res.data && res.data.reply ? res.data.reply : null;
    if (reply === null) {
      toast(t('chatFailed'), 'error');
      appendBubble('assistant', t('chatFailed'));
    } else {
      const calls = Array.isArray(res.data.tool_calls) ? res.data.tool_calls : [];
      appendBubble('assistant', reply, {
        grounded: res.data.grounded,
        engine: res.data.engine,
        tools: calls,
      });
      history.push({ role: 'assistant', content: reply });
    }
  } finally {
    setBtnLoading(btn, false);
    setTimeout(() => setWaveformState('idle'), 2500);
    if (history.length > MAX_HISTORY) history = history.slice(-MAX_HISTORY);
    const log = $('chat-log');
    if (log) log.scrollTop = log.scrollHeight;
    updateCount();
  }
}

/// SSE `conversation` (SPEC §3.4) — the canonical appender for voice
/// turns and late-arriving records; own HTTP sends de-duplicate by user
/// text (the POST response already rendered them).
export function handleConversationEvent(turn) {
  if (!turn || !turn.user_text) return;
  if (seenTurnIds.has(String(turn.id))) return;
  seenTurnIds.add(String(turn.id));
  if (turn.origin === 'http' && turn.user_text === lastHttpUserText) return;
  if (store.view === 'assistant') {
    appendTurn(turn);
    const log = $('chat-log');
    if (log) log.scrollTop = log.scrollHeight;
  } else if (turn.reply_text) {
    toast(t('voiceReply', { s: turn.reply_text }), 'info');
  }
}

/// SSE `chat_reply` (voice replies on devices without conversation
/// records, SPEC appendix A #22/#29) — mirror into the chat when the
/// view is open, toast otherwise.
export function handleChatReplyEvent(p) {
  if (!p || !p.reply) return;
  if (p.source !== 'voice') return;
  if (hasCap('conversations')) return; // the record event renders it fully
  if (store.view === 'assistant') {
    history.push({ role: 'assistant', content: p.reply });
    appendBubble('assistant', p.reply, p.grounded);
  } else {
    toast(t('voiceReply', { s: p.reply }), 'info');
  }
}

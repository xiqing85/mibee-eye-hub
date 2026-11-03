// Thermal card (extension: thermal, SPEC §3.7) — the primary surface on a
// thermal camera: pseudo-color image + absolute readings, live via the
// `thermal.temperature` SSE event with a polling fallback.

import { api } from './api.js';
import { hasCap } from './store.js';
import { $ } from './ui.js';
import { t } from './i18n.js';

let pollTimer = null;
let lastTs = 0;

export function updateThermalVisibility() {
  const show = hasCap('thermal');
  $('thermal-card').classList.toggle('hidden', !show);
  if (show && !pollTimer) startPolling();
  if (!show && pollTimer) stopPolling();
}

function startPolling() {
  // Fallback cadence when SSE is not connected (SPEC §6 advertises
  // thermal.temperature; slow sensors make 5s polls plenty).
  pollTimer = setInterval(refreshThermal, 5000);
  refreshThermal();
}

function stopPolling() {
  clearInterval(pollTimer);
  pollTimer = null;
}

export async function refreshThermal() {
  const r = await api.get('/api/thermal/current');
  // Family envelope (`{ok,data}`) on enveloped devices; the thermal bring-up
  // dialect serves the reading bare (NVR adapter's live shape) — accept both.
  const d = r.ok ? r.data : (r.data && r.data.max ? r.data : null);
  if (d) renderThermal(d);
}

/// Render one reading (shared by poll + SSE paths). Refreshes the image
/// only when a new frame landed (timestamp_ms changes) — sensors can be
/// as slow as 0.03 fps and re-requesting between frames is waste.
export function renderThermal(j) {
  if (!j) return;
  const ta = $('thermal-ta'), mx = $('thermal-max'), mn = $('thermal-min');
  const mpos = $('thermal-max-pos'), npos = $('thermal-min-pos');
  const fps = $('thermal-fps'), badge = $('thermal-trusted-badge');
  // Untrusted frames carry a placeholder ta — show the em dash until the
  // compensation chain is live (SPEC §3.7 trusted gate).
  const taOk = j.trusted !== false && j.ta != null;
  ta.textContent = taOk ? j.ta.toFixed(1) + ' °C' : '—';
  mx.textContent = j.max ? j.max.value_c.toFixed(1) + ' °C' : '—';
  mn.textContent = j.min ? j.min.value_c.toFixed(1) + ' °C' : '—';
  mpos.textContent = j.max ? `(${j.max.x + 1},${j.max.y + 1})` : '';
  npos.textContent = j.min ? `(${j.min.x + 1},${j.min.y + 1})` : '';
  fps.textContent = j.sensor_fps != null ? j.sensor_fps.toFixed(2) + ' fps' : '—';
  badge.classList.toggle('hidden', !j.trusted);
  badge.textContent = t('thermalTrusted');
  if (j.timestamp_ms && j.timestamp_ms !== lastTs) {
    lastTs = j.timestamp_ms;
    $('thermal-image').src = '/api/thermal/snapshot?format=bmp&t=' + j.timestamp_ms;
  }
}

/// SSE `thermal.temperature` hook (SPEC §6) — same payload minus the grid.
export function handleThermalEvent(payload) {
  renderThermal(payload);
}

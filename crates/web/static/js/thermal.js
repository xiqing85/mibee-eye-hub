// Thermal card (extension: thermal, SPEC §3.7) — the primary surface on a
// thermal camera: pseudo-color image + absolute readings, live via the
// `thermal.temperature` SSE event with a polling fallback. Hovering the
// image reads the point temperature from the 32x24 grid (SPEC coordinate
// system: x=col[0..w-1], y=row[0..h-1], top-left origin, row-major).

import { api } from './api.js';
import { hasCap } from './store.js';
import { $ } from './ui.js';
import { t } from './i18n.js';

let pollTimer = null;
let lastTs = 0;

// Hover state: full grid + geometry of the rendered image.
let grid = null, GW = 32, GH = 24;
let canvasEl = null, tipEl = null;

export function updateThermalVisibility() {
  const show = hasCap('thermal');
  $('thermal-card').classList.toggle('hidden', !show);
  if (show && !pollTimer) startPolling();
  if (!show && pollTimer) stopPolling();
}

function startPolling() {
  // Fallback cadence when SSE is not connected. Grid included: the hover
  // readout needs the full matrix (a few KB; poll <=1 Hz per SPEC).
  pollTimer = setInterval(refreshThermal, 5000);
  refreshThermal();
  initHover();
}

function stopPolling() {
  clearInterval(pollTimer);
  pollTimer = null;
}

export async function refreshThermal() {
  const r = await api.get('/api/thermal/current?grid=1');
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
  if (j.grid && j.grid.values) {
    grid = j.grid.values;
    GW = j.grid.w || 32;
    GH = j.grid.h || 24;
  }
  if (j.timestamp_ms && j.timestamp_ms !== lastTs) {
    lastTs = j.timestamp_ms;
    $('thermal-image').src = '/api/thermal/snapshot?format=bmp&t=' + j.timestamp_ms;
  }
}

/// SSE `thermal.temperature` hook (SPEC §6) — same payload minus the grid.
export function handleThermalEvent(payload) {
  renderThermal(payload);
}

/// Point readout: hover maps the cursor onto the sensor grid, draws a cell
/// highlight bracket on the canvas and floats a tooltip with the value.
function initHover() {
  if (canvasEl) return; // already wired
  canvasEl = $('thermal-canvas');
  if (!canvasEl) return;
  tipEl = document.createElement('div');
  tipEl.id = 'thermal-tip';
  tipEl.setAttribute('role', 'tooltip');
  document.body.appendChild(tipEl);

  const drawHighlight = (gx, gy) => {
    const r = canvasEl.getBoundingClientRect();
    canvasEl.width = Math.round(r.width);
    canvasEl.height = Math.round(r.height);
    const ctx = canvasEl.getContext('2d');
    ctx.clearRect(0, 0, canvasEl.width, canvasEl.height);
    if (gx == null) return;
    const cw = canvasEl.width / GW, chh = canvasEl.height / GH;
    const x0 = gx * cw + 1, y0 = gy * chh + 1;
    const wd = Math.max(cw - 2, 3), hd = Math.max(chh - 2, 3);
    // Thin white bracket corners — matches the baked-in marker language
    // without covering the image.
    ctx.strokeStyle = 'rgba(255,255,255,0.95)';
    ctx.lineWidth = 1.5;
    const arm = Math.min(wd, hd) * 0.3;
    ctx.beginPath();
    ctx.moveTo(x0, y0 + arm); ctx.lineTo(x0, y0); ctx.lineTo(x0 + arm, y0);
    ctx.moveTo(x0 + wd - arm, y0); ctx.lineTo(x0 + wd, y0); ctx.lineTo(x0 + wd, y0 + arm);
    ctx.moveTo(x0 + wd, y0 + hd - arm); ctx.lineTo(x0 + wd, y0 + hd); ctx.lineTo(x0 + wd - arm, y0 + hd);
    ctx.moveTo(x0 + arm, y0 + hd); ctx.lineTo(x0, y0 + hd); ctx.lineTo(x0, y0 + hd - arm);
    ctx.stroke();
  };

  canvasEl.addEventListener('mousemove', (e) => {
    if (!grid) { tipEl.style.display = 'none'; return; }
    const r = canvasEl.getBoundingClientRect();
    const gx = Math.max(0, Math.min(GW - 1, Math.floor((e.clientX - r.left) / r.width * GW)));
    const gy = Math.max(0, Math.min(GH - 1, Math.floor((e.clientY - r.top) / r.height * GH)));
    const v = grid[gy * GW + gx];
    drawHighlight(gx, gy);
    tipEl.style.display = 'block';
    tipEl.style.left = Math.min(e.clientX + 14, window.innerWidth - 130) + 'px';
    tipEl.style.top = Math.max(e.clientY - 32, 6) + 'px';
    tipEl.textContent = `${v.toFixed(1)} °C · ${t('thermalCol')} ${gx + 1}, ${t('thermalRow')} ${gy + 1}`;
  });
  canvasEl.addEventListener('mouseleave', () => {
    tipEl.style.display = 'none';
    drawHighlight(null);
  });
}

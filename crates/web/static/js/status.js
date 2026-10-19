// Status view: /api/status poll + camera info + connection badges +
// live resource monitoring & observability panes (SPEC §3.2).

import { api } from './api.js';
import { store, hasCap } from './store.js';
import { $, esc } from './ui.js';
import { t } from './i18n.js';
import { initChart, pushChart, pushChartTwo, fmtBytes, fmtRate, fmtPercent } from './charts.js';

let pollTimer = null;
let metricsTimer = null;
let logsTimer = null;
let prevTraffic = null; // previous process.traffic for client-side rates

export async function checkApi() {
  const r = await api.get('/api/health');
  const b = $('api-badge'), l = $('api-label');
  if (b) b.className = 'status-badge ' + (r.ok ? 'online' : 'offline');
  if (l) l.textContent = t(r.ok ? 'connected' : 'disconnected');
}

function obsCaps() {
  const obs = (store.caps && store.caps.observability) || {};
  return {
    metrics: !!obs.metrics,
    logs: !!obs.logs,
    requests: !!obs.requests,
  };
}

export async function refreshStatus() {
  const r = await api.get('/api/status');
  if (!r.ok) return;
  const s = r.data || {};
  const info = $('device-info');
  if (info) {
    const pill = (on) => '<span class="state-pill ' + (on ? 'on' : 'off') + '">' +
      (on ? t('stateOn') : t('stateOff')) + '</span>';
    const rows = [
      [t('deviceName'), s.device_name],
      [t('model'), s.model],
      [t('vendor'), s.vendor],
      [t('firmware'), s.firmware],
      [t('uptime'), fmtUptime(s.uptime)],
    ];
    const extras = [];
    if (s.recording !== undefined) extras.push([t('recordingService'), pill(s.recording)]);
    if (s.gb28181 !== undefined) extras.push([t('gb28181Service'), pill(s.gb28181)]);
    info.innerHTML = rows.concat(extras).map(([label, val]) =>
      '<div class="reading"><span class="reading-label">' + esc(label) + '</span><span class="reading-val">' + (val !== undefined && val !== null ? val : '-') + '</span></div>'
    ).join('');
  }
  renderCameraInfo();
}

async function renderCameraInfo() {
  const info = $('camera-info');
  if (!info) return;
  const cam = store.cameras[0];
  if (!cam) {
    info.innerHTML = '<div class="reading"><span class="reading-label">' + t('noCamera') + '</span></div>';
    return;
  }
  const rows = [
    [t('cameraName'), cam.name],
    [t('statusLabel'), t(cam.status === 'offline' ? 'statusOffline'
      : hasCap('camera_control') && cam.status === 'idle' ? 'statusIdle'
      : hasCap('camera_control') ? 'statusRunning' : 'statusOnline')],
    [t('resolution'), cam.resolution],
    [t('fps'), cam.fps],
  ];
  info.innerHTML = rows.map(([label, val]) =>
    '<div class="reading"><span class="reading-label">' + esc(label) + '</span><span class="reading-val">' + esc(val !== undefined && val !== null ? String(val) : '-') + '</span></div>'
  ).join('');
}

// ─── Observability (SPEC §3.2) ──────────────────────────────────────

function initObsCharts() {
  initChart('obs-cpu', { fmt: fmtPercent });
  initChart('obs-mem', { fmt: fmtPercent });
  initChart('obs-disk', { fmt: fmtPercent });
  initChart('obs-net', { two: true, fmt: fmtRate });
  initChart('obs-pcpu', { fmt: fmtPercent });
  initChart('obs-prss', { fmt: fmtBytes });
  initChart('obs-pstore', { fmt: fmtBytes });
  initChart('obs-pnet', { two: true, fmt: fmtRate });
}

async function pollMetrics() {
  const r = await api.get('/api/metrics/summary');
  if (!r.ok || !r.data) return;
  const d = r.data;
  const sys = d.system || {};
  const mem = sys.memory || {};
  const memPct = mem.total ? ((mem.total - (mem.available || 0)) / mem.total) * 100 : 0;
  pushChart('obs-cpu', sys.cpu_percent || 0);
  pushChart('obs-mem', memPct);
  const disks = sys.disks || [];
  const disk = disks[1] || disks[0];
  if (disk && disk.total) {
    pushChart('obs-disk', ((disk.used || 0) / disk.total) * 100);
    const lbl = $('obs-disk-path');
    if (lbl) lbl.textContent = disk.path;
  }
  const net = sys.network || {};
  pushChartTwo('obs-net', net.rx_rate || 0, net.tx_rate || 0);

  const p = d.process || {};
  pushChart('obs-pcpu', p.cpu_percent || 0);
  pushChart('obs-prss', p.rss_bytes || 0);
  pushChart('obs-pstore', p.storage_bytes || 0);
  const traf = p.traffic || {};
  const total = (traf.http_tx_bytes || 0) + (traf.rtsp_tx_bytes || 0) + (traf.gb28181_tx_bytes || 0);
  const prev = prevTraffic;
  if (prev) {
    const dt = (d.ts - prev.ts) || 2;
    const tx = Math.max(total - prev.total, 0) / dt;
    const rx = Math.max((traf.http_rx_bytes || 0) - prev.rx, 0) / dt;
    pushChartTwo('obs-pnet', rx, tx);
  }
  prevTraffic = { ts: d.ts, total, rx: traf.http_rx_bytes || 0 };
}

async function pollLogs() {
  const r = await api.get('/api/logs?limit=50');
  if (!r.ok) return;
  const box = $('obs-logs');
  if (!box) return;
  const entries = (r.data && r.data.entries) || [];
  box.innerHTML = entries.map((e) =>
    '<li class="obs-log log-' + esc(e.level) + '"><span class="obs-log-ts">' +
    esc(fmtClock(e.ts)) + '</span><span class="obs-log-level">' + esc((e.level || '').toUpperCase()) +
    '</span><span class="obs-log-msg">' + esc(e.message || '') + '</span></li>'
  ).join('');
}

async function pollRequests() {
  const r = await api.get('/api/requests?limit=20');
  if (!r.ok) return;
  const box = $('obs-requests');
  if (!box) return;
  const entries = (r.data && r.data.entries) || [];
  box.innerHTML = entries.map((e) =>
    '<tr><td class="mono">' + esc(e.id || '') + '</td><td>' + esc(e.method || '') +
    '</td><td class="obs-req-path">' + esc(e.path || '') + '</td><td>' +
    '<span class="state-pill ' + (e.status < 400 ? 'on' : 'off') + '">' + esc(String(e.status)) + '</span></td><td>' +
    esc((e.duration_ms !== undefined ? e.duration_ms.toFixed ? e.duration_ms.toFixed(1) : e.duration_ms : '-') + ' ms') + '</td></tr>'
  ).join('');
}

// ─── Resource profile (SPEC appendix A #40) ─────────────────────────
// Boot-time feature admission snapshot from capabilities.resource —
// static for the process lifetime, rendered once at init.

const RES_REASONS = {
  off_config: 'resOffConfig',
  off_budget: 'resOffBudget',
  off_tier: 'resOffTier',
  dependency: 'resDep',
};

function fmtMiB(v) {
  const n = Number(v);
  if (!isFinite(n)) return '-';
  return Math.round(n).toLocaleString() + ' MiB';
}

function renderResourceProfile() {
  const card = $('resource-card');
  const res = store.caps && store.caps.resource;
  if (card) card.classList.toggle('hidden', !res);
  if (!res) return;
  const sum = $('res-summary');
  if (sum) {
    const mode = document.createElement('span');
    mode.className = 'res-mode';
    mode.textContent = (res.mode || 'auto') === 'all' ? t('resModeAll') : t('resModeAuto');
    // Named tier pill (#40 named-tier addendum) — additive: devices on
    // older firmware simply don't advertise `tier` and keep today's line.
    const pills = [mode];
    if (res.tier) {
      const src = res.preset_source === 'config' ? t('resTierSrcCfg')
        : res.preset_source === 'hardware' ? t('resTierSrcHw')
        : res.preset_source;
      const pill = document.createElement('span');
      pill.className = 'res-tier mono';
      pill.textContent = t('resTier') + ' ' + res.tier + (src ? ' · ' + src : '');
      if (res.profile) {
        pill.title = 'cores=' + res.profile.physical_cores + ' · ' + (res.profile.simd || '')
          + (res.profile.battery ? ' · battery' : '');
      }
      pills.push(pill);
    }
    const stats = document.createElement('span');
    stats.className = 'res-stats mono';
    stats.textContent =
      t('resBudget') + ' ' + fmtMiB(res.budget_mib) +
      ' · ' + t('resAvailable') + ' ' + fmtMiB(res.available_mib) +
      ' / ' + fmtMiB(res.total_mib) +
      ' · ' + t('resReserve') + ' ' + fmtMiB(res.reserve_mib);
    pills.push(stats);
    sum.replaceChildren(...pills);
  }
  const list = $('res-features');
  if (list) {
    list.innerHTML = (res.features || []).map((f) => {
      const reasonKey = RES_REASONS[f.reason];
      const reason = f.admitted ? '' : (reasonKey ? t(reasonKey) : (f.reason || ''));
      return '<li class="res-row' + (f.admitted ? '' : ' res-row-off') + '" title="' + esc(reason) + '">' +
        '<span class="res-name">' + esc(t('resF_' + f.name)) + '</span>' +
        '<span class="res-cost mono">' + fmtMiB(f.cost_mib) + '</span>' +
        '<span class="state-pill ' + (f.admitted ? 'on' : 'off') + '">' +
        (f.admitted ? t('stateOn') : t('stateOff')) + '</span>' +
        (reason ? '<span class="res-reason">' + esc(reason) + '</span>' : '') +
        '</li>';
    }).join('');
  }
}

function fmtClock(ts) {
  const d = new Date((Number(ts) || 0) * 1000);
  if (isNaN(d.getTime())) return '-';
  const p = (n) => String(n).padStart(2, '0');
  return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
}

function fmtUptime(secs) {
  if (secs === undefined || secs === null) return '-';
  const s = Number(secs);
  if (isNaN(s)) return String(secs);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return d + 'd ' + h + 'h';
  if (h > 0) return h + 'h ' + m + 'm';
  return m + 'm ' + (s % 60) + 's';
}

export function initStatus() {
  // Hide the events badge row when the device has no SSE events.
  const evRow = $('events-row');
  if (evRow) evRow.classList.toggle('hidden', !hasCap('events') || !((store.caps.events || []).length));

  // Observability panes are capability-gated (SPEC §3.2; notebook hides all).
  const caps = obsCaps();
  const obsCard = $('obs-card');
  if (obsCard) obsCard.classList.toggle('hidden', !caps.metrics);
  const logsCard = $('obs-logs-card');
  if (logsCard) logsCard.classList.toggle('hidden', !caps.logs);
  const reqCard = $('obs-requests-card');
  if (reqCard) reqCard.classList.toggle('hidden', !caps.requests);
  if (caps.metrics) initObsCharts();
  // Boot-time resource feature admission (SPEC appendix A #40).
  renderResourceProfile();
}

export function startStatusPolling() {
  if (pollTimer) return;
  pollTimer = setInterval(() => {
    if (!$('sys-status').classList.contains('active')) return;
    checkApi();
    refreshStatus();
  }, 10000);
  const onStatusView = () => $('sys-status').classList.contains('active');
  metricsTimer = setInterval(() => {
    if (obsCaps().metrics && onStatusView()) pollMetrics();
  }, 2000);
  logsTimer = setInterval(() => {
    const caps = obsCaps();
    if (onStatusView() && (caps.logs || caps.requests)) {
      if (caps.logs) pollLogs();
      if (caps.requests) pollRequests();
    }
  }, 5000);
}

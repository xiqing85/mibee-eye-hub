// Hardware bring-up card (extension: thermal.hardware, SPEC §3.7) —
// configure the sensor wiring (pins / bus / address) and run the on-device
// discovery scan from the browser, so adapting a new board or module needs
// no SSH. The scan runs as a background job on the device (the sensor loop
// yields the bus while it sweeps); this card polls its state.

import { api } from './api.js';
import { store } from './store.js';
import { $ } from './ui.js';
import { t } from './i18n.js';

let pollTimer = null;

export function updateHardwareVisibility() {
  const hw = !!(store.caps && store.caps.thermal && store.caps.thermal.hardware);
  $('hardware-card').classList.toggle('hidden', !hw);
  if (hw && !pollTimer) {
    refreshHardware();
    wireHardware();
  }
}

/// Fetch current config + scan state; keeps polling while a scan runs.
async function refreshHardware() {
  const r = await api.get('/api/hardware');
  if (!r.ok) return;
  renderCurrent(r.data);
  renderScan(r.data.scan);
  if (r.data.scan && r.data.scan.running) {
    startPolling();
  }
}

function startPolling() {
  if (pollTimer) return;
  $('hardware-scan-btn').disabled = true;
  pollTimer = setInterval(async () => {
    const r = await api.get('/api/hardware/scan');
    if (!r.ok) return;
    renderScan(r.data);
    if (!r.data.running) stopPolling();
  }, 2000);
}

function stopPolling() {
  clearInterval(pollTimer);
  pollTimer = null;
  $('hardware-scan-btn').disabled = false;
}

/// "gpio:226,227" → {type:'gpio',sda,scl}; "twi:/dev/i2c-2" → {type:'twi',device}
function busFromHit(busStr) {
  if (busStr.startsWith('twi:')) return { type: 'twi', device: busStr.slice(4) };
  const pins = busStr.slice(5).split(',');
  return { type: 'gpio', sda: Number(pins[0]), scl: Number(pins[1]) };
}

function busLabel(bus) {
  if (!bus) return '—';
  if (bus.type === 'gpio') return `GPIO ${bus.sda}/${bus.scl}`;
  if (bus.type === 'twi') return bus.device;
  return 'auto';
}

function renderCurrent(d) {
  $('hardware-current-value').textContent =
    `${busLabel(d.bus)} · 0x${(d.addr || 0).toString(16).toUpperCase().padStart(2, '0')}`;
  // Pre-fill the manual form with the live values (edit-from-known-state).
  $('hardware-bus-type').value = d.bus ? d.bus.type : 'gpio';
  if (d.bus && d.bus.type === 'gpio') {
    $('hardware-sda').value = d.bus.sda;
    $('hardware-scl').value = d.bus.scl;
  }
  if (d.bus && d.bus.type === 'twi') $('hardware-device').value = d.bus.device;
  $('hardware-addr').value = '0x' + (d.addr || 0).toString(16);
  syncFormFields();
}

function renderScan(scan) {
  const status = $('hardware-scan-status');
  const box = $('hardware-scan-results');
  if (!scan) return;
  if (scan.running) {
    status.textContent = t('hardwareScanning');
    startPolling();
    return;
  }
  if (scan.error) {
    status.textContent = scan.error;
    return;
  }
  if (!scan.finished_ms) {
    status.textContent = '';
    return;
  }
  const n = (scan.hits || []).length;
  status.textContent = n
    ? t('hardwareScanDone').replace('{n}', n)
    : t('hardwareNoHit');
  box.innerHTML = '';
  for (const hit of scan.hits || []) {
    const row = document.createElement('div');
    row.className = 'hardware-hit';
    const label = document.createElement('span');
    label.textContent = `${hit.bus} · 0x${hit.addr.toString(16)} · id 0x${hit.id.toString(16)}`;
    const apply = document.createElement('button');
    apply.type = 'button';
    apply.className = 'btn-small';
    apply.textContent = t('hardwareApply');
    apply.addEventListener('click', () => applyConfig(busFromHit(hit.bus), hit.addr));
    row.append(label, apply);
    box.appendChild(row);
  }
  for (const err of scan.pair_errors || []) {
    const row = document.createElement('div');
    row.className = 'hardware-hit-error muted';
    row.textContent = err;
    box.appendChild(row);
  }
}

async function runScan() {
  $('hardware-scan-status').textContent = t('hardwareScanning');
  $('hardware-scan-results').innerHTML = '';
  const r = await api.post('/api/hardware/scan');
  if (r.ok) startPolling();
  else $('hardware-scan-status').textContent = r.error || 'error';
}

async function applyConfig(bus, addr) {
  const msg = $('hardware-msg');
  const r = await api.put('/api/hardware/config', { bus, addr });
  if (r.ok) {
    msg.textContent = r.data && r.data.persisted
      ? t('hardwareApplied')
      : t('hardwareAppliedNoPersist');
    refreshHardware();
  } else {
    msg.textContent = r.error || 'error';
  }
}

/// Manual form → config PUT. Addr accepts 0x33 / 51 forms.
function applyManual() {
  const type = $('hardware-bus-type').value;
  let bus;
  if (type === 'gpio') bus = { type: 'gpio', sda: Number($('hardware-sda').value), scl: Number($('hardware-scl').value) };
  else if (type === 'twi') bus = { type: 'twi', device: $('hardware-device').value.trim() };
  else bus = { type: 'auto' };
  const addr = Number.parseInt($('hardware-addr').value.trim(), 16) || 0x33;
  applyConfig(bus, addr);
}

/// Show the per-type fields (sda/scl vs device) for the selected bus type.
function syncFormFields() {
  const type = $('hardware-bus-type').value;
  $('hardware-sda').classList.toggle('hidden', type !== 'gpio');
  $('hardware-scl').classList.toggle('hidden', type !== 'gpio');
  $('hardware-device').classList.toggle('hidden', type !== 'twi');
}

function wireHardware() {
  if (wireHardware.done) return;
  wireHardware.done = true;
  $('hardware-scan-btn').addEventListener('click', runScan);
  $('hardware-apply-btn').addEventListener('click', applyManual);
  $('hardware-bus-type').addEventListener('change', syncFormFields);
}

// Central client-side state. Single source of truth for cross-module data.

export const store = {
  caps: null,          // /api/capabilities data (SPEC §3.1)
  view: 'preview',     // currently active view (main.showView keeps it fresh)
  user: null,          // {username, role} after login
  cameras: [],         // Camera[] (SPEC §4)
  currentCameraId: null,
  config: null,        // full config doc from GET /api/config
  configDirty: false,
  lang: localStorage.getItem('mibee_lang') || 'zh',
  theme: localStorage.getItem('mibee_theme') || 'dark',
  ptzEnabled: localStorage.getItem('mibee_ptz') === '1',
  hflip: localStorage.getItem('mibee_hflip') === '1',
  vflip: localStorage.getItem('mibee_vflip') === '1',
};

export function setLang(lang) {
  store.lang = lang;
  localStorage.setItem('mibee_lang', lang);
}

export function setTheme(theme) {
  store.theme = theme;
  localStorage.setItem('mibee_theme', theme);
}

export function setCameraId(id) {
  store.currentCameraId = id;
}

/// The camera the Live view is bound to. Falls back to the first camera,
/// then to the fixed single-camera id "0" (SPEC §4).
export function cameraId() {
  if (store.currentCameraId !== null) return store.currentCameraId;
  if (store.cameras.length > 0) return store.cameras[0].id;
  return '0';
}

export function hasCap(name) {
  return !!(store.caps && store.caps[name]);
}

/// Is a given SSE event name advertised in `capabilities.events`?
/// (SPEC §3.1: clients should only wire UI to advertised events.)
export function eventsHas(name) {
  return !!(store.caps && Array.isArray(store.caps.events) && store.caps.events.includes(name));
}

/// Test seam: swap the capabilities document and let callers re-run
/// visibility (used by the visual suite to exercise gating variants,
/// e.g. the cam-form ambient card).
export function setCapsForTests(caps) {
  store.caps = caps;
}

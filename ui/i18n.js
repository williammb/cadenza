// Minimal i18n loader — pulls strings from the Rust side at boot, then
// patches every `[data-i18n*]` element. Per DESIGN-desktop-v4.md
// § "UI (vanilla JS) — uma chamada no boot".
//
// `innerHTML` is intentionally not used — XSS-safe by construction.
// Supported attributes:
//   data-i18n             → textContent
//   data-i18n-aria-label  → aria-label attribute
//   data-i18n-placeholder → placeholder attribute
//   data-i18n-title       → title attribute (tooltip)

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let strings = {};
let activeLocale = "en";
const listeners = new Set();

export async function bootI18n() {
  try {
    activeLocale = await invoke("get_locale");
  } catch (e) {
    console.warn("get_locale failed, defaulting to en", e);
    activeLocale = "en";
  }
  await loadLocale(activeLocale);
  // The tray "Idioma: …" items emit this. Single source of truth so
  // the Settings dropdown and the tray stay in sync.
  try {
    await listen("locale_changed", (e) => {
      const next = typeof e.payload === "string" ? e.payload : activeLocale;
      loadLocale(next);
    });
  } catch (e) {
    console.warn("listen(locale_changed) failed", e);
  }
}

export async function loadLocale(locale) {
  try {
    strings = await invoke("load_translations", { locale });
    activeLocale = locale;
    document.documentElement.lang = locale;
    applyTranslations();
    for (const cb of listeners) {
      try { cb(locale); } catch (e) { console.warn(e); }
    }
  } catch (e) {
    console.error("load_translations failed", e);
  }
}

export function t(key, args = {}) {
  let s = strings[key] ?? key;
  for (const [k, v] of Object.entries(args)) {
    const val = String(v);
    // Fluent renders unresolved variables as `{$name}` (no spaces).
    // Our raw .ftl source uses `{ $name }`. And `{name}` is a fallback
    // for plain placeholder strings. Cover all three so substitution
    // is robust regardless of which side did the rendering.
    s = s
      .replaceAll(`{ $${k} }`, val)
      .replaceAll(`{$${k}}`, val)
      .replaceAll(`{${k}}`, val);
  }
  return s;
}

export function getActiveLocale() {
  return activeLocale;
}

export function onLocaleChange(cb) {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

function applyTranslations() {
  document.querySelectorAll("[data-i18n]").forEach((el) => {
    const v = strings[el.dataset.i18n];
    if (v != null) el.textContent = v;
  });
  document.querySelectorAll("[data-i18n-aria-label]").forEach((el) => {
    const v = strings[el.dataset.i18nAriaLabel];
    if (v != null) el.setAttribute("aria-label", v);
  });
  document.querySelectorAll("[data-i18n-placeholder]").forEach((el) => {
    const v = strings[el.dataset.i18nPlaceholder];
    if (v != null) el.setAttribute("placeholder", v);
  });
  document.querySelectorAll("[data-i18n-title]").forEach((el) => {
    const v = strings[el.dataset.i18nTitle];
    if (v != null) el.setAttribute("title", v);
  });
}

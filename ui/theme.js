// Two-state theme override (light / dark). When the user has never
// toggled, the document has no `data-theme` attribute and CSS falls
// back to `prefers-color-scheme`. Toggling once writes the explicit
// choice to localStorage and applies `data-theme` to <html>, which
// then wins over the media query (see styles.css :root[data-theme]).

const STORAGE_KEY = "cadenza-theme";

export function initTheme() {
  const saved = read();
  if (saved === "light" || saved === "dark") {
    document.documentElement.setAttribute("data-theme", saved);
  }
}

/// Flip to the opposite of whatever's currently being shown. The
/// "current" is the effective theme (explicit override OR OS pref),
/// so the first toggle on a dark-OS install flips to light, and vice
/// versa — matches what the user sees on screen, not what's stored.
export function toggleTheme() {
  const next = currentTheme() === "dark" ? "light" : "dark";
  document.documentElement.setAttribute("data-theme", next);
  try { localStorage.setItem(STORAGE_KEY, next); } catch {}
  return next;
}

export function currentTheme() {
  const explicit = document.documentElement.getAttribute("data-theme");
  if (explicit === "light" || explicit === "dark") return explicit;
  return systemTheme();
}

function systemTheme() {
  try {
    return window.matchMedia("(prefers-color-scheme: dark)").matches
      ? "dark"
      : "light";
  } catch {
    return "light";
  }
}

function read() {
  try { return localStorage.getItem(STORAGE_KEY); } catch { return null; }
}

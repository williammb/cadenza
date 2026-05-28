// Two-state theme override (light / dark). initTheme() sets data-theme on
// <html> at startup — from localStorage if saved, otherwise from the OS via
// window.matchMedia. Toggling writes the new choice to localStorage and
// updates data-theme (see styles.css :root[data-theme]).
//
// When no explicit override is saved, we also subscribe to the OS theme
// media query so a system-level light/dark switch (macOS/Windows auto
// appearance at sunset) propagates live instead of being pinned to the
// boot-time value.

const STORAGE_KEY = "cadenza-theme";

export function initTheme() {
  const saved = read();
  const hasOverride = saved === "light" || saved === "dark";
  document.documentElement.setAttribute(
    "data-theme",
    hasOverride ? saved : systemTheme(),
  );
  if (!hasOverride) {
    try {
      window
        .matchMedia("(prefers-color-scheme: dark)")
        .addEventListener("change", (e) => {
          if (read()) return;
          document.documentElement.setAttribute(
            "data-theme",
            e.matches ? "dark" : "light",
          );
        });
    } catch {}
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

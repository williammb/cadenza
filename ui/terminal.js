// Terminal drawer — wraps xterm.js (UMD from vendor/) and multiplexes
// across multiple PTY sessions via a tab strip.
//
// Public surface:
//   attachTerminal(sessionId, { taskId, title }) — register the session,
//     render its tab, and make it the active xterm view.
//   detachTerminal()  — dispose the current xterm view without killing
//                       any PTY (used internally when switching tabs).
//   closeSession(id)  — kill the PTY for `id`, remove its tab; if it
//                       was the active one, fall back to another tab or
//                       collapse the drawer.
//   toggleDrawer(open) — expand / collapse the drawer.
//
// The drawer is a flex sibling of <main> (see styles.css), so expanding
// shrinks the board rather than overlaying it. A drag handle on the top
// edge lets the user resize between MIN_HEIGHT and (viewport - topbar).

import { t } from "./i18n.js";

const { invoke, Channel } = window.__TAURI__.core;

const drawer = document.getElementById("terminal-drawer");
const toggleBtn = document.getElementById("btn-terminal-toggle");
const tabsEl = document.getElementById("terminal-tabs");
const host = document.getElementById("terminal-host");
const emptyEl = document.getElementById("terminal-empty");
const resizeHandle = document.getElementById("terminal-resize-handle");

let term = null;
let fitAddon = null;
let activeSession = null;
let resizeObserver = null;

// All PTY sessions the UI currently knows about. Each entry:
//   { taskId: string|null, title: string|null }
// Keyed by the backend's `S-…` session id. The xterm view shows
// whichever id matches `activeSession`.
const sessions = new Map();

// Track per-session disposables (resize observers, data listeners) so
// switching tabs doesn't leak them.

export function isOpen() {
  return drawer.getAttribute("data-collapsed") !== "true";
}

export function toggleDrawer(open) {
  const next = open == null ? !isOpen() : open;
  drawer.setAttribute("data-collapsed", next ? "false" : "true");
  if (next && term) {
    queueMicrotask(() => safeFit());
  }
}

export async function attachTerminal(sessionId, opts = {}) {
  const meta = {
    taskId: opts.taskId ?? sessions.get(sessionId)?.taskId ?? null,
    title: opts.title ?? sessions.get(sessionId)?.title ?? null,
  };
  sessions.set(sessionId, meta);
  renderTabs();

  if (activeSession === sessionId && term) {
    toggleDrawer(true);
    return;
  }
  detachTerminal();

  const Terminal = window.Terminal;
  const FitAddonExport = window.FitAddon;
  const FitAddon =
    typeof FitAddonExport === "function" ? FitAddonExport : FitAddonExport?.FitAddon;
  if (!Terminal || !FitAddon) {
    console.error("xterm vendor not loaded — check ui/vendor/xterm.js");
    return;
  }

  term = new Terminal({
    fontFamily:
      'Cascadia Code, "JetBrains Mono", Menlo, Consolas, ui-monospace, monospace',
    fontSize: 13,
    cursorBlink: true,
    scrollback: 5000,
    convertEol: false,
    theme: currentTheme(),
  });
  fitAddon = new FitAddon();
  term.loadAddon(fitAddon);

  emptyEl.hidden = true;
  host.hidden = false;

  term.open(host);
  safeFit();

  // Sync the PTY size to the xterm BEFORE attaching, so the child
  // process (claude/codex) sees the real cols/rows from the first byte
  // it writes. Without this, the child keeps the spawn-time default
  // (agent.rs DEFAULT_COLS/ROWS = 120×30) while xterm renders at the
  // drawer's actual width — and any cursor-rewrite sequences (spinner,
  // progress bars) overlap because \x1b[K clears only up to col 120.
  if (term.cols && term.rows) {
    try {
      await invoke("pty_resize", { sessionId, cols: term.cols, rows: term.rows });
    } catch (e) {
      console.warn("initial pty_resize failed", e);
    }
  }

  const channel = new Channel();
  channel.onmessage = (bytes) => {
    if (!term) return;
    term.write(new Uint8Array(bytes));
  };

  try {
    await invoke("pty_attach", { sessionId, channel });
  } catch (e) {
    console.error("pty_attach failed", e);
    term.write(`\r\n\x1b[31m${t("terminal-attach-error", { error: e })}\x1b[0m\r\n`);
    return;
  }

  term.onData((data) => {
    invoke("pty_write", {
      sessionId,
      data: new TextEncoder().encode(data),
    }).catch((err) => console.warn("pty_write failed", err));
  });

  resizeObserver = new ResizeObserver(() => safeFit(sessionId));
  resizeObserver.observe(host);

  activeSession = sessionId;
  renderTabs();
  toggleDrawer(true);
}

export function detachTerminal() {
  if (resizeObserver) {
    resizeObserver.disconnect();
    resizeObserver = null;
  }
  if (term) {
    term.dispose();
    term = null;
  }
  fitAddon = null;
  activeSession = null;
  host.replaceChildren();
  host.hidden = true;
  emptyEl.hidden = false;
  renderTabs();
}

/// Kill the PTY for `sessionId` and remove its tab. If it was the
/// active view, attach the next remaining session (or collapse the
/// drawer entirely if there's nothing left).
export async function closeSession(sessionId) {
  const wasActive = activeSession === sessionId;
  sessions.delete(sessionId);

  try {
    await invoke("pty_kill", { sessionId });
  } catch (e) {
    console.warn("pty_kill failed", e);
  }

  if (wasActive) {
    detachTerminal();
    const next = sessions.keys().next();
    if (!next.done) {
      attachTerminal(next.value);
    } else {
      toggleDrawer(false);
    }
  } else {
    renderTabs();
  }
}

function renderTabs() {
  tabsEl.replaceChildren();
  for (const [sessionId, meta] of sessions) {
    const tab = document.createElement("button");
    tab.type = "button";
    tab.className = "terminal-tab";
    if (sessionId === activeSession) tab.classList.add("active");
    tab.dataset.sessionId = sessionId;

    if (meta.taskId) {
      const idSpan = document.createElement("span");
      idSpan.className = "terminal-tab-id";
      idSpan.textContent = meta.taskId;
      tab.append(idSpan);
    }
    if (meta.title) {
      const titleSpan = document.createElement("span");
      titleSpan.className = "terminal-tab-title";
      titleSpan.textContent = meta.title;
      tab.append(titleSpan);
    }
    if (!meta.taskId && !meta.title) {
      // No metadata — at least show the session id so the tab isn't blank.
      const idSpan = document.createElement("span");
      idSpan.className = "terminal-tab-id";
      idSpan.textContent = shortSessionId(sessionId);
      tab.append(idSpan);
    }

    const closeBtn = document.createElement("span");
    closeBtn.className = "terminal-tab-close";
    closeBtn.textContent = "×";
    closeBtn.setAttribute("aria-label", t("terminal-close-aria"));
    closeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      closeSession(sessionId);
    });
    tab.append(closeBtn);

    tab.addEventListener("click", () => {
      if (sessionId !== activeSession) attachTerminal(sessionId);
    });
    tabsEl.append(tab);
  }
}

function shortSessionId(id) {
  return id.length > 10 ? id.slice(0, 10) + "…" : id;
}

function safeFit(sessionIdForResize) {
  if (!fitAddon || !term) return;
  try {
    fitAddon.fit();
    if (sessionIdForResize) {
      invoke("pty_resize", {
        sessionId: sessionIdForResize,
        cols: term.cols,
        rows: term.rows,
      }).catch(() => {});
    }
  } catch (e) {
    // fit() throws on hidden containers; ignore until visible.
  }
}

function currentTheme() {
  const dark =
    document.documentElement.dataset.theme === "dark" ||
    (document.documentElement.dataset.theme !== "light" &&
      window.matchMedia?.("(prefers-color-scheme: dark)").matches);
  return dark
    ? { background: "#1c1f24", foreground: "#e6e7eb", cursor: "#60a5fa" }
    : { background: "#1c1f24", foreground: "#e6e7eb", cursor: "#3b82f6" };
}

// ─────────────────────────── drag-to-resize ───────────────────────────
//
// The handle sits on the top edge of the drawer. While dragging, we
// disable the height transition (data-resizing="true") so the panel
// follows the cursor 1:1, and clamp the height between MIN_HEIGHT
// (just the header) and the viewport minus a small margin.

const MIN_HEIGHT = 38;
const TOP_MARGIN = 80; // leave room for the topbar + a glance of the board

resizeHandle?.addEventListener("pointerdown", (e) => {
  if (drawer.getAttribute("data-collapsed") === "true") return;
  e.preventDefault();
  resizeHandle.setPointerCapture(e.pointerId);
  drawer.setAttribute("data-resizing", "true");

  const onMove = (ev) => {
    const fromTop = ev.clientY;
    const desired = window.innerHeight - fromTop;
    const max = window.innerHeight - TOP_MARGIN;
    const clamped = Math.max(MIN_HEIGHT, Math.min(desired, max));
    drawer.style.height = `${clamped}px`;
    safeFit(activeSession);
  };

  const stop = () => {
    drawer.removeAttribute("data-resizing");
    resizeHandle.removeEventListener("pointermove", onMove);
    resizeHandle.removeEventListener("pointerup", stop);
    resizeHandle.removeEventListener("pointercancel", stop);
    try {
      resizeHandle.releasePointerCapture(e.pointerId);
    } catch {
      /* releaseCapture throws if already released — ignore */
    }
  };

  resizeHandle.addEventListener("pointermove", onMove);
  resizeHandle.addEventListener("pointerup", stop);
  resizeHandle.addEventListener("pointercancel", stop);
});

// Header chevron toggles the drawer regardless of whether a session is
// attached — letting the user open the empty state to spawn one later.
toggleBtn?.addEventListener("click", () => toggleDrawer());

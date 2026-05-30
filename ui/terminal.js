// Terminal drawer — wraps xterm.js (UMD from vendor/) and multiplexes
// across multiple PTY sessions, ONE dedicated xterm per session.
//
// Each session owns its own xterm + FitAddon + host element + PTY
// stream. All hosts stay mounted in #terminal-host at once; only the
// active session's host is visible (the rest are `hidden`). Switching
// tabs flips visibility — it never disposes or re-streams, so a hidden
// session keeps receiving its own output without bleeding into another.
//
// Public surface:
//   attachTerminal(sessionId, { taskId, title }) — register the session
//     (creating its xterm + PTY stream on first call, idempotent after),
//     render its tab, and make it the active view.
//   detachTerminal()  — hide the active host without disposing anything
//                       (drops back to the empty state).
//   closeSession(id)  — kill the PTY for `id`, dispose its xterm, and
//                       remove its tab; if it was active, fall back to
//                       another tab or collapse the drawer.
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

let activeSession = null;
const MAX_WRITE_BATCH_BYTES = 64 * 1024;

// Every PTY session the UI knows about, keyed by the backend's `S-…`
// session id. Each entry:
//   { taskId, title, term, fitAddon, channel, hostEl, resizeObserver,
//     lastCols, lastRows }
// `term`/`hostEl` live for the whole session lifetime; only the active
// session's `hostEl` is visible.
const sessions = new Map();

export function isOpen() {
  return drawer.getAttribute("data-collapsed") !== "true";
}

export function toggleDrawer(open) {
  const next = open == null ? !isOpen() : open;
  drawer.setAttribute("data-collapsed", next ? "false" : "true");
  if (next && activeSession) {
    queueMicrotask(() => fitSession(activeSession));
  }
}

export async function attachTerminal(sessionId, opts = {}) {
  const existing = sessions.get(sessionId);
  if (existing) {
    // Already streaming — just refresh metadata and bring it forward.
    // Never recreate the xterm or re-call pty_attach (idempotent).
    if (opts.taskId != null) existing.taskId = opts.taskId;
    if (opts.title != null) existing.title = opts.title;
    showSession(sessionId);
    toggleDrawer(true);
    renderTabs();
    fitSession(sessionId);
    return;
  }

  const Terminal = window.Terminal;
  const FitAddonExport = window.FitAddon;
  const FitAddon =
    typeof FitAddonExport === "function" ? FitAddonExport : FitAddonExport?.FitAddon;
  if (!Terminal || !FitAddon) {
    console.error("xterm vendor not loaded — check ui/vendor/xterm.js");
    return;
  }

  // Dedicated host element + xterm for this session, mounted alongside
  // the other sessions' hosts inside #terminal-host.
  const hostEl = document.createElement("div");
  hostEl.className = "terminal-pane";
  hostEl.dataset.sessionId = sessionId;
  host.append(hostEl);

  const term = new Terminal({
    fontFamily:
      'Cascadia Code, "JetBrains Mono", Menlo, Consolas, ui-monospace, monospace',
    fontSize: 13,
    cursorBlink: true,
    scrollback: 5000,
    convertEol: false,
    theme: currentTheme(),
  });
  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);

  const entry = {
    taskId: opts.taskId ?? null,
    title: opts.title ?? null,
    term,
    fitAddon,
    channel: null,
    hostEl,
    resizeObserver: null,
    writeQueue: [],
    writeInFlight: false,
    writeClosed: false,
    lastCols: 0,
    lastRows: 0,
  };
  sessions.set(sessionId, entry);

  emptyEl.hidden = true;
  host.hidden = false;

  term.open(hostEl);
  showSession(sessionId);

  // Sync the PTY size to the xterm BEFORE attaching, so the child
  // process (claude/codex) sees the real cols/rows from the first byte
  // it writes. Without this, the child keeps the spawn-time default
  // (agent.rs DEFAULT_COLS/ROWS = 120×30) while xterm renders at the
  // drawer's actual width — and any cursor-rewrite sequences (spinner,
  // progress bars) overlap because \x1b[K clears only up to col 120.
  fitSession(sessionId);

  // The handler captures THIS session's `term` by closure, so its bytes
  // can never be written into another session's xterm — that was the
  // root of the multi-terminal corruption bug.
  const channel = new Channel();
  channel.onmessage = (bytes) => {
    enqueueTerminalBytes(entry, bytes);
  };
  entry.channel = channel;

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

  const resizeObserver = new ResizeObserver(() => fitSession(sessionId));
  resizeObserver.observe(hostEl);
  entry.resizeObserver = resizeObserver;

  renderTabs();
  toggleDrawer(true);
}

/// Hide whichever session is active, falling back to the empty state.
/// Disposes nothing — the PTY stream and xterm stay alive in the
/// background. Real teardown only happens in `closeSession`.
export function detachTerminal() {
  for (const entry of sessions.values()) {
    entry.hostEl.hidden = true;
  }
  activeSession = null;
  host.hidden = true;
  emptyEl.hidden = false;
  renderTabs();
}

/// Kill the PTY for `sessionId`, dispose its xterm, and remove its tab.
/// If it was the active view, show the next remaining session (or
/// collapse the drawer entirely if there's nothing left).
export async function closeSession(sessionId) {
  const wasActive = activeSession === sessionId;
  const entry = sessions.get(sessionId);
  sessions.delete(sessionId);

  if (entry) {
    if (entry.resizeObserver) entry.resizeObserver.disconnect();
    entry.writeClosed = true;
    entry.writeQueue = [];
    if (entry.term) entry.term.dispose();
    entry.hostEl.remove();
  }

  try {
    await invoke("pty_kill", { sessionId });
  } catch (e) {
    console.warn("pty_kill failed", e);
  }

  if (wasActive) {
    activeSession = null;
    const next = sessions.keys().next();
    if (!next.done) {
      showSession(next.value);
      host.hidden = false;
      emptyEl.hidden = true;
      renderTabs();
      fitSession(next.value);
    } else {
      host.hidden = true;
      emptyEl.hidden = false;
      renderTabs();
      toggleDrawer(false);
    }
  } else {
    renderTabs();
  }
}

/// Make `sessionId`'s host the only visible one. Visibility-only — no
/// dispose, no re-stream.
function showSession(sessionId) {
  activeSession = sessionId;
  for (const [id, entry] of sessions) {
    entry.hostEl.hidden = id !== sessionId;
  }
}

function enqueueTerminalBytes(entry, bytes) {
  if (!entry || entry.writeClosed || !entry.term) return;
  const chunk = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (!chunk.byteLength) return;
  entry.writeQueue.push(chunk);
  drainTerminalWrites(entry);
}

function drainTerminalWrites(entry) {
  if (entry.writeInFlight || entry.writeClosed || !entry.term) return;
  if (!entry.writeQueue.length) return;

  const chunks = [];
  let total = 0;
  while (entry.writeQueue.length && total < MAX_WRITE_BATCH_BYTES) {
    const next = entry.writeQueue.shift();
    chunks.push(next);
    total += next.byteLength;
  }

  const payload = chunks.length === 1 ? chunks[0] : concatBytes(chunks, total);
  entry.writeInFlight = true;
  entry.term.write(payload, () => {
    entry.writeInFlight = false;
    if (entry.writeQueue.length) {
      queueMicrotask(() => drainTerminalWrites(entry));
    }
  });
}

function concatBytes(chunks, total) {
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
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

/// Fit `sessionId`'s xterm to its host and push the new size to its PTY
/// (only when it actually changed). Skips hidden panes — xterm's fit()
/// can't measure a `display:none` element.
function fitSession(sessionId) {
  const entry = sessions.get(sessionId);
  if (!entry || !entry.fitAddon || !entry.term) return;
  if (entry.hostEl.hidden) return;
  try {
    entry.fitAddon.fit();
    const { cols, rows } = entry.term;
    if (cols && rows && (cols !== entry.lastCols || rows !== entry.lastRows)) {
      entry.lastCols = cols;
      entry.lastRows = rows;
      invoke("pty_resize", { sessionId, cols, rows }).catch(() => {});
    }
  } catch (e) {
    // fit() throws on hidden/zero-size containers; ignore until visible.
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
    if (activeSession) fitSession(activeSession);
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

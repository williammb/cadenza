// Terminal drawer — wraps xterm.js (UMD from vendor/) and multiplexes
// across multiple PTY sessions, ONE dedicated xterm per session.
//
// Sessions are *registered* the moment an agent run attaches, but their
// xterm is NOT created until the session is *placed* in the grid. The
// drawer shows a toolbar listing every registered session plus a grid
// holding up to MAX_PANES placed panes. Placing a session lazily creates
// its xterm, mounts it in the now-visible cell, syncs the PTY size, and
// calls pty_attach exactly once — the backend's 256 KiB ring replays the
// scrollback so deferring the attach loses nothing within that bound.
//
// Each placed session owns its own xterm + FitAddon + host element + PTY
// stream. All placed hosts stay mounted in #terminal-host at once and are
// laid out via CSS grid (no re-parenting); unplaced sessions keep their
// host `hidden`. A session's bytes are captured by closure, so they can
// never bleed into another session's xterm.
//
// Public surface:
//   attachTerminal(sessionId, { taskId, title }) — register the session,
//     render its toolbar entry, and open the drawer. Does NOT create the
//     xterm or attach the PTY (that happens on first placement).
//   placeSession / removeFromGrid — add/remove a session from the grid
//     (removeFromGrid keeps the PTY + tab alive).
//   closeSession(id)  — kill the PTY for `id`, dispose its xterm, and
//                       remove its tab AND its grid pane.
//   toggleDrawer(open) — show / hide the drawer (single state setter).
//   onDrawerStateChange(cb) — subscribe to show/hide for topbar sync.
//
// The drawer is a flex sibling of <main> (see styles.css), so showing it
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
const gridLiveEl = document.getElementById("terminal-grid-live");
const body = drawer?.querySelector(".terminal-body");
// Explicit "drop to add a pane" strip, revealed only while a session tab is
// being dragged and there's still a free cell. It overlays the right edge so
// the panes themselves stay hittable for swap/replace.
const addZone = document.getElementById("terminal-add-zone");

// Ordered sessionIds currently in the grid (max MAX_PANES). The placed
// session whose pane holds keyboard focus, or null.
const placed = [];
let focusedSessionId = null;
const MAX_PANES = 4;
const SESSION_MIME = "application/x-cadenza-session-id";
const MAX_WRITE_BATCH_BYTES = 64 * 1024;

// Every PTY session the UI knows about, keyed by the backend's `S-…`
// session id. Each entry:
//   { taskId, title, term, fitAddon, channel, hostEl, resizeObserver,
//     writeQueue, writeInFlight, writeClosed, lastCols, lastRows,
//     opened, attached }
// `term`/`hostEl` are null until the session is first placed; `opened`
// and `attached` guard the one-time term.open()/pty_attach.
const sessions = new Map();

// The toolbar (#terminal-tabs) is an ARIA toolbar, not a tablist. Mark it
// once so placement before owner #1's HTML change still sets the role.
if (tabsEl && tabsEl.getAttribute("role") !== "toolbar") {
  tabsEl.setAttribute("role", "toolbar");
}

let drawerStateCb = null;

export function onDrawerStateChange(cb) {
  drawerStateCb = cb;
}

export function isOpen() {
  return drawer.getAttribute("data-collapsed") !== "true";
}

/// Single state setter for show/hide. On show it schedules a rAF fit of
/// every placed pane (display:none → shown fires NO transitionend, so we
/// cannot wait on the transition). Always invokes the drawer-state hook so
/// every caller (topbar button, chevron, closeSession empty branch) keeps
/// the topbar button's aria-pressed in sync.
export function toggleDrawer(open) {
  const next = open == null ? !isOpen() : open;
  drawer.setAttribute("data-collapsed", next ? "false" : "true");
  if (next) {
    requestAnimationFrame(() => fitPlaced());
  }
  drawerStateCb?.(next);
}

/// Register a session: store its metadata, render the toolbar, and open
/// the drawer. Does NOT create the xterm, term.open, or pty_attach — that
/// is deferred to the first placeSession() call.
export async function attachTerminal(sessionId, opts = {}) {
  const existing = sessions.get(sessionId);
  if (existing) {
    // Already registered — just refresh metadata, re-render, and surface
    // the drawer. Never recreate the xterm or re-attach.
    if (opts.taskId != null) existing.taskId = opts.taskId;
    if (opts.title != null) existing.title = opts.title;
    renderToolbar();
    toggleDrawer(true);
    return;
  }

  sessions.set(sessionId, {
    taskId: opts.taskId ?? null,
    title: opts.title ?? null,
    term: null,
    fitAddon: null,
    channel: null,
    hostEl: null,
    resizeObserver: null,
    writeQueue: [],
    writeInFlight: false,
    writeClosed: false,
    lastCols: 0,
    lastRows: 0,
    opened: false,
    attached: false,
  });

  renderToolbar();
  // Shows the drawer with the toolbar + the empty-grid drop hint
  // (#terminal-empty); the session is registered but not yet placed.
  toggleDrawer(true);
}

/// Lazily create a session's xterm + FitAddon + host element (a direct child
/// of #terminal-host — never re-parented; CSS `order` places it) plus its
/// per-pane × and DnD wiring. Idempotent (no-op once `entry.term` exists).
/// Returns false only when the xterm vendor failed to load. Shared by both
/// the first-placement path and the drop-replace path so the Terminal options
/// and pane wiring live in exactly one place.
function ensurePaneCreated(entry, sessionId) {
  if (entry.term) return true;
  const Terminal = window.Terminal;
  const FitAddonExport = window.FitAddon;
  const FitAddon =
    typeof FitAddonExport === "function" ? FitAddonExport : FitAddonExport?.FitAddon;
  if (!Terminal || !FitAddon) {
    console.error("xterm vendor not loaded — check ui/vendor/xterm.js");
    return false;
  }

  const hostEl = document.createElement("div");
  hostEl.className = "terminal-pane";
  hostEl.dataset.sessionId = sessionId;
  host.append(hostEl);

  // Per-pane × — remove this session from the grid (PTY stays alive),
  // distinct from the toolbar kill ×.
  const paneClose = document.createElement("button");
  paneClose.type = "button";
  paneClose.className = "terminal-pane-close";
  paneClose.textContent = "×";
  paneClose.setAttribute("aria-label", t("terminal-pane-close-aria"));
  paneClose.addEventListener("click", (e) => {
    e.stopPropagation();
    removeFromGrid(sessionId);
  });
  hostEl.append(paneClose);

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

  entry.term = term;
  entry.fitAddon = fitAddon;
  entry.hostEl = hostEl;

  const resizeObserver = new ResizeObserver(() => fitSession(sessionId));
  resizeObserver.observe(hostEl);
  entry.resizeObserver = resizeObserver;

  // Clicking a pane focuses it (highlight + xterm focus).
  hostEl.addEventListener("mousedown", () => focusPane(sessionId));

  // Per-pane drop target — dropping a tab onto a pane replaces/swaps it.
  wirePaneDnd(hostEl, sessionId);
  return true;
}

/// Place `sessionId` into the next free grid cell. On first placement the
/// xterm + FitAddon + host element are created lazily, opened in the
/// now-visible cell, fitted, the PTY resized, and pty_attach called once.
/// Returns false (no-op) if the session is unknown or the grid is full;
/// returns true if it is (or becomes) placed.
function placeSession(sessionId) {
  const entry = sessions.get(sessionId);
  if (!entry) return false;
  if (placed.includes(sessionId)) return true;
  if (placed.length >= MAX_PANES) return false;

  if (!ensurePaneCreated(entry, sessionId)) return false;

  placed.push(sessionId);
  emptyEl.hidden = true;
  host.hidden = false;
  // Make the cell visible BEFORE term.open()/fit — xterm cannot measure a
  // display:none element. renderToolbar() refreshes this session's tab to its
  // placed state: the click/keyboard place paths reach placeSession directly
  // (only the drop handlers re-render separately), so without this the tab
  // would keep its unplaced styling/badge/aria-pressed after being placed.
  renderGrid();
  renderToolbar();

  // Sync the PTY size to the real cell BEFORE attaching, so the child process
  // (claude/codex) sees the actual cols/rows from its first byte instead of the
  // spawn-time default (agent.rs 120×30).
  openFitAttach(entry, sessionId);
  return true;
}

/// On the next frame, lazily open the session's xterm in its now-visible cell,
/// fit it to the real pixel size (resizing the PTY before any bytes flow), and
/// attach the stream exactly once. Shared by the first-placement path
/// (placeSession) and the drop-replace path (handlePaneDrop).
function openFitAttach(entry, sessionId) {
  requestAnimationFrame(() => {
    // The session may have been closed between scheduling this frame and now
    // (closeSession disposes the term + removes the host but keeps the captured
    // `entry` reference); bail rather than open a disposed terminal. Mirrors the
    // writeClosed guard in enqueueTerminalBytes/drainTerminalWrites.
    if (entry.writeClosed) return;
    if (!entry.opened) {
      entry.term.open(entry.hostEl);
      entry.opened = true;
    }
    fitSession(sessionId);
    if (!entry.attached) attachStream(entry, sessionId);
  });
}

/// Create the byte channel, call pty_attach once, and wire term.onData →
/// pty_write. Factored out of placeSession so the lazy-open path and the
/// drop-replace path share it. Sets entry.attached on success so the
/// 256 KiB ring is replayed exactly once.
async function attachStream(entry, sessionId) {
  if (entry.attached) return;

  // The handler captures THIS session's `entry` by closure, so its bytes
  // can never be written into another session's xterm — that was the root
  // of the multi-terminal corruption bug.
  const channel = new Channel();
  channel.onmessage = (bytes) => {
    enqueueTerminalBytes(entry, bytes);
  };
  entry.channel = channel;

  try {
    await invoke("pty_attach", { sessionId, channel });
  } catch (e) {
    console.error("pty_attach failed", e);
    entry.term?.write(
      `\r\n\x1b[31m${t("terminal-attach-error", { error: e })}\x1b[0m\r\n`,
    );
    return;
  }
  entry.attached = true;

  entry.term.onData((data) => {
    invoke("pty_write", {
      sessionId,
      data: new TextEncoder().encode(data),
    }).catch((err) => console.warn("pty_write failed", err));
  });
}

/// Remove `sessionId` from the grid while keeping its PTY + xterm + tab
/// alive. The pane is hidden (not disposed); re-placing it later reuses
/// the same xterm. No auto-fill of the freed cell.
export function removeFromGrid(sessionId) {
  const i = placed.indexOf(sessionId);
  if (i === -1) return;
  placed.splice(i, 1);
  const entry = sessions.get(sessionId);
  if (entry?.hostEl) entry.hostEl.hidden = true;
  if (focusedSessionId === sessionId) focusedSessionId = null;
  if (placed.length === 0) {
    host.hidden = true;
    emptyEl.hidden = false;
  }
  renderGrid();
  renderToolbar();
  announceGrid(sessionId);
}

/// Kill the PTY for `sessionId`, dispose its xterm, and remove its tab AND
/// grid pane. No auto-fill — a freed cell stays empty until the user
/// places something. If nothing is left placed, hide the drawer.
export async function closeSession(sessionId) {
  const entry = sessions.get(sessionId);
  // Capture the human label BEFORE deleting, so the aria-live announcement
  // uses the task id rather than the raw `S-…` session id.
  const label = entry?.taskId || shortSessionId(sessionId);
  sessions.delete(sessionId);

  const i = placed.indexOf(sessionId);
  if (i !== -1) placed.splice(i, 1);
  if (focusedSessionId === sessionId) focusedSessionId = null;

  if (entry) {
    if (entry.resizeObserver) entry.resizeObserver.disconnect();
    entry.writeClosed = true;
    entry.writeQueue = [];
    if (entry.term) entry.term.dispose();
    if (entry.hostEl) entry.hostEl.remove();
  }

  try {
    await invoke("pty_kill", { sessionId });
  } catch (e) {
    console.warn("pty_kill failed", e);
  }

  if (placed.length === 0) {
    host.hidden = true;
    emptyEl.hidden = false;
    renderToolbar();
    if (sessions.size === 0) {
      // Nothing registered at all — fully hide the drawer (the empty branch
      // also drives the topbar button via the drawer-state hook).
      toggleDrawer(false);
    } else {
      // Other sessions remain registered (unplaced) — keep the drawer open
      // showing the empty-grid hint + toolbar so those live sessions stay
      // reachable instead of vanishing with the drawer.
      announceGrid(sessionId, label);
    }
  } else {
    renderGrid();
    renderToolbar();
    announceGrid(sessionId, label);
  }
}

/// Try to place `sessionId`; on a full grid, surface the grid-full status
/// instead of a silent dead click.
function tryPlace(sessionId) {
  if (!placeSession(sessionId)) {
    setGridStatus(t("terminal-grid-full"));
  } else {
    announceGrid(sessionId);
  }
}

/// Focus a placed pane: remember it, focus its xterm, and re-render so the
/// focus highlight border tracks it.
function focusPane(sessionId) {
  if (!placed.includes(sessionId)) return;
  focusedSessionId = sessionId;
  const entry = sessions.get(sessionId);
  entry?.term?.focus();
  renderGrid();
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

/// Build the session toolbar (replaces the old renderTabs). The toolbar is
/// a role="toolbar" of session GROUPS; each group is two SIBLING buttons
/// (select/place + close) — never a span nested inside a button.
function renderToolbar() {
  tabsEl.replaceChildren();
  for (const [sessionId, meta] of sessions) {
    const isPlaced = placed.includes(sessionId);
    const label = meta.taskId || shortSessionId(sessionId);

    const group = document.createElement("div");
    group.className = "terminal-tab-group";
    group.setAttribute("role", "group");

    // Select/place button — draggable; toggles placement.
    const tab = document.createElement("button");
    tab.type = "button";
    tab.className = "terminal-tab";
    if (isPlaced) tab.classList.add("is-placed");
    tab.draggable = true;
    tab.dataset.sessionId = sessionId;
    tab.setAttribute("aria-pressed", isPlaced ? "true" : "false");
    tab.setAttribute(
      "aria-label",
      isPlaced
        ? t("terminal-remove-from-grid", { id: label })
        : t("terminal-add-to-grid", { id: label }),
    );
    // Tooltip reflects placed/unplaced state.
    tab.title = isPlaced ? t("terminal-tab-placed") : t("terminal-tab-unplaced");

    const idSpan = document.createElement("span");
    idSpan.className = "terminal-tab-id";
    idSpan.textContent = label;
    tab.append(idSpan);
    if (meta.title) {
      const titleSpan = document.createElement("span");
      titleSpan.className = "terminal-tab-title";
      titleSpan.textContent = meta.title;
      tab.append(titleSpan);
    }

    // Static "output not viewed" badge on every registered-but-unplaced
    // session (generic — there is no byte Channel before placement, so we
    // cannot detect real activity).
    if (!isPlaced) {
      const badge = document.createElement("span");
      badge.className = "terminal-output-pending";
      badge.textContent = t("terminal-output-pending");
      badge.title = t("terminal-output-pending");
      tab.append(badge);
    }

    tab.addEventListener("click", () => {
      if (placed.includes(sessionId)) focusPane(sessionId);
      else tryPlace(sessionId);
    });
    // Double-click is mouse convenience: place into the first free cell.
    tab.addEventListener("dblclick", () => {
      if (!placed.includes(sessionId)) tryPlace(sessionId);
    });
    tab.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        if (placed.includes(sessionId)) removeFromGrid(sessionId);
        else tryPlace(sessionId);
        // place/remove re-render the toolbar (replaceChildren), which would
        // drop keyboard focus to <body>. Restore it to this session's button
        // so a keyboard-only user keeps their place (PLAN §C.8).
        restoreToolbarFocus(sessionId);
      } else if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
        e.preventDefault();
        moveToolbarFocus(tab, e.key === "ArrowRight" ? 1 : -1);
      }
    });
    tab.addEventListener("dragstart", (e) => {
      e.dataTransfer.setData(SESSION_MIME, sessionId);
      e.dataTransfer.effectAllowed = "move";
      // Reveal the explicit "add a pane" strip only when the grid is tiled but
      // not full — at 0 panes the body itself is the append target, at 4 the
      // grid is full. Only an UNPLACED source can append into a free cell; a
      // placed source dragging can only swap/replace, so showing the strip for
      // it would be a dead drop target (the append drop rejects placed sources).
      if (
        addZone &&
        !placed.includes(sessionId) &&
        placed.length >= 1 &&
        placed.length < MAX_PANES
      ) {
        addZone.hidden = false;
      }
    });

    group.append(tab);

    // Close (kill) button — SIBLING of the select button.
    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.className = "terminal-tab-close";
    closeBtn.textContent = "×";
    closeBtn.setAttribute("aria-label", t("terminal-close-aria"));
    closeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      closeSession(sessionId);
    });
    closeBtn.addEventListener("keydown", (e) => {
      if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
        e.preventDefault();
        moveToolbarFocus(closeBtn, e.key === "ArrowRight" ? 1 : -1);
      }
    });
    group.append(closeBtn);

    tabsEl.append(group);
  }
  applyToolbarRovingTabindex();
}

/// Roving tabindex: exactly one toolbar button is tabbable at a time.
function applyToolbarRovingTabindex() {
  const buttons = toolbarButtons();
  buttons.forEach((b, i) => {
    b.tabIndex = i === 0 ? 0 : -1;
  });
}

function toolbarButtons() {
  return [...tabsEl.querySelectorAll("button")];
}

/// After a toolbar re-render, return keyboard focus to a given session's
/// select button (and make it the single tabbable one). Session ids are
/// `S-<uuid>` — safe in an attribute selector.
function restoreToolbarFocus(sessionId) {
  const btn = tabsEl.querySelector(
    `.terminal-tab[data-session-id="${sessionId}"]`,
  );
  if (!btn) return;
  for (const b of toolbarButtons()) b.tabIndex = -1;
  btn.tabIndex = 0;
  btn.focus();
}

/// Move keyboard focus along the toolbar (Left/Right), wrapping.
function moveToolbarFocus(current, dir) {
  const buttons = toolbarButtons();
  const idx = buttons.indexOf(current);
  if (idx === -1 || !buttons.length) return;
  const nextIdx = (idx + dir + buttons.length) % buttons.length;
  const nextBtn = buttons[nextIdx];
  for (const b of buttons) b.tabIndex = -1;
  nextBtn.tabIndex = 0;
  nextBtn.focus();
}

function shortSessionId(id) {
  return id.length > 10 ? id.slice(0, 10) + "…" : id;
}

/// Apply the grid layout to placed hosts. #terminal-host is a CSS grid:
/// a grid-N class drives the template, and each placed host's `order`
/// drives its cell. Hosts are NEVER re-parented; unplaced hosts are
/// hidden. The focused pane gets the .is-focused highlight.
function renderGrid() {
  host.classList.remove("grid-1", "grid-2", "grid-3", "grid-4");
  if (placed.length) host.classList.add(`grid-${placed.length}`);
  for (const [id, entry] of sessions) {
    if (!entry.hostEl) continue;
    const idx = placed.indexOf(id);
    if (idx === -1) {
      entry.hostEl.hidden = true;
    } else {
      entry.hostEl.hidden = false;
      entry.hostEl.style.order = String(idx);
      entry.hostEl.classList.toggle("is-focused", id === focusedSessionId);
    }
  }
}

/// Fit every placed pane. Called on rAF after a grid change and after a
/// drawer resize. Each fitSession keeps its own hidden/zero-rect guard.
function fitPlaced() {
  for (const id of placed) fitSession(id);
}

/// Fit `sessionId`'s xterm to its host and push the new size to its PTY
/// (only when it actually changed). Skips hidden panes — xterm's fit()
/// can't measure a `display:none` element.
function fitSession(sessionId) {
  const entry = sessions.get(sessionId);
  if (!entry || !entry.fitAddon || !entry.term || !entry.hostEl) return;
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

/// Write a grid status line into the aria-live region. `id` is the label
/// of the last-changed session; `n` is the current placed count.
function announceGrid(sessionId, labelArg) {
  if (!gridLiveEl) return;
  const meta = sessions.get(sessionId);
  // Prefer an explicit label (the caller may have already deleted the session
  // from the Map, e.g. closeSession) so we never fall back to the raw `S-…`.
  const label =
    labelArg ?? (meta ? meta.taskId || shortSessionId(sessionId) : shortSessionId(sessionId));
  gridLiveEl.textContent = t("terminal-grid-status", {
    id: label,
    n: placed.length,
  });
}

/// Write a status message (e.g. grid-full) into the same live region.
function setGridStatus(text) {
  if (!gridLiveEl) return;
  gridLiveEl.textContent = text;
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

// ─────────────────────────── drag-and-drop ────────────────────────────
//
// A toolbar tab can be dragged into the grid in two ways: APPEND a new pane
// (drop on the drawer body when the grid is empty, or on the "add-zone" strip
// when it is tiled-but-not-full) or REPLACE/SWAP (drop on an existing pane).
// Append lives on the always-visible body + add-zone rather than on
// #terminal-host, which is display:none while empty and so cannot receive the
// first-placement drop. The payload is the sessionId under a private MIME type;
// drops validate the session still exists, so foreign/stale payloads are
// ignored.

function clearDropTargets() {
  host.classList.remove("drop-target");
  body?.classList.remove("drop-target");
  addZone?.classList.remove("drop-target");
  for (const el of host.querySelectorAll(".terminal-pane.drop-target")) {
    el.classList.remove("drop-target");
  }
}

function dndHasSession(e) {
  return e.dataTransfer && [...e.dataTransfer.types].includes(SESSION_MIME);
}

/// Append-on-drop: place the dragged session into the next free cell. Used by
/// the two always-reachable append surfaces — the drawer body (covers the
/// empty-grid case, where #terminal-host is display:none and cannot receive
/// drops — the original "host.hidden" gap) and the add-zone strip (covers the
/// tiled-grid case, where panes fill the host and would otherwise only swap).
function wireAppendDrop(el, stopBubble) {
  if (!el) return;
  el.addEventListener("dragover", (e) => {
    if (!dndHasSession(e) || placed.length >= MAX_PANES) return;
    e.preventDefault();
    if (stopBubble) e.stopPropagation();
    clearDropTargets();
    el.classList.add("drop-target");
  });
  el.addEventListener("dragleave", () => el.classList.remove("drop-target"));
  el.addEventListener("drop", (e) => {
    if (!dndHasSession(e)) return;
    e.preventDefault();
    if (stopBubble) e.stopPropagation();
    clearDropTargets();
    const sid = e.dataTransfer.getData(SESSION_MIME);
    if (!sessions.has(sid)) return; // ignore unknown/foreign payloads
    if (placed.includes(sid)) return; // already placed → not an append
    if (placed.length >= MAX_PANES) {
      setGridStatus(t("terminal-grid-full"));
      return;
    }
    if (placeSession(sid)) {
      // placeSession re-renders the toolbar itself; just announce here.
      announceGrid(sid);
    }
  });
}

// Body covers the empty grid (host hidden) and any non-pane gaps; the add-zone
// strip covers the tiled grid (it stops bubbling so it doesn't double-fire into
// the body). Pane drops stopPropagation, so a swap/replace over a pane never
// bubbles here and is never treated as an append.
wireAppendDrop(body);
wireAppendDrop(addZone, true);

/// Wire a pane element as a drop target (replace if source unplaced, swap
/// if source already placed).
function wirePaneDnd(paneEl, targetSessionId) {
  paneEl.addEventListener("dragover", (e) => {
    if (!dndHasSession(e)) return;
    e.preventDefault();
    e.stopPropagation();
    clearDropTargets();
    paneEl.classList.add("drop-target");
  });
  paneEl.addEventListener("dragleave", () => {
    paneEl.classList.remove("drop-target");
  });
  paneEl.addEventListener("drop", (e) => {
    if (!dndHasSession(e)) return;
    e.preventDefault();
    e.stopPropagation();
    const sid = e.dataTransfer.getData(SESSION_MIME);
    clearDropTargets();
    if (!sessions.has(sid)) return; // ignore unknown/foreign payloads
    if (sid === targetSessionId) return; // dropped onto itself — no-op
    handlePaneDrop(sid, targetSessionId);
  });
}

/// Resolve a drop onto a placed pane. If the source is already placed →
/// SWAP the two cells. If the source is unplaced → REPLACE: the target
/// drops back to tab-only and the source takes its cell (creating its
/// xterm on the spot if needed). Count is unchanged either way.
function handlePaneDrop(sourceSid, targetSid) {
  const targetIdx = placed.indexOf(targetSid);
  if (targetIdx === -1) return;

  const sourceIdx = placed.indexOf(sourceSid);
  if (sourceIdx !== -1) {
    // Placed → placed: swap positions, no eject.
    placed[sourceIdx] = targetSid;
    placed[targetIdx] = sourceSid;
    renderGrid();
    renderToolbar();
    announceGrid(sourceSid);
    return;
  }

  // Unplaced → placed pane: replace the cell. The displaced target keeps
  // its PTY/tab but loses its grid slot.
  const entry = sessions.get(sourceSid);
  if (!entry) return;

  // Lazily create the source's xterm if it has none yet (shared helper —
  // mirrors placeSession's first-placement path). Its hostEl stays a direct
  // child of #terminal-host; CSS `order` places it.
  if (!ensurePaneCreated(entry, sourceSid)) return;

  // Swap the slot: source takes the target's index, target goes tab-only.
  placed[targetIdx] = sourceSid;
  const displaced = sessions.get(targetSid);
  if (displaced?.hostEl) displaced.hostEl.hidden = true;
  if (focusedSessionId === targetSid) focusedSessionId = null;

  renderGrid();
  openFitAttach(entry, sourceSid);
  renderToolbar();
  announceGrid(sourceSid);
}

// Clear drop indicators and hide the add-zone whenever a drag ends anywhere
// (including outside a drop zone or on cancel) — dragend fires on the source
// even if no drop target handled it.
document.addEventListener("dragend", () => {
  clearDropTargets();
  if (addZone) addZone.hidden = true;
});

// ─────────────────────────── drag-to-resize ───────────────────────────
//
// The handle sits on the top edge of the drawer. While dragging, we
// disable the height transition (data-resizing="true") so the panel
// follows the cursor 1:1, and clamp the height between MIN_HEIGHT
// (just the header) and the viewport minus a small margin.

// A usable terminal minimum — NOT the old 38px header strip. Dragging the
// handle all the way down must never recreate the collapsed strip the
// full-hide rework removed; full dismissal is toggleDrawer(false) instead.
const MIN_HEIGHT = 160;
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
    fitPlaced();
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
// With the full-hide CSS, this now hides the drawer when open.
toggleBtn?.addEventListener("click", () => toggleDrawer());

// Triage modal — handles the queue of pending propostas surfaced when
// the agent calls `cadenza propose ...`. Opens automatically on the
// `proposta_pendente` event (emitted by the backend when a propose op
// lands) and also from the topbar badge.
//
// Three buttons: Aceitar / Rejeitar / Cancelar (close).
// `decidir_proposta` is called with the matching Decisao enum value;
// the backend resolves the await_decisao future and the CLI returns
// the appropriate exit code (0 / 20).

import { t } from "./i18n.js";
import { renderMarkdown } from "./markdown.js";

const { invoke } = window.__TAURI__.core;

const dialog = document.getElementById("triage-modal");
const emptyEl = document.getElementById("triage-empty");
const bodyEl = document.getElementById("triage-body");
const idBadge = document.getElementById("triage-id-badge");
const titleEl = document.getElementById("triage-title");
const parentEl = document.getElementById("triage-parent");
const fileEl = document.getElementById("triage-file");
const reproEl = document.getElementById("triage-repro");
const whatFailedEl = document.getElementById("triage-what-failed");
const actionEl = document.getElementById("triage-action");
const createdEl = document.getElementById("triage-created");
const statusEl = document.getElementById("triage-status");
const navEl = document.getElementById("triage-nav");
const counterEl = document.getElementById("triage-counter");
const prevBtn = document.getElementById("triage-prev");
const nextBtn = document.getElementById("triage-next");
const acceptBtn = document.getElementById("triage-accept");
const rejectBtn = document.getElementById("triage-reject");
const badgeBtn = document.getElementById("btn-triage");
const badgeCountEl = document.getElementById("triage-pending-count");

let queue = []; // Proposta[]
let cursor = 0;
let refreshBoard = null;

export function setRefreshBoard(fn) {
  refreshBoard = fn;
}

/**
 * Open the modal seeded with the current pending queue. If `propostaId`
 * is provided, jump to that entry; otherwise show the first one.
 */
export async function openTriage(propostaId) {
  await reloadQueue();
  if (queue.length === 0) {
    renderEmpty();
    if (!dialog.open) dialog.showModal();
    return;
  }
  cursor = 0;
  if (propostaId) {
    const idx = queue.findIndex((p) => p.proposta_id === propostaId);
    if (idx >= 0) cursor = idx;
  }
  renderCurrent();
  if (!dialog.open) dialog.showModal();
}

/** Refresh the topbar badge without opening the modal. */
export async function refreshPendingBadge() {
  try {
    const pending = await invoke("list_pending_propostas");
    setBadge(pending.length);
  } catch (e) {
    console.warn("list_pending_propostas failed", e);
    setBadge(0);
  }
}

function setBadge(count) {
  if (!badgeBtn || !badgeCountEl) return;
  badgeCountEl.textContent = String(count);
  badgeBtn.hidden = count === 0;
  if (count > 0) {
    const label = t("triage-pending-badge", { count });
    badgeBtn.setAttribute("aria-label", label);
    badgeBtn.setAttribute("title", label);
  }
}

async function reloadQueue() {
  try {
    queue = await invoke("list_pending_propostas");
  } catch (e) {
    setStatus(t("triage-load-error", { error: e }), "error");
    queue = [];
  }
  setBadge(queue.length);
}

function renderEmpty() {
  emptyEl.hidden = false;
  bodyEl.hidden = true;
  navEl.hidden = true;
  acceptBtn.disabled = true;
  rejectBtn.disabled = true;
  idBadge.hidden = true;
  setStatus("");
}

function renderCurrent() {
  emptyEl.hidden = true;
  bodyEl.hidden = false;
  acceptBtn.disabled = false;
  rejectBtn.disabled = false;
  setStatus("");

  const p = queue[cursor];
  idBadge.textContent = p.proposta_id;
  idBadge.hidden = false;
  titleEl.textContent = p.title ?? "";
  parentEl.textContent = p.parent ?? "—";
  fileEl.textContent = p.file ?? "—";
  renderMarkdown(reproEl, p.repro ?? "");
  renderMarkdown(whatFailedEl, p.what_failed ?? "");
  renderMarkdown(actionEl, p.action ?? "");
  createdEl.textContent = formatCreated(p.created_at_ms);

  if (queue.length > 1) {
    navEl.hidden = false;
    counterEl.textContent = `${cursor + 1} / ${queue.length}`;
    prevBtn.disabled = cursor === 0;
    nextBtn.disabled = cursor === queue.length - 1;
  } else {
    navEl.hidden = true;
  }
}

function formatCreated(ms) {
  if (!ms) return "—";
  try {
    return new Date(Number(ms)).toLocaleString();
  } catch {
    return String(ms);
  }
}

function setStatus(msg, kind) {
  statusEl.textContent = msg ?? "";
  statusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

function closeTriage() {
  if (dialog.open) dialog.close();
}

async function decide(decisao) {
  if (queue.length === 0) return;
  const p = queue[cursor];
  const registro = {
    proposta_id: p.proposta_id,
    decisao, // "aceita" | "rejeitada" | "mesclada"
    task_id: null,
    autor: "humano",
    decided_at_ms: Date.now(),
  };
  acceptBtn.disabled = true;
  rejectBtn.disabled = true;
  try {
    await invoke("decidir_proposta", { registro });
  } catch (e) {
    setStatus(t("triage-decided-error", { error: e }), "error");
    acceptBtn.disabled = false;
    rejectBtn.disabled = false;
    return;
  }
  // Drop the resolved item and advance.
  queue.splice(cursor, 1);
  if (cursor >= queue.length) cursor = Math.max(0, queue.length - 1);
  setBadge(queue.length);
  if (queue.length === 0) {
    renderEmpty();
    refreshBoard?.();
    return;
  }
  renderCurrent();
  refreshBoard?.();
}

// ─────────────────────────── event wiring ───────────────────────────

document
  .querySelectorAll('[data-action="close-triage"]')
  .forEach((b) => b.addEventListener("click", closeTriage));

acceptBtn.addEventListener("click", () => decide("aceita"));
rejectBtn.addEventListener("click", () => decide("rejeitada"));

prevBtn.addEventListener("click", () => {
  if (cursor > 0) {
    cursor -= 1;
    renderCurrent();
  }
});
nextBtn.addEventListener("click", () => {
  if (cursor < queue.length - 1) {
    cursor += 1;
    renderCurrent();
  }
});

badgeBtn?.addEventListener("click", () => openTriage());

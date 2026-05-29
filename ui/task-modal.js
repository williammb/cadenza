// Task create/edit modal. Two modes:
//   openNewTask(prefill?) — empty form, Save calls create_task.
//   openEditTask(id)      — read_task → fill form → Save patches the
//                            mutable surfaces (titulo, estado, body)
//                            one command at a time, since the backend
//                            exposes them as separate ops.
//
// IDs and `responsavel` are NOT user-facing — the form auto-generates
// the id on create and defaults responsavel to "humano". In edit mode
// the id is shown read-only as a badge in the header.
// Delete is hidden in create mode and visible in edit mode.

import { t } from "./i18n.js";
import { openStartAgent } from "./start-agent-modal.js";

const { invoke } = window.__TAURI__.core;

const DEFAULT_RESPONSAVEL = "humano";

const dialog = document.getElementById("task-modal");
const form = document.getElementById("task-form");
const titleEl = document.getElementById("task-modal-title");
const idBadge = document.getElementById("task-id-badge");
const tituloEl = document.getElementById("task-titulo");
const projectFieldEl = document.getElementById("task-project-field");
const projectEl = document.getElementById("task-project");
const estadoEl = document.getElementById("task-estado");
const bodyEl = document.getElementById("task-body");
const deleteBtn = document.getElementById("btn-delete-task");
const startBtn = document.getElementById("btn-start-task");
const statusEl = document.getElementById("task-status");

let mode = "create"; // "create" | "edit"
let editingId = null;
let original = null;
let onClosedRefresh = null;

export function setRefreshCallback(fn) {
  onClosedRefresh = fn;
}

export async function openNewTask(prefill = {}) {
  mode = "create";
  editingId = null;
  original = null;
  titleEl.textContent = t("task-modal-title-new");
  idBadge.hidden = true;
  idBadge.textContent = "";
  tituloEl.value = prefill.titulo ?? "";
  estadoEl.value = prefill.estado ?? "a_fazer";
  bodyEl.value = prefill.body ?? "";
  deleteBtn.hidden = true;
  startBtn.hidden = true;
  projectFieldEl.hidden = false;
  setStatus("");

  // Populate the project selector.
  let projects = [];
  try {
    const cfg = await invoke("get_config");
    projects = cfg?.projects ?? [];
  } catch (_) {}
  projectEl.replaceChildren();
  const placeholder = document.createElement("option");
  placeholder.value = "";
  placeholder.textContent = t("task-project-placeholder");
  projectEl.append(placeholder);
  for (const p of projects) {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = p.name;
    projectEl.append(opt);
  }
  projectEl.value = prefill.projectId ?? "";

  if (!dialog.open) dialog.showModal();
  tituloEl.focus();
}

export async function openEditTask(id) {
  mode = "edit";
  editingId = id;
  setStatus("");
  let task;
  try {
    task = await invoke("read_task", { id });
  } catch (e) {
    setStatus(t("task-error", { error: e }), "error");
    return;
  }
  original = task;
  titleEl.textContent = t("task-modal-title-edit");
  idBadge.textContent = task.id;
  idBadge.hidden = false;
  tituloEl.value = task.titulo ?? "";
  estadoEl.value = task.estado ?? "a_fazer";
  bodyEl.value = task.body ?? "";
  deleteBtn.hidden = false;
  startBtn.hidden = false;
  projectFieldEl.hidden = true;
  if (!dialog.open) dialog.showModal();
  tituloEl.focus();
}

export function closeTaskModal() {
  if (dialog.open) dialog.close();
}

function setStatus(msg, kind) {
  statusEl.textContent = msg ?? "";
  statusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

// ─────────────────────────── event wiring ───────────────────────────

document
  .querySelectorAll('[data-action="close-task"]')
  .forEach((b) => b.addEventListener("click", closeTaskModal));

// "Iniciar" in the modal header — close the edit modal so the two
// dialogs don't stack, then open the start-agent modal for the same
// task. The backend moves the task to `fazendo` AFTER a successful
// spawn, so we don't pre-flip the estado here anymore.
startBtn.addEventListener("click", () => {
  if (mode !== "edit" || !editingId) return;
  const id = editingId;
  const titulo = original?.titulo ?? tituloEl.value.trim();
  closeTaskModal();
  onClosedRefresh?.();
  openStartAgent(id, { titulo });
});

deleteBtn.addEventListener("click", async () => {
  if (mode !== "edit" || !editingId) return;
  if (!confirm(t("confirm-delete-task"))) return;
  try {
    await invoke("delete_task", { id: editingId });
    closeTaskModal();
    onClosedRefresh?.();
  } catch (e) {
    setStatus(t("task-error", { error: e }), "error");
  }
});

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  const titulo = tituloEl.value.trim();
  if (!titulo) {
    setStatus(t("task-error", { error: "titulo required" }), "error");
    return;
  }
  const estado = estadoEl.value;
  const body = bodyEl.value;

  if (mode === "create") {
    const projectId = projectEl.value || null;
    if (!projectId) {
      setStatus(t("task-project-required"), "error");
      return;
    }
    try {
      // Sequential id minted by the backend (T-1, T-2, ...) — readable
      // and stable across the on-disk format shared with task-ai (Node).
      const id = await invoke("next_task_id");
      await invoke("create_task", {
        task: { id, titulo, estado, responsavel: DEFAULT_RESPONSAVEL, body },
        projectId,
      });
      closeTaskModal();
      onClosedRefresh?.();
    } catch (err) {
      setStatus(t("task-error", { error: err }), "error");
    }
    return;
  }

  // edit mode — only push the surfaces that actually changed
  try {
    if (titulo !== original.titulo) {
      await invoke("set_titulo", { id: editingId, titulo });
    }
    if (estado !== original.estado) {
      await invoke("set_estado", { id: editingId, estado });
    }
    if (body !== (original.body ?? "")) {
      await invoke("update_task_body", { id: editingId, body });
    }
    closeTaskModal();
    onClosedRefresh?.();
  } catch (err) {
    setStatus(t("task-error", { error: err }), "error");
  }
});


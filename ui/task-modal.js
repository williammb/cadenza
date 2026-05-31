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
import { setupAttachments } from "./attachments.js";

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
const blockersListEl = document.getElementById("task-blockers-list");
const blockersEmptyEl = document.getElementById("task-blockers-empty");
const bodyEl = document.getElementById("task-body");
const deleteBtn = document.getElementById("btn-delete-task");
const startBtn = document.getElementById("btn-start-task");
const statusEl = document.getElementById("task-status");

// Worktree / branch section — edit mode only. Declarative: the user sets
// origin → destination + whether to use a worktree; the actual git (pull,
// branch create/switch, worktree create) runs at "Iniciar", server-side in
// start_task_agent. No per-action buttons here anymore.
const worktreeSection = document.getElementById("task-worktree-section");
const originBranchEl = document.getElementById("task-origin-branch");
const branchEl = document.getElementById("task-branch"); // destination
const branchListEl = document.getElementById("task-branch-list");
const useWorktreeEl = document.getElementById("task-use-worktree");
const worktreePathEl = document.getElementById("task-worktree-path");
const worktreePathField = document.getElementById("task-worktree-path-field");
const worktreeStatusEl = document.getElementById("worktree-status");

let mode = "create"; // "create" | "edit"
let editingId = null;
let original = null;
let onClosedRefresh = null;
let selectedBlockers = new Set();
let blockerLoadGen = 0;

// Image attachments: paste / drop / file button + Edit/Preview toggle.
// For a new task there's no id yet, so images are buffered and flushed to
// disk right after create mints the id.
const attachments = setupAttachments({
  textarea: bodyEl,
  preview: document.getElementById("task-body-preview-pane"),
  editBtn: document.getElementById("task-body-edit"),
  previewBtn: document.getElementById("task-body-preview-btn"),
  fileInput: document.getElementById("task-attach-input"),
  attachBtn: document.getElementById("task-attach-btn"),
  kind: "tasks",
  getOwnerId: () => (mode === "edit" ? editingId : null),
  onError: (msg) => setStatus(msg, "error"),
});
// Bumped on each worktree-defaults load so a stale in-flight response from a
// previously-opened task can't overwrite the fields of the task now open.
let worktreeLoadGen = 0;

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
  worktreeSection.hidden = true; // no task id yet → nothing to attach a worktree to
  attachments.reset();
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
  await loadBlockerChoices(prefill.blockedBy ?? []);

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
  worktreeSection.hidden = false;
  attachments.reset();
  await loadBlockerChoices(task.blocked_by ?? []);
  loadWorktreeDefaults(id);
  if (!dialog.open) dialog.showModal();
  tituloEl.focus();
}

function normalizeBlockerIds(ids) {
  const out = [];
  for (const raw of ids ?? []) {
    const id = String(raw ?? "").trim();
    if (!id || out.includes(id)) continue;
    out.push(id);
  }
  return out;
}

function sameIdList(a, b) {
  const aa = normalizeBlockerIds(a);
  const bb = normalizeBlockerIds(b);
  return aa.length === bb.length && aa.every((id, idx) => id === bb[idx]);
}

function readBlockedBy() {
  return [...selectedBlockers];
}

async function loadBlockerChoices(selected = readBlockedBy()) {
  const myGen = ++blockerLoadGen;
  selectedBlockers = new Set(normalizeBlockerIds(selected));
  blockersListEl.replaceChildren();
  blockersEmptyEl.hidden = false;
  try {
    const [tasks, mapping] = await Promise.all([
      invoke("list_tasks", { estado: null }),
      invoke("list_task_projects"),
    ]);
    if (myGen !== blockerLoadGen) return;
    const currentProject =
      mode === "create" ? projectEl.value || null : mapping?.[editingId] || null;
    const candidates = (tasks ?? []).filter((task) => {
      if (!task?.id || task.id === editingId) return false;
      return !currentProject || mapping?.[task.id] === currentProject;
    });
    renderBlockerChoices(candidates);
  } catch (e) {
    if (myGen !== blockerLoadGen) return;
    blockersEmptyEl.hidden = false;
    setStatus(t("task-error", { error: e }), "error");
  }
}

function renderBlockerChoices(tasks) {
  blockersListEl.replaceChildren();
  const shown = new Set();
  for (const task of tasks) {
    shown.add(task.id);
    blockersListEl.append(makeBlockerOption(task));
  }
  for (const id of selectedBlockers) {
    if (!shown.has(id)) {
      blockersListEl.append(makeBlockerOption({ id, titulo: id, estado: "" }, true));
    }
  }
  blockersEmptyEl.hidden = blockersListEl.children.length > 0;
}

function makeBlockerOption(task, stale = false) {
  const label = document.createElement("label");
  label.className = "blocker-option" + (stale ? " is-stale" : "");

  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = selectedBlockers.has(task.id);
  input.addEventListener("change", () => {
    if (input.checked) {
      selectedBlockers.add(task.id);
    } else {
      selectedBlockers.delete(task.id);
    }
  });

  const text = document.createElement("span");
  text.className = "blocker-option-text";
  const id = document.createElement("strong");
  id.textContent = task.id;
  const title = document.createElement("span");
  title.textContent = task.titulo ? ` ${task.titulo}` : "";
  text.append(id, title);

  const state = document.createElement("span");
  state.className = "blocker-state";
  state.textContent = task.estado ? t(`estado-${task.estado.replaceAll("_", "-")}`) : "";

  label.append(input, text, state);
  return label;
}

async function persistBlockersConfig(id) {
  await invoke("set_task_blockers", {
    taskId: id,
    blockedBy: readBlockedBy(),
  });
}

// Pre-fill the worktree section in one round-trip. Origin defaults to the
// project's configured default branch (else the repo's current branch);
// destination defaults to the stored branch or, on first setup, equals
// origin. The branch list populates both editable pickers. Git failures
// (e.g. the project isn't a git repo) leave the fields editable and just
// show a hint — they don't block editing the rest of the task.
async function loadWorktreeDefaults(id) {
  const myGen = ++worktreeLoadGen;
  setWorktreeStatus("");
  originBranchEl.value = "";
  branchEl.value = "";
  worktreePathEl.value = "";
  branchListEl.replaceChildren();
  // Reset to "no worktree" up front so a failed defaults load (below)
  // doesn't carry the previously-opened task's checkbox / path visibility
  // into this task.
  useWorktreeEl.checked = false;
  syncWorktreeMode();
  try {
    const d = await invoke("task_worktree_defaults", { taskId: id });
    if (myGen !== worktreeLoadGen) return; // a newer task was opened meanwhile
    // Populate the shared datalist with the repo's local branches.
    for (const name of d?.branches ?? []) {
      const opt = document.createElement("option");
      opt.value = name;
      branchListEl.append(opt);
    }
    const origin =
      d?.stored?.origin_branch || d?.default_branch || d?.current_branch || "";
    originBranchEl.value = origin;
    // Destination starts equal to origin until the user changes it.
    branchEl.value = d?.stored?.branch || origin;
    worktreePathEl.value =
      d?.stored?.worktree_path || d?.suggested_worktree_path || "";
    useWorktreeEl.checked = !!d?.stored?.use_worktree;
    syncWorktreeMode();
  } catch (e) {
    if (myGen !== worktreeLoadGen) return;
    setWorktreeStatus(t("task-worktree-defaults-error", { error: e }), "error");
  }
}

// Show the worktree path field only when "use worktree" is checked.
function syncWorktreeMode() {
  worktreePathField.hidden = !useWorktreeEl.checked;
}

// Persist the declarative branch/worktree config for the open task. Pure
// metadata — no git runs here; the workspace is prepared at "Iniciar".
async function persistWorktreeConfig(id) {
  await invoke("set_task_worktree", {
    taskId: id,
    originBranch: originBranchEl.value.trim() || null,
    branch: branchEl.value.trim() || null,
    useWorktree: useWorktreeEl.checked,
    worktreePath: useWorktreeEl.checked
      ? worktreePathEl.value.trim() || null
      : null,
  });
}

function setWorktreeStatus(msg, kind) {
  worktreeStatusEl.textContent = msg ?? "";
  worktreeStatusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
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
startBtn.addEventListener("click", async () => {
  if (mode !== "edit" || !editingId) return;
  const id = editingId;
  const titulo = original?.titulo ?? tituloEl.value.trim();
  // Persist the branch/worktree config first so the start-agent flow
  // prepares the workspace the user just configured. Blockers are also
  // persisted here because they affect whether execution may start.
  // A failure here is
  // surfaced in the section status rather than silently dropping the config.
  try {
    await persistBlockersConfig(id);
    await persistWorktreeConfig(id);
  } catch (e) {
    setStatus(t("task-error", { error: e }), "error");
    return;
  }
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

// ─────────────────── worktree / branch config (edit mode) ───────────────────
// The section is declarative now: changes are persisted on Save (see the
// form submit) and the git work happens at "Iniciar". The checkbox only
// gates the worktree path field's visibility.
useWorktreeEl.addEventListener("change", syncWorktreeMode);
projectEl.addEventListener("change", () => {
  if (mode === "create") loadBlockerChoices(readBlockedBy());
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
      // Persist any pasted/dropped images now that we have an id, and
      // rewrite the buffered tokens to their saved relative paths.
      const finalBody = await attachments.flush(id);
      await invoke("create_task", {
        task: {
          id,
          titulo,
          estado,
          responsavel: DEFAULT_RESPONSAVEL,
          body: finalBody,
          blocked_by: readBlockedBy(),
        },
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
    if (!sameIdList(readBlockedBy(), original.blocked_by ?? [])) {
      await persistBlockersConfig(editingId);
    }
    if (estado !== original.estado) {
      await invoke("set_estado", { id: editingId, estado });
    }
    if (body !== (original.body ?? "")) {
      await invoke("update_task_body", { id: editingId, body });
    }
    // Persist the declarative branch/worktree config (no git here).
    await persistWorktreeConfig(editingId);
    closeTaskModal();
    onClosedRefresh?.();
  } catch (err) {
    setStatus(t("task-error", { error: err }), "error");
  }
});


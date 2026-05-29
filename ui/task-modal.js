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

// Worktree / branch section — edit mode only.
const worktreeSection = document.getElementById("task-worktree-section");
const useWorktreeEl = document.getElementById("task-use-worktree");
const branchEl = document.getElementById("task-branch");
const worktreePathEl = document.getElementById("task-worktree-path");
const createWorktreeBtn = document.getElementById("btn-create-worktree");
const switchBranchBtn = document.getElementById("btn-switch-branch");
const removeWorktreeBtn = document.getElementById("btn-remove-worktree");
const worktreeStatusEl = document.getElementById("worktree-status");

let mode = "create"; // "create" | "edit"
let editingId = null;
let original = null;
let onClosedRefresh = null;
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
  worktreeSection.hidden = false;
  loadWorktreeDefaults(id);
  if (!dialog.open) dialog.showModal();
  tituloEl.focus();
}

// Pre-fill the worktree section in one round-trip. Branch defaults to the
// project's current branch (unless the task already has one stored); the
// worktree path defaults to the suggested sibling path. Git failures
// (e.g. the project isn't a git repo) leave the fields editable and just
// show a hint — they don't block editing the rest of the task.
async function loadWorktreeDefaults(id) {
  const myGen = ++worktreeLoadGen;
  setWorktreeStatus("");
  branchEl.value = "";
  worktreePathEl.value = "";
  // Reset to "no worktree" up front so a failed defaults load (below)
  // doesn't carry the previously-opened task's checkbox / control
  // visibility into this task.
  useWorktreeEl.checked = false;
  syncWorktreeMode();
  try {
    const d = await invoke("task_worktree_defaults", { taskId: id });
    if (myGen !== worktreeLoadGen) return; // a newer task was opened meanwhile
    branchEl.value = d?.stored?.branch || d?.current_branch || "";
    worktreePathEl.value =
      d?.stored?.worktree_path || d?.suggested_worktree_path || "";
    // "Using a worktree" is derived from whether one is actually stored —
    // there's no separate persisted flag. Creating/removing a worktree is
    // what flips this; the checkbox only gates which controls are shown.
    useWorktreeEl.checked = !!d?.stored?.worktree_path;
    syncWorktreeMode();
  } catch (e) {
    if (myGen !== worktreeLoadGen) return;
    setWorktreeStatus(t("task-worktree-defaults-error", { error: e }), "error");
  }
}

// Toggle the worktree-only controls (path field + create/remove) based on the
// "use worktree" checkbox. The branch field and "switch branch" stay visible
// either way: without a worktree, switching operates on the project repo.
function syncWorktreeMode() {
  const useWorktree = useWorktreeEl.checked;
  worktreePathEl.closest("label").hidden = !useWorktree;
  createWorktreeBtn.hidden = !useWorktree;
  removeWorktreeBtn.hidden = !useWorktree;
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

// ─────────────────── worktree / branch actions (edit mode) ───────────────────
// These run real git in the project repo and keep the modal open so the
// user can see the result/error and keep working with the branch fields.

useWorktreeEl.addEventListener("change", syncWorktreeMode);

createWorktreeBtn.addEventListener("click", async () => {
  if (mode !== "edit" || !editingId) return;
  const branch = branchEl.value.trim();
  const worktreePath = worktreePathEl.value.trim();
  if (!branch || !worktreePath) {
    setWorktreeStatus(t("task-worktree-fields-required"), "error");
    return;
  }
  setWorktreeStatus(t("task-worktree-working"));
  try {
    const info = await invoke("create_task_worktree", {
      taskId: editingId,
      branch,
      worktreePath,
    });
    branchEl.value = info?.branch ?? branch;
    worktreePathEl.value = info?.worktree_path ?? worktreePath;
    setWorktreeStatus(t("task-worktree-created"), "ok");
    onClosedRefresh?.();
  } catch (e) {
    setWorktreeStatus(t("task-worktree-error", { error: e }), "error");
  }
});

switchBranchBtn.addEventListener("click", async () => {
  if (mode !== "edit" || !editingId) return;
  const branch = branchEl.value.trim();
  if (!branch) {
    setWorktreeStatus(t("task-worktree-fields-required"), "error");
    return;
  }
  setWorktreeStatus(t("task-worktree-working"));
  try {
    const info = await invoke("switch_task_branch", {
      taskId: editingId,
      branch,
    });
    branchEl.value = info?.branch ?? branch;
    setWorktreeStatus(t("task-worktree-switched", { branch }), "ok");
    onClosedRefresh?.();
  } catch (e) {
    setWorktreeStatus(t("task-worktree-error", { error: e }), "error");
  }
});

removeWorktreeBtn.addEventListener("click", async () => {
  if (mode !== "edit" || !editingId) return;
  if (!confirm(t("confirm-remove-worktree"))) return;
  setWorktreeStatus(t("task-worktree-working"));
  try {
    await invoke("remove_task_worktree", { taskId: editingId });
    worktreePathEl.value = "";
    setWorktreeStatus(t("task-worktree-removed"), "ok");
    onClosedRefresh?.();
  } catch (e) {
    setWorktreeStatus(t("task-worktree-error", { error: e }), "error");
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


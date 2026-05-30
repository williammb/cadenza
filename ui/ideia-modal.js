// Ideia create/edit modal. Mirror do task-modal mas mais simples —
// ideia tem só (titulo, body, project), sem estado/responsavel.
//   openNewIdeia({ projectId }) — form vazio, Save → create_ideia.
//   openEditIdeia(id)           — read_ideia → preenche form. Save é
//                                  no-op (ideias não têm patch ops);
//                                  para mudar a ideia basta apagar e
//                                  criar uma nova. Por enquanto o modo
//                                  edit existe só para ver o conteúdo
//                                  e poder apagar via Delete.

import { t } from "./i18n.js";
import { setupAttachments } from "./attachments.js";

const { invoke } = window.__TAURI__.core;

const dialog = document.getElementById("ideia-modal");
const form = document.getElementById("ideia-form");
const titleEl = document.getElementById("ideia-modal-title");
const idBadge = document.getElementById("ideia-id-badge");
const tituloEl = document.getElementById("ideia-titulo");
const projectEl = document.getElementById("ideia-project");
const bodyEl = document.getElementById("ideia-body");
const deleteBtn = document.getElementById("btn-delete-ideia");
const statusEl = document.getElementById("ideia-status");

let mode = "create"; // "create" | "edit"
let editingId = null;
let onClosedRefresh = null;

// Image attachments. Ideias have no body-patch op, so on create we mint
// the id client-side (create_ideia accepts an optional id), flush the
// buffered images to that id, and pass the rewritten body straight into
// create. Edit mode is read-only, so the attach button is hidden there.
const attachments = setupAttachments({
  textarea: bodyEl,
  preview: document.getElementById("ideia-body-preview-pane"),
  editBtn: document.getElementById("ideia-body-edit"),
  previewBtn: document.getElementById("ideia-body-preview-btn"),
  fileInput: document.getElementById("ideia-attach-input"),
  attachBtn: document.getElementById("ideia-attach-btn"),
  kind: "ideias",
  getOwnerId: () => (mode === "edit" ? editingId : null),
  onError: (msg) => setStatus(msg, "error"),
});

export function setIdeiaRefreshCallback(fn) {
  onClosedRefresh = fn;
}

export async function openNewIdeia(prefill = {}) {
  mode = "create";
  editingId = null;
  titleEl.textContent = t("ideia-modal-title-new") || "Nova ideia";
  idBadge.hidden = true;
  idBadge.textContent = "";
  tituloEl.value = "";
  bodyEl.value = "";
  await populateProjects(prefill.projectId);
  deleteBtn.hidden = true;
  tituloEl.disabled = false;
  bodyEl.disabled = false;
  projectEl.disabled = false;
  attachments.reset();
  setStatus("");
  if (!dialog.open) dialog.showModal();
  tituloEl.focus();
}

export async function openEditIdeia(id) {
  mode = "edit";
  editingId = id;
  setStatus("");
  let ideia;
  try {
    ideia = await invoke("read_ideia", { id });
  } catch (e) {
    setStatus(t("task-error", { error: e }), "error");
    return;
  }
  if (!ideia) {
    setStatus(t("task-error", { error: "ideia not found" }), "error");
    return;
  }
  titleEl.textContent = t("ideia-modal-title-edit") || "Ideia";
  idBadge.textContent = ideia.id;
  idBadge.hidden = false;
  tituloEl.value = ideia.titulo ?? "";
  bodyEl.value = ideia.body ?? "";
  await populateProjects(ideia.project_id);
  // Por enquanto: ideias não têm patch ops, então em edit o form é
  // read-only. Para mudar a ideia o usuário apaga e cria de novo.
  tituloEl.disabled = true;
  bodyEl.disabled = true;
  projectEl.disabled = true;
  deleteBtn.hidden = false;
  // Read-only view: hide the attach button but keep the preview toggle so
  // the user can still see embedded images rendered.
  attachments.reset(true);
  if (!dialog.open) dialog.showModal();
}

export function closeIdeiaModal() {
  if (dialog.open) dialog.close();
}

async function populateProjects(preselected) {
  let cfg = null;
  try {
    cfg = await invoke("get_config");
  } catch (e) {
    console.warn("get_config in ideia-modal failed", e);
  }
  projectEl.replaceChildren();
  for (const p of cfg?.projects ?? []) {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = p.name;
    projectEl.append(opt);
  }
  if (preselected) {
    projectEl.value = preselected;
  }
}

function setStatus(msg, kind) {
  statusEl.textContent = msg ?? "";
  statusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

// ─────────────────────────── event wiring ───────────────────────────

document
  .querySelectorAll('[data-action="close-ideia"]')
  .forEach((b) => b.addEventListener("click", closeIdeiaModal));

deleteBtn.addEventListener("click", async () => {
  if (mode !== "edit" || !editingId) return;
  if (!confirm(t("confirm-delete-ideia") || "Excluir esta ideia?")) return;
  try {
    await invoke("delete_ideia", { id: editingId });
    closeIdeiaModal();
    onClosedRefresh?.();
  } catch (e) {
    setStatus(t("task-error", { error: e }), "error");
  }
});

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (mode !== "create") {
    // Edit mode é read-only por design — apenas fecha.
    closeIdeiaModal();
    return;
  }
  const titulo = tituloEl.value.trim();
  if (!titulo) {
    setStatus(t("task-error", { error: "titulo required" }), "error");
    return;
  }
  const projectId = projectEl.value;
  if (!projectId) {
    setStatus(t("ideia-project-required") || "Selecione um projeto.", "error");
    return;
  }
  try {
    // Mint the id up front so buffered images can be saved under it and
    // the body refs rewritten before the ideia row is created (ideias
    // have no body-patch op to apply afterwards). Matches the backend's
    // `I-<simple-uuid>` shape.
    const id = "I-" + crypto.randomUUID().replaceAll("-", "");
    const finalBody = await attachments.flush(id);
    await invoke("create_ideia", {
      // Campos da struct viajam como serde os espera (snake_case) —
      // só os nomes de parâmetro top-level do #[tauri::command] são
      // auto-convertidos de camelCase pelo Tauri.
      args: { id, titulo, body: finalBody, project_id: projectId },
    });
    closeIdeiaModal();
    onClosedRefresh?.();
  } catch (err) {
    setStatus(t("task-error", { error: err }), "error");
  }
});

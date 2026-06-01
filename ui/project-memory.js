// Project shared memory — the Memory section inside Settings → Projeto.
//
// The user is the curator: they edit the official memory items manually,
// trigger a reevaluation agent, and approve/reject its reeval suggestions.
// Learning suggestions (proposed by the execution agent) are NOT shown
// here — they live in the task review modal. Nothing an agent proposes
// enters memory without an explicit click here.
//
// Source of truth is the Cadenza store (per project); this module just
// reads/writes through Tauri commands and re-renders on `memory_changed`.

import { t } from "./i18n.js";
import { openStartAgent } from "./start-agent-modal.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const sectionEl = document.getElementById("project-memory-section");
const itemsListEl = document.getElementById("memory-items-list");
const itemsEmptyEl = document.getElementById("memory-items-empty");
const newTextEl = document.getElementById("memory-new-text");
const addBtn = document.getElementById("btn-memory-add");
const reevalBtn = document.getElementById("btn-memory-reeval");
const suggestionsListEl = document.getElementById("memory-suggestions-list");
const suggestionsEmptyEl = document.getElementById("memory-suggestions-empty");
const statusEl = document.getElementById("memory-status");

let projectId = null;

function setStatus(msg, kind = "") {
  statusEl.textContent = msg ?? "";
  statusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

// Render the section for `id`. Pass null to hide it (no project selected).
export async function renderProjectMemory(id) {
  projectId = id;
  if (!id) {
    sectionEl.hidden = true;
    return;
  }
  sectionEl.hidden = false;
  setStatus("");
  await refresh();
}

async function refresh() {
  if (!projectId) return;
  let items = [];
  let suggestions = [];
  try {
    [items, suggestions] = await Promise.all([
      invoke("get_project_memory", { projectId }),
      invoke("list_memory_suggestions", { projectId }),
    ]);
  } catch (e) {
    setStatus(typeof e === "string" ? e : String(e), "error");
    return;
  }
  renderItems(items);
  // Only reeval ops belong here; "aprendizado" suggestions surface in the
  // task review modal instead.
  renderSuggestions(
    suggestions.filter((s) => s.kind?.tipo && s.kind.tipo !== "aprendizado"),
    new Map(items.map((it) => [it.id, it.texto])),
  );
}

function renderItems(items) {
  itemsListEl.replaceChildren();
  itemsEmptyEl.hidden = items.length > 0;
  for (const item of items) {
    itemsListEl.append(makeItemRow(item));
  }
}

function makeItemRow(item) {
  const li = document.createElement("li");
  li.className = "memory-item";

  const text = document.createElement("span");
  text.className = "memory-item-text";
  text.textContent = item.texto;
  li.append(text);

  if (item.origem_task) {
    const origin = document.createElement("span");
    origin.className = "memory-item-origin";
    origin.textContent = item.origem_task;
    li.append(origin);
  }

  const actions = document.createElement("div");
  actions.className = "memory-item-actions";

  const editBtn = document.createElement("button");
  editBtn.type = "button";
  editBtn.className = "btn btn-sm";
  editBtn.textContent = t("action-edit") || "Editar";
  editBtn.addEventListener("click", () => startEdit(li, item));

  const delBtn = document.createElement("button");
  delBtn.type = "button";
  delBtn.className = "btn btn-sm btn-danger";
  delBtn.textContent = t("action-delete") || "Excluir";
  delBtn.addEventListener("click", () => removeItem(item.id));

  actions.append(editBtn, delBtn);
  li.append(actions);
  return li;
}

// Swap the row for an inline editor (textarea + Salvar/Cancelar).
function startEdit(li, item) {
  li.replaceChildren();
  li.className = "memory-item memory-item-editing";

  const ta = document.createElement("textarea");
  ta.rows = 2;
  ta.value = item.texto;
  li.append(ta);

  const actions = document.createElement("div");
  actions.className = "memory-item-actions";

  const saveBtn = document.createElement("button");
  saveBtn.type = "button";
  saveBtn.className = "btn btn-sm btn-primary";
  saveBtn.textContent = t("action-save") || "Salvar";
  saveBtn.addEventListener("click", async () => {
    const texto = ta.value.trim();
    if (!texto) return;
    try {
      await invoke("update_memory_item", {
        projectId,
        itemId: item.id,
        texto,
      });
      // refresh comes from the memory_changed event, but call it too in
      // case events are delayed.
      await refresh();
    } catch (e) {
      setStatus(typeof e === "string" ? e : String(e), "error");
    }
  });

  const cancelBtn = document.createElement("button");
  cancelBtn.type = "button";
  cancelBtn.className = "btn btn-sm";
  cancelBtn.textContent = t("action-cancel") || "Cancelar";
  cancelBtn.addEventListener("click", () => refresh());

  actions.append(saveBtn, cancelBtn);
  li.append(actions);
  ta.focus();
}

async function removeItem(itemId) {
  try {
    await invoke("delete_memory_item", { projectId, itemId });
    await refresh();
  } catch (e) {
    setStatus(typeof e === "string" ? e : String(e), "error");
  }
}

function renderSuggestions(suggestions, itemTextById) {
  suggestionsListEl.replaceChildren();
  suggestionsEmptyEl.hidden = suggestions.length > 0;
  for (const s of suggestions) {
    suggestionsListEl.append(makeSuggestionRow(s, itemTextById));
  }
}

// Human-readable description of a reeval op, resolving target ids to
// their current text where it helps.
function describeKind(kind, itemTextById) {
  const txt = (id) => itemTextById.get(id) || id;
  switch (kind.tipo) {
    case "remover":
      return `${t("settings-memory-op-remover") || "Remover"}: ${txt(kind.target_id)}`;
    case "reescrever":
      return `${t("settings-memory-op-reescrever") || "Reescrever"}: ${txt(kind.target_id)} → ${kind.novo_texto}`;
    case "mesclar":
      return `${t("settings-memory-op-mesclar") || "Mesclar"} (${kind.target_ids.length}) → ${kind.texto_mesclado}`;
    case "nova":
      return `${t("settings-memory-op-nova") || "Novo item"}: ${kind.texto}`;
    case "contradicao":
      return `${t("settings-memory-op-contradicao") || "Contradição"}: ${kind.nota}`;
    default:
      return kind.tipo;
  }
}

function makeSuggestionRow(s, itemTextById) {
  const li = document.createElement("li");
  li.className = "memory-suggestion";

  const desc = document.createElement("span");
  desc.className = "memory-suggestion-desc";
  desc.textContent = describeKind(s.kind, itemTextById);
  li.append(desc);

  const actions = document.createElement("div");
  actions.className = "memory-suggestion-actions";

  // Contradiction is informational: approving changes nothing, so offer
  // only "Descartar". Every other op gets Aprovar + Rejeitar.
  const informational = s.kind.tipo === "contradicao";
  if (!informational) {
    const approve = document.createElement("button");
    approve.type = "button";
    approve.className = "btn btn-sm btn-primary";
    approve.textContent = t("settings-memory-approve") || "Aprovar";
    approve.addEventListener("click", () => resolve(s.id, true));
    actions.append(approve);
  }

  const reject = document.createElement("button");
  reject.type = "button";
  reject.className = "btn btn-sm";
  reject.textContent = informational
    ? t("settings-memory-dismiss") || "Descartar"
    : t("settings-memory-reject") || "Rejeitar";
  reject.addEventListener("click", () => resolve(s.id, false));
  actions.append(reject);

  li.append(actions);
  return li;
}

async function resolve(suggestionId, aprovar) {
  try {
    await invoke("resolve_memory_suggestion", { suggestionId, aprovar });
    await refresh();
  } catch (e) {
    setStatus(typeof e === "string" ? e : String(e), "error");
  }
}

addBtn.addEventListener("click", async () => {
  const texto = newTextEl.value.trim();
  if (!texto || !projectId) return;
  try {
    await invoke("add_memory_item", { projectId, texto });
    newTextEl.value = "";
    await refresh();
  } catch (e) {
    setStatus(typeof e === "string" ? e : String(e), "error");
  }
});

reevalBtn.addEventListener("click", () => {
  if (!projectId) return;
  // Reuse the start-agent modal (memory mode) to pick agent + model and
  // spawn the reeval PTY against this project.
  openStartAgent(projectId, { mode: "memory" });
});

// Re-render live when suggestions/items change (e.g. the reeval agent
// just emitted a `memory revise`, or another window edited memory).
// The payload is a bare project-id string from the Tauri command path
// and an object `{ project_id }` from the IPC path — handle both.
listen("memory_changed", (e) => {
  const pid =
    typeof e?.payload === "string" ? e.payload : e?.payload?.project_id;
  if (!sectionEl.hidden && projectId && (!pid || pid === projectId)) {
    refresh();
  }
}).catch(() => {});

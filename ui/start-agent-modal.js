// Start-agent modal — picks platform (Claude Code / Codex) + model and
// spawns the agent CLI in a PTY by calling `start_task_agent`. On
// success it attaches the terminal drawer to the new session.
//
// Behaviour:
//   - If the task has a saved `task-runs` entry whose `agent` matches
//     the current selection AND has a `conversation_id`, the modal
//     shows a "Continuar conversa <id>" banner and the submit button
//     reads "Continuar". A small "Iniciar nova" button on the banner
//     wipes the saved run (clear_task_run) so the next submit becomes
//     a fresh session.
//   - "Outro…" in the model dropdown reveals a free-text input.
//
// Per-agent model catalogues are kept inline; updating them is a
// one-file change.

import { t } from "./i18n.js";
import { attachTerminal } from "./terminal.js";

const { invoke } = window.__TAURI__.core;

const MODELS = {
  claude_code: [
    { id: "claude-opus-4-7", labelKey: "model-claude-opus-4-7", fallback: "Claude Opus 4.7" },
    { id: "claude-sonnet-4-6", labelKey: "model-claude-sonnet-4-6", fallback: "Claude Sonnet 4.6" },
    { id: "claude-haiku-4-5", labelKey: "model-claude-haiku-4-5", fallback: "Claude Haiku 4.5" },
  ],
  codex: [
    { id: "gpt-5.5", labelKey: "model-gpt-5-5", fallback: "gpt-5.5" },
    { id: "gpt-5.4", labelKey: "model-gpt-5-4", fallback: "gpt-5.4" },
    { id: "gpt-5.4-mini", labelKey: "model-gpt-5-4-mini", fallback: "gpt-5.4-mini" },
    { id: "gpt-5.3-codex", labelKey: "model-gpt-5-3-codex", fallback: "gpt-5.3-codex" },
    { id: "gpt-5.3-codex-spark", labelKey: "model-gpt-5-3-codex-spark", fallback: "gpt-5.3-codex-spark" },
    { id: "gpt-5.2", labelKey: "model-gpt-5-2", fallback: "gpt-5.2" },
  ],
};

const OTHER_VALUE = "__other__";

const dialog = document.getElementById("start-agent-modal");
const form = document.getElementById("start-agent-form");
const kindSel = document.getElementById("start-agent-kind");
const modelSel = document.getElementById("start-agent-model");
const otherField = document.getElementById("start-agent-model-other-field");
const otherInput = document.getElementById("start-agent-model-other");
const taskBadge = document.getElementById("start-agent-task-badge");
const resumeBanner = document.getElementById("start-agent-resume-banner");
const resumeIdEl = document.getElementById("start-agent-resume-id");
const freshBtn = document.getElementById("btn-start-agent-fresh");
const submitBtn = document.getElementById("btn-start-agent-submit");
const statusEl = document.getElementById("start-agent-status");

let currentTaskId = null;
let currentTitulo = null;
let currentRun = null; // task-run record from backend (or null)
// "task" (default): chama start_task_agent + suporta resume.
// "ideia": chama destrinchar_ideia, sem resume (cada decomposição é
// one-shot — não há conversa antiga para continuar).
let currentMode = "task";
let onSpawnedRefresh = null;

export function setStartAgentRefreshCallback(fn) {
  onSpawnedRefresh = fn;
}

export async function openStartAgent(targetId, opts = {}) {
  currentTaskId = targetId;
  currentTitulo = opts.titulo ?? null;
  currentMode = opts.mode === "ideia" ? "ideia" : "task";
  taskBadge.textContent = targetId;
  setStatus("");

  // Default kind comes from Config.agente.kind. read_task_run is só
  // para tasks — ideias não têm run associado.
  const [config, run] = await Promise.all([
    invoke("get_config").catch(() => null),
    currentMode === "task"
      ? invoke("read_task_run", { taskId: targetId }).catch(() => null)
      : Promise.resolve(null),
  ]);
  currentRun = run;

  const defaultKind = run?.agent ?? config?.agente?.kind ?? "claude_code";
  kindSel.value = defaultKind;
  populateModels(defaultKind, run?.model);
  updateResumeBanner();

  if (!dialog.open) dialog.showModal();
}

export function closeStartAgent() {
  if (dialog.open) dialog.close();
}

function populateModels(kind, preselectedId) {
  modelSel.replaceChildren();
  const list = MODELS[kind] ?? [];
  let foundPreselected = false;
  for (const m of list) {
    const opt = document.createElement("option");
    opt.value = m.id;
    opt.textContent = t(m.labelKey, {}) === m.labelKey ? m.fallback : t(m.labelKey);
    if (preselectedId === m.id) {
      opt.selected = true;
      foundPreselected = true;
    }
    modelSel.append(opt);
  }
  const otherOpt = document.createElement("option");
  otherOpt.value = OTHER_VALUE;
  otherOpt.textContent = t("start-agent-model-other") || "Outro…";
  modelSel.append(otherOpt);

  // If preselectedId isn't in the catalogue (custom model from a prior
  // run), reveal the other-field with that value.
  if (preselectedId && !foundPreselected) {
    modelSel.value = OTHER_VALUE;
    otherField.hidden = false;
    otherInput.value = preselectedId;
  } else {
    otherField.hidden = true;
    otherInput.value = "";
  }
}

function updateResumeBanner() {
  const canResume =
    currentRun &&
    currentRun.agent === kindSel.value &&
    typeof currentRun.conversation_id === "string" &&
    currentRun.conversation_id.length > 0;
  if (canResume) {
    resumeBanner.hidden = false;
    resumeIdEl.textContent = shortenId(currentRun.conversation_id);
    submitBtn.textContent = t("start-agent-action-resume") || "Continuar";
  } else {
    resumeBanner.hidden = true;
    submitBtn.textContent = t("start-agent-action-start") || "Iniciar";
  }
}

function shortenId(id) {
  if (id.length <= 12) return id;
  return id.slice(0, 8) + "…";
}

function readModel() {
  const v = modelSel.value;
  if (v === OTHER_VALUE) {
    return otherInput.value.trim();
  }
  return v;
}

function setStatus(msg, kind) {
  statusEl.textContent = msg ?? "";
  statusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

// ─────────────────────────── event wiring ───────────────────────────

kindSel.addEventListener("change", () => {
  populateModels(kindSel.value, currentRun?.agent === kindSel.value ? currentRun.model : null);
  updateResumeBanner();
});

modelSel.addEventListener("change", () => {
  otherField.hidden = modelSel.value !== OTHER_VALUE;
  if (modelSel.value === OTHER_VALUE) otherInput.focus();
});

freshBtn.addEventListener("click", async () => {
  if (!currentTaskId) return;
  if (!confirm(t("start-agent-fresh-confirm") || "Apagar conversa salva e iniciar uma nova?")) return;
  try {
    await invoke("clear_task_run", { taskId: currentTaskId });
    currentRun = null;
    updateResumeBanner();
  } catch (e) {
    setStatus(t("task-error", { error: e }), "error");
  }
});

document
  .querySelectorAll('[data-action="close-start-agent"]')
  .forEach((b) => b.addEventListener("click", closeStartAgent));

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (!currentTaskId) return;
  const model = readModel();
  if (!model) {
    setStatus(t("start-agent-model-required") || "Escolha um modelo.", "error");
    return;
  }
  const agentKind = kindSel.value;
  submitBtn.disabled = true;
  setStatus(t("start-agent-launching") || "Iniciando agente…");
  try {
    const result =
      currentMode === "ideia"
        ? await invoke("destrinchar_ideia", {
            ideiaId: currentTaskId,
            agentKind,
            model,
          })
        : await invoke("start_task_agent", {
            taskId: currentTaskId,
            agentKind,
            model,
          });
    await attachTerminal(result.session_id, {
      taskId: currentTaskId,
      title: currentTitulo,
    });
    closeStartAgent();
    // Backend may have moved the task to `fazendo` as part of the
    // spawn — re-render the board so the card reflects that.
    onSpawnedRefresh?.();
  } catch (err) {
    setStatus(typeof err === "string" ? err : t("task-error", { error: err }), "error");
  } finally {
    submitBtn.disabled = false;
  }
});

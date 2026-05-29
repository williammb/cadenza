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
//   - The model dropdown is populated by invoking `list_agent_models`,
//     which spawns the agent CLI under a PTY and parses its `/model`
//     menu. Result is cached backend-side per process. While the call
//     is in flight, the dropdown is disabled and shows a placeholder.

import { t } from "./i18n.js";
import { attachTerminal } from "./terminal.js";
import {
  loadAgentPresence,
  decorateKindSelect,
  onAgentPresenceRefresh,
} from "./agent-presence.js";

const { invoke } = window.__TAURI__.core;

const dialog = document.getElementById("start-agent-modal");
const form = document.getElementById("start-agent-form");
const kindSel = document.getElementById("start-agent-kind");
const modelSel = document.getElementById("start-agent-model");
const modelText = document.getElementById("start-agent-model-text");
const modelHint = document.getElementById("start-agent-model-hint");
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
  updateResumeBanner();
  await applyAgentPresence();
  if (!dialog.open) dialog.showModal();
  await populateModels(defaultKind, run?.model);
}

async function applyAgentPresence() {
  // Force a fresh probe on open so an agent installed since boot is
  // detected — the cache otherwise persists for the whole session and
  // the submit-time hard-block below would keep refusing it.
  const map = await loadAgentPresence({ force: true });
  decorateKindSelect(kindSel, map);
}

// Re-decorate the kind select if the locale flips while the modal is
// open — translations overwrite the option text, wiping the "(not
// installed)" suffix added by decorateKindSelect.
onAgentPresenceRefresh(() => {
  if (dialog.open) applyAgentPresence();
});

export function closeStartAgent() {
  if (dialog.open) dialog.close();
}

// Each (modal-open, kind) starts a fresh discovery generation. If the
// user flips the kind selector while a slow discovery is in flight, the
// in-flight result for the old kind must not stomp the dropdown
// belonging to the new kind. We track that via this monotonic counter.
let modelLoadGen = 0;

async function populateModels(kind, preselectedId) {
  const myGen = ++modelLoadGen;
  submitBtn.disabled = true;

  // Read-only, cached lookup — no PTY probe, so this is instant. Model
  // discovery lives in Settings → Modelos (see settings.js); the task
  // modal never triggers the slow ~15 s spawn.
  let entries;
  try {
    entries = await invoke("list_agent_models", { agentKind: kind, cachedOnly: true });
  } catch {
    entries = [];
  }
  if (myGen !== modelLoadGen) return;

  if (!entries.length) {
    // Nothing loaded for this platform yet — fall back to a free-text
    // model id so the user is never blocked, and point them at Settings.
    showModelText(preselectedId ?? "");
    submitBtn.disabled = false;
    setStatus("");
    return;
  }

  showModelSelect();
  modelSel.replaceChildren();
  let foundPreselected = false;
  for (const m of entries) {
    const opt = document.createElement("option");
    opt.value = m.id;
    opt.textContent = m.label || m.id;
    if (preselectedId === m.id) {
      opt.selected = true;
      foundPreselected = true;
    } else if (!preselectedId && m.current) {
      // Mirror the agent's own current selection when we have no
      // saved-run hint — matches the value the agent would pick if
      // invoked without `--model`, so the UI default tracks the CLI.
      opt.selected = true;
    }
    modelSel.append(opt);
  }
  // Preselected id (from a prior run) is no longer offered — keep it
  // as a sticky option so the user can still resume on the same model
  // without losing the choice, but tag it so they know it's stale.
  if (preselectedId && !foundPreselected) {
    const opt = document.createElement("option");
    opt.value = preselectedId;
    opt.textContent = `${preselectedId} (${t("start-agent-model-saved") || "salvo"})`;
    opt.selected = true;
    modelSel.append(opt);
  }
  modelSel.disabled = false;
  submitBtn.disabled = false;
  setStatus("");
}

// Show the dropdown (models are loaded); hide the free-text fallback.
function showModelSelect() {
  modelSel.hidden = false;
  modelSel.disabled = false;
  modelText.hidden = true;
  modelHint.hidden = true;
}

// Show the free-text id input + the "load them in Settings" hint, and
// hide the (empty) dropdown.
function showModelText(value) {
  modelSel.replaceChildren();
  modelSel.hidden = true;
  modelSel.disabled = true;
  modelText.value = value || "";
  modelText.hidden = false;
  modelHint.hidden = false;
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
  return (modelText.hidden ? modelSel.value : modelText.value.trim()) || "";
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
  // An empty model is allowed: when nothing is loaded (free-text fallback
  // left blank) we start without a model so the agent falls back to its
  // own configured default instead of blocking the user — a safety net if
  // model discovery is broken. The backend omits the --model/-m flag when
  // the string is empty (see agent::plan_claude_launch / plan_codex_launch).
  const model = readModel();
  const agentKind = kindSel.value;
  // Hard-block when the picked agent isn't installed. The dropdown
  // already disables non-installed options, but the user's saved
  // default may itself be non-installed (we don't silently change it).
  const presence = (await loadAgentPresence()).get(agentKind);
  if (presence && !presence.installed) {
    setStatus(t("settings-agent-not-installed-tooltip"), "error");
    return;
  }
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

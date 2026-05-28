// Bootstrap script. Wires the board (drag-and-drop + render) and the
// topbar buttons. Modals live in settings.js and task-modal.js — this
// file just opens them. Backend event listeners trigger a re-render.

import { bootI18n, t, onLocaleChange } from "./i18n.js";
import { openSettings, setSettingsRefreshCallback } from "./settings.js";
import {
  openNewTask,
  openEditTask,
  setRefreshCallback,
} from "./task-modal.js";
import {
  openTriage,
  refreshPendingBadge,
  setRefreshBoard as setTriageRefresh,
} from "./triage-modal.js";
import {
  openNewIdeia,
  openEditIdeia,
  setIdeiaRefreshCallback,
} from "./ideia-modal.js";
import { initTheme, toggleTheme } from "./theme.js";
import {
  openStartAgent,
  setStartAgentRefreshCallback,
} from "./start-agent-modal.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ESTADOS = ["a_fazer", "fazendo", "aguardando_revisao", "feito"];

// Cached so the board can re-filter without round-tripping to disk on
// every project-selector change. Repopulated on every renderBoard().
let cachedTaskProjects = {};
let cachedActiveProject = null;
// Shown once per session when no projects exist, so the user is guided
// to add a first project without reopening settings on every re-render.
let _guidedToFirstProject = false;
// task_id → task-run record from list_task_runs. Used to mark cards
// that have a saved conversation so the user knows "click ▶ = resume".
let cachedTaskRuns = {};

async function renderBoard() {
  let tasks = [];
  let ideias = [];
  let mapping = {};
  let cfg = null;
  let runs = {};
  try {
    [tasks, ideias, mapping, cfg, runs] = await Promise.all([
      invoke("list_tasks", { estado: null }),
      invoke("list_ideias").catch(() => []),
      invoke("list_task_projects"),
      invoke("get_config"),
      invoke("list_task_runs").catch(() => ({})),
    ]);
  } catch (e) {
    setStatus(`error: ${e}`);
    return;
  }
  cachedTaskProjects = mapping ?? {};
  cachedTaskRuns = runs ?? {};
  cachedActiveProject = cfg?.active_project_id ?? null;
  renderProjectOptions(cfg?.projects ?? [], cachedActiveProject);

  // First launch: no projects yet — guide the user to add one.
  if ((cfg?.projects ?? []).length === 0 && !_guidedToFirstProject) {
    _guidedToFirstProject = true;
    openSettings();
  }

  // Filter by project before bucketing so the per-column counts also
  // reflect the active project — otherwise "FAZENDO 0" would be a lie
  // when there are tasks from other projects in that state.
  if (cachedActiveProject) {
    tasks = tasks.filter((t) => cachedTaskProjects[t.id] === cachedActiveProject);
    ideias = ideias.filter((i) => i.project_id === cachedActiveProject);
  }
  // Esconder ideias já destrinchadas — saíram do estágio "pendente".
  // Arquivadas também ficam ocultas. Mantém a Inbox focada no que
  // precisa de atenção.
  ideias = ideias.filter((i) => i.status === "pendente");

  const buckets = Object.fromEntries(ESTADOS.map((s) => [s, []]));
  for (const task of tasks) {
    if (buckets[task.estado]) buckets[task.estado].push(task);
  }

  for (const estado of ESTADOS) {
    const list = document.querySelector(
      `.column[data-estado="${estado}"] .cards`,
    );
    if (!list) continue;
    list.replaceChildren();
    if (buckets[estado].length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty";
      empty.textContent = t("board-empty");
      list.append(empty);
    } else {
      for (const task of buckets[estado]) {
        list.append(makeCard(task));
      }
    }
    const counter = document.querySelector(`[data-count-for="${estado}"]`);
    if (counter) counter.textContent = String(buckets[estado].length);
  }

  // Inbox column.
  const inboxList = document.querySelector('.column-inbox .cards');
  if (inboxList) {
    inboxList.replaceChildren();
    if (ideias.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty";
      empty.textContent = t("ideia-empty") || t("board-empty");
      inboxList.append(empty);
    } else {
      for (const ideia of ideias) {
        inboxList.append(makeIdeiaCard(ideia));
      }
    }
    const counter = document.querySelector(`[data-count-for="inbox"]`);
    if (counter) counter.textContent = String(ideias.length);
  }

  setStatus("");
}

function makeCard(task) {
  const card = document.createElement("div");
  card.className = "card";
  card.draggable = true;
  card.dataset.id = task.id;

  const title = document.createElement("strong");
  title.textContent = task.titulo ?? task.id;
  const id = document.createElement("small");
  id.textContent = task.id;

  // Start/resume button — visible on every card. Enabled in any state
  // except `feito`. The backend transitions to `fazendo` AFTER a
  // successful spawn; we no longer pre-flip the estado here, so a
  // failed/cancelled start leaves the card in its original column.
  const startBtn = document.createElement("button");
  startBtn.type = "button";
  startBtn.className = "btn btn-icon card-start";
  startBtn.textContent = "▶";
  const hasRun = !!cachedTaskRuns[task.id];
  if (hasRun) startBtn.classList.add("has-run");
  startBtn.title = hasRun
    ? t("card-start-resume-aria")
    : t("card-start-aria");
  startBtn.setAttribute(
    "aria-label",
    hasRun ? t("card-start-resume-aria") : t("card-start-aria"),
  );
  startBtn.disabled = task.estado === "feito";
  startBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (startBtn.disabled) return;
    openStartAgent(task.id, { titulo: task.titulo });
  });
  // Prevent button drag from also dragging the card.
  startBtn.addEventListener("dragstart", (e) => e.preventDefault());

  card.append(title, id, startBtn);

  card.addEventListener("dragstart", (e) => {
    e.dataTransfer.setData("text/plain", task.id);
    e.dataTransfer.effectAllowed = "move";
    card.classList.add("dragging");
  });
  card.addEventListener("dragend", () => card.classList.remove("dragging"));
  card.addEventListener("dblclick", () => openEditTask(task.id));
  return card;
}

function makeIdeiaCard(ideia) {
  const card = document.createElement("div");
  card.className = "card card-ideia";
  card.dataset.id = ideia.id;

  const title = document.createElement("strong");
  title.textContent = ideia.titulo ?? ideia.id;
  const id = document.createElement("small");
  id.textContent = ideia.id;

  // Botão "Destrinchar" — substitui o ▶ (start agent) das tasks.
  // Abre o start-agent-modal em modo "ideia" → backend roda
  // destrinchar_ideia em vez de start_task_agent.
  const splitBtn = document.createElement("button");
  splitBtn.type = "button";
  splitBtn.className = "btn btn-icon card-start";
  splitBtn.textContent = "✦";
  splitBtn.title = t("ideia-destrinchar") || "Destrinchar em tasks";
  splitBtn.setAttribute(
    "aria-label",
    t("ideia-destrinchar") || "Destrinchar em tasks",
  );
  splitBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    openStartAgent(ideia.id, { mode: "ideia", titulo: ideia.titulo });
  });

  card.append(title, id, splitBtn);
  card.addEventListener("dblclick", () => openEditIdeia(ideia.id));
  return card;
}

function wireDropZones() {
  document.querySelectorAll("[data-drop]").forEach((zone) => {
    zone.addEventListener("dragover", (e) => {
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      zone.classList.add("drop-target");
    });
    zone.addEventListener("dragleave", (e) => {
      // dragleave fires every time the cursor crosses into a child
      // element (empty placeholder, cards) — gating on relatedTarget
      // prevents the highlight from flickering off mid-drag.
      if (!zone.contains(e.relatedTarget)) {
        zone.classList.remove("drop-target");
      }
    });
    zone.addEventListener("drop", async (e) => {
      e.preventDefault();
      zone.classList.remove("drop-target");
      const id = e.dataTransfer.getData("text/plain");
      const estado = zone.closest(".column")?.dataset.estado;
      if (!id || !estado) return;
      try {
        await invoke("set_estado", { id, estado });
      } catch (err) {
        setStatus(`error: ${err}`);
      }
      renderBoard();
    });
  });
}

function wireTopbar() {
  document
    .getElementById("btn-new-task")
    .addEventListener("click", () => openNewTask({ projectId: cachedActiveProject }));
  document
    .getElementById("btn-settings")
    .addEventListener("click", () => openSettings());
  document
    .getElementById("btn-theme")
    .addEventListener("click", () => toggleTheme());

  const newIdeiaBtn = document.getElementById("btn-new-ideia");
  if (newIdeiaBtn) {
    newIdeiaBtn.addEventListener("click", () =>
      openNewIdeia({ projectId: cachedActiveProject }),
    );
  }

  document
    .getElementById("project-select")
    .addEventListener("change", async (e) => {
      const value = e.target.value || null;
      try {
        await invoke("set_active_project", { projectId: value });
        await renderBoard();
      } catch (err) {
        setStatus(`error: ${err}`);
      }
    });
}

function renderProjectOptions(projects, active) {
  const sel = document.getElementById("project-select");
  // Wipe existing options except the first ("Todos os projetos") so
  // we preserve the data-i18n binding on that <option>.
  while (sel.options.length > 1) sel.remove(1);
  for (const p of projects) {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = p.name;
    sel.append(opt);
  }
  sel.value = active ?? "";
}

function setStatus(msg) {
  const el = document.getElementById("status");
  if (el) el.textContent = msg ?? "";
}

async function main() {
  // Apply the persisted theme override before anything paints, so we
  // don't flash the OS-default theme for a frame.
  initTheme();
  await bootI18n();
  wireTopbar();
  wireDropZones();
  setRefreshCallback(renderBoard);
  setTriageRefresh(renderBoard);
  setIdeiaRefreshCallback(renderBoard);
  setStartAgentRefreshCallback(renderBoard);
  setSettingsRefreshCallback(renderBoard);
  invoke("app_version")
    .then((v) => {
      const el = document.getElementById("app-version");
      if (el && typeof v === "string") el.textContent = `v${v}`;
    })
    .catch(() => {});
  await renderBoard();
  await refreshPendingBadge();

  // Locale switch should redraw board chrome (column headers update
  // via [data-i18n] in i18n.js; empty-state strings come from t()).
  onLocaleChange(() => {
    renderBoard();
    refreshPendingBadge();
  });

  // Backend → UI pushes. The tray "Configurações…" item lands here.
  try {
    await listen("open_settings", () => openSettings());
    await listen("proposta_pendente", (e) => {
      const propostaId = e?.payload?.proposta_id;
      // Auto-open the triage modal so the human notices and decides.
      openTriage(propostaId);
      renderBoard();
    });
    await listen("proposta_decidida", () => {
      refreshPendingBadge();
      renderBoard();
    });
    // Codex captures its session UUID async after first spawn — the
    // backend emits this event so the card indicator (has-run dot) can
    // refresh without the user having to do anything.
    await listen("task_run_changed", renderBoard);
    // Emitido pelo IPC server quando o agente cria tasks via
    // `cadenza-cli new-task` (fluxo "destrinchar ideia").
    await listen("tasks_changed", renderBoard);
    // Idem para mudanças em ideias (criação via CLI, marcar como
    // destrinchada quando todas as tasks da decomposição foram criadas).
    await listen("ideias_changed", renderBoard);
  } catch (e) {
    console.warn("event subscribe failed", e);
  }
}

main().catch((err) => {
  console.error(err);
  setStatus(`fatal: ${err}`);
});

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
import { PROJECT_COLORS } from "./project-colors.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ESTADOS = ["a_fazer", "fazendo", "aguardando_revisao", "feito"];

// Cached so the board can re-filter without round-tripping to disk on
// every project-selector change. Repopulated on every renderBoard().
let cachedTaskProjects = {};
// project_id → color key, rebuilt on every renderBoard().
let cachedProjectColors = {};
let cachedActiveProject = null;
let cachedTasksById = {};
// Shown once per session when no projects exist, so the user is guided
// to add a first project without reopening settings on every re-render.
let _guidedToFirstProject = false;
// task_id → task-run record from list_task_runs. Used to mark cards
// that have a saved conversation so the user knows "click ▶ = resume".
let cachedTaskRuns = {};
// Estado the currently dragged card started in. Set on dragstart, read
// on drop to tell a within-column reorder from a cross-column move (so
// we only call set_estado when the column actually changed).
let draggedFromEstado = null;

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
  cachedTasksById = Object.fromEntries((tasks ?? []).map((task) => [task.id, task]));
  cachedActiveProject = cfg?.active_project_id ?? null;
  const colorMap = {};
  for (const p of (cfg?.projects ?? [])) {
    if (p.color) colorMap[p.id] = p.color;
  }
  cachedProjectColors = colorMap;
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

// Estados that satisfy a blocker so a dependent task may start. Mirror of
// `Estado::satisfies_blocker` in proto/src/task.rs — keep the two in sync.
const BLOCKER_SATISFIED_ESTADOS = ["aguardando_revisao", "feito"];

function blockerStatus(task) {
  const blockers = Array.isArray(task.blocked_by) ? task.blocked_by : [];
  const pending = [];
  for (const id of blockers) {
    const blocker = cachedTasksById[id];
    if (!blocker) {
      pending.push(`${id}: ${t("task-blocker-missing") || "not found"}`);
    } else if (!BLOCKER_SATISFIED_ESTADOS.includes(blocker.estado)) {
      pending.push(`${id}: ${estadoLabel(blocker.estado)}`);
    }
  }
  return { count: blockers.length, pending };
}

function estadoLabel(estado) {
  return t(`estado-${String(estado).replaceAll("_", "-")}`) || estado;
}

function makeCard(task) {
  const card = document.createElement("div");
  card.className = "card";
  card.draggable = true;
  card.dataset.id = task.id;

  // Color bar — left accent, shown only in the all-projects view so
  // cards from different projects are visually distinguishable.
  if (!cachedActiveProject) {
    const projectId = cachedTaskProjects[task.id];
    const colorKey = projectId ? cachedProjectColors[projectId] : null;
    const hex = colorKey ? PROJECT_COLORS[colorKey] : null;
    if (hex) {
      const bar = document.createElement("span");
      bar.className = "card-project-bar";
      bar.style.background = hex;
      card.append(bar);
      card.classList.add("card--colored");
    }
  }

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
  const blockers = blockerStatus(task);
  const isBlocked = blockers.pending.length > 0;
  if (hasRun && !isBlocked) startBtn.classList.add("has-run");
  startBtn.title = isBlocked
    ? `${t("card-blocked-title") || "Blocked"}: ${blockers.pending.join("; ")}`
    : hasRun
      ? t("card-start-resume-aria")
      : t("card-start-aria");
  startBtn.setAttribute(
    "aria-label",
    isBlocked
      ? t("card-blocked-title") || "Blocked"
      : hasRun ? t("card-start-resume-aria") : t("card-start-aria"),
  );
  startBtn.disabled = task.estado === "feito" || isBlocked;
  startBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (startBtn.disabled) return;
    openStartAgent(task.id, { titulo: task.titulo });
  });
  // Prevent button drag from also dragging the card.
  startBtn.addEventListener("dragstart", (e) => e.preventDefault());

  // Plan button — opens the same agent modal in plan mode. The agent
  // interviews the human and writes a `## Plano` section into the body;
  // the task stays in its column (planning happens before execution).
  const planBtn = document.createElement("button");
  planBtn.type = "button";
  planBtn.className = "btn btn-icon card-plan";
  planBtn.textContent = "🗒";
  planBtn.title = t("card-plan-aria");
  planBtn.setAttribute("aria-label", t("card-plan-aria"));
  planBtn.disabled = task.estado === "feito";
  planBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (planBtn.disabled) return;
    openStartAgent(task.id, { titulo: task.titulo, mode: "plan" });
  });
  planBtn.addEventListener("dragstart", (e) => e.preventDefault());

  card.append(title, id, startBtn, planBtn);

  if (blockers.count > 0) {
    const blockerBadge = document.createElement("span");
    blockerBadge.className =
      "card-blockers" + (isBlocked ? " is-blocked" : " is-clear");
    blockerBadge.textContent = isBlocked
      ? t("card-blocked-title") || "Blocked"
      : t("card-unblocked-title") || "Unblocked";
    blockerBadge.title = isBlocked
      ? blockers.pending.join("; ")
      : t("card-unblocked-title") || "Unblocked";
    card.append(blockerBadge);
  }

  // Branch badge — shown when the task is associated with a git branch
  // (field enriched by the backend from task-worktrees.json).
  if (task.branch) {
    const branchBadge = document.createElement("span");
    branchBadge.className = "card-branch";
    branchBadge.textContent = task.branch;
    branchBadge.title = task.worktree_path ?? task.branch;
    card.append(branchBadge);
  }

  card.addEventListener("dragstart", (e) => {
    e.dataTransfer.setData("text/plain", task.id);
    e.dataTransfer.effectAllowed = "move";
    card.classList.add("dragging");
    draggedFromEstado = card.closest(".column")?.dataset.estado ?? null;
  });
  card.addEventListener("dragend", () => {
    card.classList.remove("dragging");
    // Clear on every drag end (cancel or successful drop) so a cancelled
    // drag never leaks its source column into the next drop handler.
    draggedFromEstado = null;
  });
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

// Find the card a dropped element should be inserted *before*, given the
// cursor's vertical position — the standard "element after cursor" trick.
// Returns null when the cursor is below every card (append at the end).
// The card being dragged is skipped so it doesn't measure against itself.
function cardAfterCursor(zone, y) {
  const cards = [...zone.querySelectorAll(".card:not(.dragging)")];
  let closest = { offset: Number.NEGATIVE_INFINITY, el: null };
  for (const card of cards) {
    const box = card.getBoundingClientRect();
    const offset = y - (box.top + box.height / 2);
    if (offset < 0 && offset > closest.offset) {
      closest = { offset, el: card };
    }
  }
  return closest.el;
}

function wireDropZones() {
  document.querySelectorAll("[data-drop]").forEach((zone) => {
    zone.addEventListener("dragover", (e) => {
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      zone.classList.add("drop-target");
      // The Inbox column holds ideias, not tasks — no reordering there.
      if (zone.closest(".column")?.dataset.estado == null) return;
      // Live preview: move the dragged card to where it would land, so
      // the resulting gap is the drop indicator. Works within a column
      // and across columns (gives the precise-position cross-column UX).
      const dragging = document.querySelector(".card.dragging");
      if (!dragging) return;
      const ref = cardAfterCursor(zone, e.clientY);
      if (ref == null) {
        zone.appendChild(dragging);
      } else {
        zone.insertBefore(dragging, ref);
      }
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
      // No estado → the Inbox/ideia column; leave it to its own handler.
      // renderBoard() reverts any dragover preview that may have moved the
      // card into this zone visually.
      if (!id || !estado) { renderBoard(); return; }
      const movedColumns = draggedFromEstado && draggedFromEstado !== estado;
      // Snapshot the DOM order *before* any await — a Tauri event (e.g.
      // task_run_changed) can fire renderBoard() between awaits, detaching
      // `zone` and making domOrder() return [] which would erase the stored
      // order for this column.
      const domOrder = (z) =>
        [...z.querySelectorAll(".card")].map((c) => c.dataset.id);
      const destIds = domOrder(zone);
      const srcEl = movedColumns
        ? document.querySelector(
            `.column[data-estado="${draggedFromEstado}"] .cards`,
          )
        : null;
      const srcIds = srcEl ? domOrder(srcEl) : null;
      try {
        if (movedColumns) await invoke("set_estado", { id, estado });
        // The dragover preview already placed the card; persist the order
        // captured above (safe across any re-render that follows the await).
        await invoke("set_task_order", { estado, ids: destIds });
        if (movedColumns && srcIds) {
          // The card left its source column — persist that column's new
          // order too so its stored list no longer references the card.
          await invoke("set_task_order", {
            estado: draggedFromEstado,
            ids: srcIds,
          });
        }
      } catch (err) {
        setStatus(`error: ${err}`);
      }
      draggedFromEstado = null;
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
    // `check_for_updates` em lib.rs dispara este evento com a string da
    // nova versão como payload. O banner é não-bloqueante e fica até
    // o usuário clicar "Reiniciar agora" ou "×".
    await listen("update_available", (e) => {
      const version = typeof e?.payload === "string" ? e.payload : "";
      showUpdateBanner(version);
    });
  } catch (e) {
    console.warn("event subscribe failed", e);
  }
  wireUpdateBanner();
}

// Version the user explicitly dismissed. The 24h ticker (and manual
// check_update) re-emit `update_available` for the same pending build;
// without this, dismissing the banner only hides it until the next
// poll re-shows it for a version the user already waved off.
let dismissedUpdateVersion = null;

function showUpdateBanner(version) {
  const banner = document.getElementById("update-banner");
  if (!banner) return;
  if (version && version === dismissedUpdateVersion) return;
  const tag = document.getElementById("update-banner-version");
  if (tag) tag.textContent = version ? `v${version}` : "";
  banner.dataset.version = version || "";
  banner.hidden = false;
}

function wireUpdateBanner() {
  const banner = document.getElementById("update-banner");
  const restartBtn = document.getElementById("btn-update-restart");
  const dismissBtn = document.getElementById("btn-update-dismiss");
  if (!banner || !restartBtn || !dismissBtn) return;
  restartBtn.addEventListener("click", async () => {
    restartBtn.disabled = true;
    try {
      // App relaunches mid-call; the promise never resolves in the
      // happy path. A rejection means the install failed before the
      // process restart — surface it so the user isn't stuck.
      await invoke("install_update_and_restart");
    } catch (err) {
      restartBtn.disabled = false;
      setStatus(`error: ${err}`);
    }
  });
  dismissBtn.addEventListener("click", () => {
    dismissedUpdateVersion = banner.dataset.version || "";
    banner.hidden = true;
  });
}

main().catch((err) => {
  console.error(err);
  setStatus(`fatal: ${err}`);
});

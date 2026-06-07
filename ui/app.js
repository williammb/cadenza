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
import { toggleDrawer, onDrawerStateChange } from "./terminal.js";
import {
  openJiraImport,
  setJiraImportRefreshCallback,
} from "./jira-import.js";
import { initModalA11y } from "./modal-a11y.js";

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
// task_id → latest evidence_state, from a single list_review_states call
// (no N+1). Feeds the small evidence dot on cards awaiting review.
let cachedReviewStates = {};
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
  let reviewStates = {};
  try {
    [tasks, ideias, mapping, cfg, runs, reviewStates] = await Promise.all([
      invoke("list_tasks", { estado: null }),
      invoke("list_ideias").catch(() => []),
      invoke("list_task_projects"),
      invoke("get_config"),
      invoke("list_task_runs").catch(() => ({})),
      invoke("list_review_states").catch(() => ({})),
    ]);
  } catch (e) {
    setStatus(`error: ${e}`);
    return;
  }
  cachedTaskProjects = mapping ?? {};
  cachedTaskRuns = runs ?? {};
  cachedReviewStates = reviewStates ?? {};
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

// Localized label for an evidence_state (review package). Mirrors the
// snake_case wire variants of EvidenceState in src-tauri/src/review/mod.rs.
function evidenceStateLabel(state) {
  return t(`review-state-${String(state).replaceAll("_", "-")}`) || state;
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

  // Evidence dot — a small colored dot reflecting the latest review
  // package's evidence_state, fed by the batched list_review_states call
  // (no per-card fetch). Shown only for tasks awaiting review.
  const evidenceState = cachedReviewStates[task.id];
  if (evidenceState && task.estado === "aguardando_revisao") {
    const dot = document.createElement("span");
    dot.className = `card-evidence card-evidence--${evidenceState}`;
    dot.title = evidenceStateLabel(evidenceState);
    dot.setAttribute("aria-label", evidenceStateLabel(evidenceState));
    card.append(dot);
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
    .getElementById("btn-jira-import")
    .addEventListener("click", () => openJiraImport({ projectId: cachedActiveProject }));
  document
    .getElementById("btn-settings")
    .addEventListener("click", () => openSettings());
  document
    .getElementById("btn-theme")
    .addEventListener("click", () => toggleTheme());

  // Topbar terminal toggle. The drawer's own chevron and terminal.js's
  // closeSession empty-branch also flip the drawer, so the button's
  // pressed state is driven by the onDrawerStateChange hook (the single
  // writer of aria-pressed) rather than mutated inline here.
  const terminalBtn = document.getElementById("btn-terminal-open");
  terminalBtn.addEventListener("click", () => toggleDrawer());
  onDrawerStateChange((open) => {
    terminalBtn.setAttribute("aria-pressed", open ? "true" : "false");
  });

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

function setModelsStatus(msg) {
  const el = document.getElementById("models-status");
  if (el) el.textContent = msg ?? "";
}

async function autoLoadModels() {
  let cfg;
  try { cfg = await invoke("get_config"); } catch { return; }
  const kind = cfg?.agente?.kind;
  if (!kind) return;

  let cached;
  try {
    cached = await invoke("list_agent_models", { agentKind: kind, cachedOnly: true });
  } catch { return; }
  if (cached && cached.length > 0) return;

  setModelsStatus(t("settings-models-loading") || "Carregando modelos…");
  try {
    await invoke("list_agent_models", { agentKind: kind });
    setModelsStatus(t("topbar-models-loaded") || "Modelos carregados");
    setTimeout(() => setModelsStatus(null), 3000);
  } catch {
    setModelsStatus(null);
  }
}

async function main() {
  // Apply the persisted theme override before anything paints, so we
  // don't flash the OS-default theme for a frame.
  initTheme();
  await bootI18n();
  // Systematic modal a11y: focus restore, focus trap, aria-labelledby —
  // applied to every `<dialog class="modal">` before any can open.
  initModalA11y();
  wireTopbar();
  wireDropZones();
  setRefreshCallback(renderBoard);
  setTriageRefresh(renderBoard);
  setIdeiaRefreshCallback(renderBoard);
  setStartAgentRefreshCallback(renderBoard);
  setSettingsRefreshCallback(renderBoard);
  setJiraImportRefreshCallback(renderBoard);
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
    // Background failures (updater poll, IPC server) emit a structured
    // payload {kind, detail}. The detail is English (for support) and
    // already logged; the toast picks a localized line by kind.
    await listen("background-error", (e) => {
      const kind = e?.payload?.kind || "generic";
      const detail = e?.payload?.detail || "";
      showBackgroundErrorToast(kind, detail);
    });
  } catch (e) {
    console.warn("event subscribe failed", e);
  }
  wireUpdateBanner();
  autoLoadModels();
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

// Build and show a dismissible toast for a background failure. The
// message is chosen by `kind` (background-error-<kind>, falling back to
// the generic line for unknown kinds); the English `detail` is appended
// as a muted sub-line for support. Auto-dismisses after a grace period.
// No innerHTML — every node is created via the DOM API.
const MAX_BACKGROUND_TOASTS = 3;

function showBackgroundErrorToast(kind, detail) {
  const stack = document.getElementById("toast-stack");
  if (!stack) return;

  // De-duplicate: a flapping subsystem (a failing updater poll, a crash-looping
  // IPC server) re-emits the same (kind, detail) every cycle. Drop a repeat
  // that's already on screen instead of stacking identical copies.
  const dedupKey = `${kind} ${detail || ""}`;
  for (const existing of stack.children) {
    if (existing.dataset.toastKey === dedupKey) return;
  }

  const toast = document.createElement("div");
  toast.className = "toast toast-error";
  toast.dataset.toastKey = dedupKey;

  const body = document.createElement("div");
  body.className = "toast-body";

  const title = document.createElement("strong");
  title.textContent = t("background-error-title");

  const msg = document.createElement("span");
  const knownKinds = ["updater", "ipc"];
  const key = knownKinds.includes(kind)
    ? `background-error-${kind}`
    : "background-error-generic";
  msg.textContent = t(key);

  body.append(title, msg);
  if (detail) {
    const det = document.createElement("span");
    det.className = "toast-detail";
    det.textContent = detail;
    body.append(det);
  }

  const dismiss = document.createElement("button");
  dismiss.type = "button";
  dismiss.className = "btn btn-icon";
  dismiss.setAttribute("aria-label", t("background-error-dismiss"));
  dismiss.title = t("background-error-dismiss");
  dismiss.textContent = "×";
  const remove = () => toast.remove();
  dismiss.addEventListener("click", remove);

  toast.append(body, dismiss);
  stack.append(toast);

  // Cap the stack so a burst of distinct errors can't bury the UI: drop the
  // oldest toasts beyond the limit.
  while (stack.childElementCount > MAX_BACKGROUND_TOASTS) {
    stack.firstElementChild.remove();
  }

  // Auto-dismiss; the user can also close it sooner via the × button.
  setTimeout(remove, 12000);
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

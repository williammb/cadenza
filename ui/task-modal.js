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

// Tabbed sections inside #task-form. Scoped to `form` so the settings-modal
// tabs (.settings-tab) are never picked up. The Worktree tab is usable only
// when editing an existing task; the Revisão tab only when a review package
// and/or suggested learnings exist.
const taskTabsNav = form.querySelector(".task-tabs");
const taskTabButtons = [...form.querySelectorAll(".task-tab")];
const taskPanels = [...form.querySelectorAll(".task-panel")];

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

// Suggested-learnings section — review (aguardando_revisao) only. The
// execution agent proposes learnings via `cadenza-cli memory suggest`;
// here the user promotes the ones worth keeping into the project's
// official memory, or discards them. Independent of completing the task.
const learningsSection = document.getElementById("task-learnings-section");
const learningsListEl = document.getElementById("task-learnings-list");
const learningsEmptyEl = document.getElementById("task-learnings-empty");
let learningsLoadGen = 0;

// Revisão section — shown only when the task has a review package
// (get_review_package returns one, typically estado=aguardando_revisao).
// All content is built in JS and rendered via textContent/createElement —
// every string here comes from the agent or the repo and is UNTRUSTED.
const reviewSection = document.getElementById("task-review-section");
const reviewBodyEl = document.getElementById("task-review-body");
let reviewLoadGen = 0;

let mode = "create"; // "create" | "edit"
let editingId = null;
let original = null;
let onClosedRefresh = null;
let selectedBlockers = new Set();
let blockerLoadGen = 0;

// Tab state. `activeTab` mirrors the shown panel; `userPickedTab` flips true
// once the user clicks/keys a tab so the auto-switch-to-Revisão default never
// yanks focus away from a tab they chose. `openGen` is a per-open generation
// token: every open bumps it so a post-await review/learnings loader from a
// previously-open task can't flip flags for the task now showing.
let activeTab = "detalhes";
let userPickedTab = false;
let openGen = 0;
let reviewAvailable = false;
let learningsAvailable = false;
let reviewLoadDone = false;

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

// ──────────────────────────────── tabs ──────────────────────────────
//
// Real ARIA tabs (role="tab"/"tabpanel"). `activateTab` mirrors the
// settings-modal pattern, scoped to the task tab/panel arrays.

function activateTab(name) {
  for (const b of taskTabButtons) {
    const active = b.dataset.tab === name;
    b.classList.toggle("is-active", active);
    b.setAttribute("aria-selected", active ? "true" : "false");
    b.tabIndex = active ? 0 : -1;
  }
  for (const p of taskPanels) {
    p.hidden = p.dataset.panel !== name;
  }
  activeTab = name;
}

// Single source of truth for tab visibility + the active-tab correction.
// Driven by the availability flags: detalhes always available; worktree only
// when editing an existing task; revisao only when a review package and/or
// suggested learnings loaded. Hides unavailable tab BUTTONS, auto-switches to
// Revisão on the initial load of a review task (before any user interaction),
// and falls back to the first available tab if the active one disappears.
function syncTaskTabs() {
  const avail = {
    detalhes: true,
    worktree: mode === "edit",
    revisao: reviewAvailable || learningsAvailable,
  };
  for (const b of taskTabButtons) {
    b.hidden = !avail[b.dataset.tab];
  }
  // Auto-switch to Revisão on initial load of a review task, only if the
  // user hasn't navigated yet and the review loader has settled.
  if (
    avail.revisao &&
    !userPickedTab &&
    reviewLoadDone &&
    original?.estado === "aguardando_revisao"
  ) {
    activateTab("revisao");
    return;
  }
  // If the active tab became unavailable, fall back to the first available.
  if (!avail[activeTab]) {
    const first = taskTabButtons.find((b) => avail[b.dataset.tab]);
    activateTab(first ? first.dataset.tab : "detalhes");
  }
}

for (const b of taskTabButtons) {
  b.addEventListener("click", () => {
    userPickedTab = true;
    activateTab(b.dataset.tab);
  });
}

// Roving arrow-key focus across the tablist (Left/Right wrap, Home/End),
// mirroring ARIA tab semantics. Skips hidden tab buttons.
taskTabsNav?.addEventListener("keydown", (e) => {
  const keys = ["ArrowLeft", "ArrowRight", "Home", "End"];
  if (!keys.includes(e.key)) return;
  const visible = taskTabButtons.filter((b) => !b.hidden);
  if (visible.length === 0) return;
  const current = document.activeElement;
  const at = visible.indexOf(current);
  let next;
  if (e.key === "Home") {
    next = 0;
  } else if (e.key === "End") {
    next = visible.length - 1;
  } else {
    const from = at === -1 ? 0 : at;
    const delta = e.key === "ArrowRight" ? 1 : -1;
    next = (from + delta + visible.length) % visible.length;
  }
  const target = visible[next];
  if (!target) return;
  e.preventDefault();
  userPickedTab = true;
  activateTab(target.dataset.tab);
  target.focus();
});

export async function openNewTask(prefill = {}) {
  const myOpen = ++openGen;
  userPickedTab = false;
  reviewAvailable = false;
  learningsAvailable = false;
  reviewLoadDone = false;
  activateTab("detalhes");
  void myOpen; // no post-await loaders here; Revisão stays hidden by design
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
  learningsSection.hidden = true; // no learnings for a not-yet-created task
  reviewSection.hidden = true; // no review package for a not-yet-created task
  reviewBodyEl.replaceChildren();
  attachments.reset();
  setStatus("");
  // Hide the worktree/revisao tabs (create mode → only Detalhes).
  syncTaskTabs();

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
  // Reset tab state before any await so a stale loader from a previously-open
  // task can't flip flags for this one (each loader captures `openGen`).
  const myOpen = ++openGen;
  userPickedTab = false;
  reviewAvailable = false;
  learningsAvailable = false;
  reviewLoadDone = false;
  activateTab("detalhes");
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
  if (myOpen !== openGen) return; // a newer open superseded this one
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
  // Show the Worktree tab now (edit mode); the Revisão tab is revealed later
  // by loadReview/loadSuggestedLearnings once their flags settle.
  syncTaskTabs();
  await loadBlockerChoices(task.blocked_by ?? []);
  loadWorktreeDefaults(id);
  loadSuggestedLearnings(id, task.estado);
  loadReview(id, task.estado);
  if (!dialog.open) dialog.showModal();
  tituloEl.focus();
}

// ─────────────────────────── Revisão tab ───────────────────────────
//
// Renders the latest ReviewPackage for the task: evidence-state chip
// (+ overlays), reported-checks table, risk chips, summary, open
// questions, a lazily-loaded intent-grouped diff, and reviewer actions
// (Aprovar / Pedir alterações) when the task is awaiting review and the
// package is still undecided. Everything is built with createElement +
// textContent; agent/repo text is run through stripAnsi first.

// Strip ANSI SGR sequences and other control chars so untrusted log
// excerpts / paths / labels can't smuggle terminal escapes into the DOM.
// textContent already neutralizes HTML; this keeps the rendered text clean.
function stripAnsi(s) {
  return String(s ?? "")
    // eslint-disable-next-line no-control-regex
    .replace(/\x1b\[[0-9;]*[A-Za-z]/g, "")
    // eslint-disable-next-line no-control-regex
    .replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, "");
}

// Evidence-state → (label key, chip modifier) map. The modifier drives
// the chip color via CSS (review-chip--ok / --warn / --bad / --info).
const EVIDENCE_CHIP = {
  passed: { key: "review-state-passed", mod: "ok" },
  failed: { key: "review-state-failed", mod: "bad" },
  partial: { key: "review-state-partial", mod: "warn" },
  no_validation: { key: "review-state-no-validation", mod: "info" },
  contract_changed: { key: "review-state-contract-changed", mod: "warn" },
  contract_unavailable: { key: "review-state-contract-unavailable", mod: "info" },
};

const RISK_LABEL = {
  new_dependency: "review-risk-new-dependency",
  migration: "review-risk-migration",
  auth: "review-risk-auth",
  public_contract: "review-risk-public-contract",
  large_file: "review-risk-large-file",
  possible_secret: "review-risk-possible-secret",
};

function reviewChip(text, mod) {
  const span = document.createElement("span");
  span.className = "review-chip" + (mod ? ` review-chip--${mod}` : "");
  span.textContent = text;
  return span;
}

function reviewSubhead(textKey) {
  const h = document.createElement("h4");
  h.className = "review-subhead";
  h.textContent = t(textKey);
  return h;
}

async function loadReview(taskId, estado) {
  const myGen = ++reviewLoadGen;
  const myOpen = openGen; // capture the current open generation
  reviewSection.hidden = true;
  reviewBodyEl.replaceChildren();

  let pkg = null;
  try {
    pkg = await invoke("get_review_package", { taskId });
  } catch {
    // no package / read failed → keep section hidden, Revisão tab unavailable
    if (myGen === reviewLoadGen && myOpen === openGen) {
      reviewAvailable = false;
      reviewLoadDone = true;
      syncTaskTabs();
    }
    return;
  }
  if (myGen !== reviewLoadGen || myOpen !== openGen) return;
  if (!pkg) {
    // task never `done` with evidence
    reviewAvailable = false;
    reviewLoadDone = true;
    syncTaskTabs();
    return;
  }

  reviewSection.hidden = false;

  // ── evidence-state chip + overlays ──
  const stateRow = document.createElement("div");
  stateRow.className = "review-state-row";
  const chipInfo = EVIDENCE_CHIP[pkg.evidence_state] || {
    key: `review-state-${String(pkg.evidence_state).replaceAll("_", "-")}`,
    mod: "info",
  };
  stateRow.append(reviewChip(t(chipInfo.key), chipInfo.mod));
  if (pkg.needs_focused_human_review) {
    stateRow.append(reviewChip(t("review-needs-focused-human-review"), "bad"));
  }
  if (pkg.validation_scope_unknown) {
    stateRow.append(reviewChip(t("review-validation-scope-unknown"), "warn"));
  }
  reviewBodyEl.append(stateRow);

  // ── summary ──
  if (pkg.summary && pkg.summary.trim()) {
    reviewBodyEl.append(reviewSubhead("review-summary-header"));
    const p = document.createElement("p");
    p.className = "review-summary";
    p.textContent = stripAnsi(pkg.summary);
    reviewBodyEl.append(p);
  }

  // ── changed-file counts ──
  const counts = document.createElement("p");
  counts.className = "review-counts modal-hint";
  counts.textContent = t("review-changed-files", {
    added: pkg.files_added ?? 0,
    modified: pkg.files_modified ?? 0,
    deleted: pkg.files_deleted ?? 0,
  });
  reviewBodyEl.append(counts);

  // ── checks table ──
  reviewBodyEl.append(reviewSubhead("review-checks-header"));
  const checks = pkg.checks ?? [];
  if (checks.length === 0) {
    const empty = document.createElement("p");
    empty.className = "modal-status";
    empty.textContent = t("review-checks-empty");
    reviewBodyEl.append(empty);
  } else {
    reviewBodyEl.append(makeChecksTable(checks));
  }

  // ── risk chips + secret findings ──
  const risks = pkg.risks ?? [];
  const secrets = pkg.secret_matches ?? [];
  if (risks.length || secrets.length) {
    reviewBodyEl.append(reviewSubhead("review-risks-header"));
    const riskRow = document.createElement("div");
    riskRow.className = "review-chips";
    for (const r of risks) {
      const key = RISK_LABEL[r];
      riskRow.append(reviewChip(key ? t(key) : stripAnsi(r), "warn"));
    }
    reviewBodyEl.append(riskRow);
    // possible_secret findings: redacted {kind,file,line} only — never a value.
    for (const s of secrets) {
      const line = document.createElement("p");
      line.className = "review-secret modal-hint";
      line.textContent = t("review-secret-finding", {
        kind: stripAnsi(s.kind),
        file: stripAnsi(s.file),
        line: s.line ?? 0,
      });
      reviewBodyEl.append(line);
    }
  }

  // ── open questions ──
  const questions = pkg.open_questions ?? [];
  if (questions.length) {
    reviewBodyEl.append(reviewSubhead("review-open-questions"));
    const ul = document.createElement("ul");
    ul.className = "review-questions";
    for (const q of questions) {
      const li = document.createElement("li");
      li.textContent = stripAnsi(q);
      ul.append(li);
    }
    reviewBodyEl.append(ul);
  }

  // ── lazy diff ──
  const diffWrap = document.createElement("div");
  diffWrap.className = "review-diff-wrap";
  const loadDiffBtn = document.createElement("button");
  loadDiffBtn.type = "button";
  loadDiffBtn.className = "btn btn-sm";
  loadDiffBtn.textContent = t("review-load-diff");
  loadDiffBtn.addEventListener("click", () => loadReviewDiff(taskId, diffWrap, loadDiffBtn));
  diffWrap.append(loadDiffBtn);
  reviewBodyEl.append(diffWrap);

  // ── reviewer actions (only awaiting review + undecided) ──
  const undecided = pkg.status === "pending";
  if (estado === "aguardando_revisao" && undecided) {
    reviewBodyEl.append(makeReviewActions(taskId));
  }

  reviewAvailable = true;
  reviewLoadDone = true;
  syncTaskTabs();
}

function makeChecksTable(checks) {
  const table = document.createElement("table");
  table.className = "review-checks-table";
  const thead = document.createElement("thead");
  const htr = document.createElement("tr");
  for (const key of ["review-checks-col-id", "review-checks-col-exit", "review-checks-col-log"]) {
    const th = document.createElement("th");
    th.textContent = t(key);
    htr.append(th);
  }
  thead.append(htr);
  table.append(thead);

  const tbody = document.createElement("tbody");
  for (const c of checks) {
    const tr = document.createElement("tr");

    const tdId = document.createElement("td");
    tdId.textContent = stripAnsi(c.id);
    tr.append(tdId);

    const tdExit = document.createElement("td");
    const exit = c.exit ?? 0;
    tdExit.textContent = String(exit);
    tdExit.className = exit === 0 ? "review-exit-ok" : "review-exit-bad";
    tr.append(tdExit);

    const tdLog = document.createElement("td");
    const pre = document.createElement("pre");
    pre.className = "review-log-excerpt";
    pre.textContent = stripAnsi(c.log_excerpt ?? "");
    tdLog.append(pre);
    // log_path is display-only — shown as a label, NEVER fetched.
    if (c.log_path) {
      const pathLabel = document.createElement("span");
      pathLabel.className = "review-log-path modal-hint";
      pathLabel.textContent = t("review-log-path", { path: stripAnsi(c.log_path) });
      tdLog.append(pathLabel);
    }
    tr.append(tdLog);

    tbody.append(tr);
  }
  table.append(tbody);
  return table;
}

async function loadReviewDiff(taskId, wrap, btn) {
  btn.disabled = true;
  const loading = document.createElement("span");
  loading.className = "modal-status";
  loading.textContent = t("review-diff-loading");
  wrap.append(loading);

  let resp;
  try {
    resp = await invoke("get_review_diff", { taskId });
  } catch (e) {
    loading.remove();
    btn.disabled = false;
    const err = document.createElement("span");
    err.className = "modal-status error";
    err.textContent = t("review-load-error", { error: e });
    wrap.append(err);
    return;
  }
  loading.remove();
  btn.remove();
  renderReviewDiff(resp, wrap);
}

function renderReviewDiff(resp, wrap) {
  // Stale: the worktree moved since done — show a note and the stored
  // capped+redacted uncommitted patch instead of the (now divergent)
  // live committed diff.
  if (resp.stale) {
    const note = document.createElement("p");
    note.className = "modal-status warn";
    note.textContent = t("review-worktree-stale");
    wrap.append(note);
    if (resp.uncommitted && (resp.uncommitted.files ?? []).length) {
      for (const f of resp.uncommitted.files) {
        wrap.append(makeDiffFileDetails(f.path, f.patch, f.truncated));
      }
      if (resp.uncommitted.files_omitted) {
        wrap.append(diffMarker(t("review-diff-files-omitted", { count: resp.uncommitted.files_omitted })));
      }
    } else {
      wrap.append(diffMarker(t("review-diff-empty")));
    }
    return;
  }

  if (resp.diff_unavailable) {
    wrap.append(diffMarker(t("review-diff-unavailable")));
    return;
  }

  const groups = resp.groups ?? [];
  if (groups.length === 0) {
    wrap.append(diffMarker(t("review-diff-empty")));
    return;
  }

  for (const g of groups) {
    // "Other" is a backend sentinel label; localize it, leave agent
    // labels as-is (stripped).
    const label = g.label === "Other" ? t("review-diff-other") : stripAnsi(g.label);
    const section = document.createElement("details");
    section.className = "review-diff-group";
    section.open = true;
    const summary = document.createElement("summary");
    summary.textContent = label;
    section.append(summary);
    for (const f of g.files ?? []) {
      section.append(makeDiffFileDetails(f.path, f.patch, f.truncated));
    }
    wrap.append(section);
  }

  if (resp.truncated) {
    wrap.append(diffMarker(t("review-diff-truncated")));
  }
  if (resp.files_omitted) {
    wrap.append(diffMarker(t("review-diff-files-omitted", { count: resp.files_omitted })));
  }
}

function diffMarker(text) {
  const p = document.createElement("p");
  p.className = "modal-status";
  p.textContent = text;
  return p;
}

function makeDiffFileDetails(path, patch, truncated) {
  const details = document.createElement("details");
  details.className = "review-diff-file";
  const summary = document.createElement("summary");
  summary.textContent = stripAnsi(path);
  details.append(summary);
  const pre = document.createElement("pre");
  pre.className = "review-diff-body";
  pre.textContent = stripAnsi(patch ?? "");
  details.append(pre);
  if (truncated) {
    details.append(diffMarker(t("review-diff-truncated")));
  }
  return details;
}

function makeReviewActions(taskId) {
  const actions = document.createElement("div");
  actions.className = "review-actions";

  const note = document.createElement("textarea");
  note.id = "task-review-note";
  note.rows = 2;
  note.className = "review-note";
  note.placeholder = t("review-note-placeholder");
  actions.append(note);

  const status = document.createElement("span");
  status.className = "modal-status";

  const btnRow = document.createElement("div");
  btnRow.className = "review-action-buttons";

  const requestBtn = document.createElement("button");
  requestBtn.type = "button";
  requestBtn.className = "btn btn-danger";
  requestBtn.textContent = t("review-request-changes");

  const approveBtn = document.createElement("button");
  approveBtn.type = "button";
  approveBtn.className = "btn btn-primary";
  approveBtn.textContent = t("review-approve");

  async function decide(verdict, btn) {
    requestBtn.disabled = true;
    approveBtn.disabled = true;
    status.className = "modal-status";
    status.textContent = "";
    try {
      await invoke("review_decision", {
        taskId,
        verdict,
        note: note.value,
      });
      status.className = "modal-status ok";
      status.textContent = t("review-decided");
      // Refresh the board and close so the card lands in its new column.
      closeTaskModal();
      onClosedRefresh?.();
    } catch (e) {
      requestBtn.disabled = false;
      approveBtn.disabled = false;
      status.className = "modal-status error";
      status.textContent = t("review-decision-error", { error: e });
    }
  }

  requestBtn.addEventListener("click", () => decide("pedir_alteracoes", requestBtn));
  approveBtn.addEventListener("click", () => decide("aprovado", approveBtn));

  btnRow.append(requestBtn, approveBtn);
  actions.append(btnRow, status);
  return actions;
}

// Load the learnings the execution agent proposed for this task, shown
// only in review. Each can be promoted (added to project memory) or
// discarded; neither touches the task's own state.
async function loadSuggestedLearnings(taskId, estado) {
  const myGen = ++learningsLoadGen;
  const myOpen = openGen; // capture the current open generation
  learningsSection.hidden = true;
  learningsListEl.replaceChildren();
  // Each terminal path sets `learningsAvailable` then re-syncs the tabs, under
  // both the learnings-load and open generation guards.
  const settle = (available) => {
    if (myGen !== learningsLoadGen || myOpen !== openGen) return;
    learningsAvailable = available;
    syncTaskTabs();
  };
  if (estado !== "aguardando_revisao") {
    settle(false);
    return;
  }
  let projectId = null;
  let suggestions = [];
  try {
    const mapping = await invoke("list_task_projects");
    projectId = mapping?.[taskId] || null;
    if (!projectId) {
      settle(false);
      return;
    }
    suggestions = await invoke("list_memory_suggestions", { projectId });
  } catch {
    settle(false);
    return;
  }
  if (myGen !== learningsLoadGen || myOpen !== openGen) return;
  // Only this task's learnings — filter to aprendizado suggestions whose
  // origin is this task.
  const learnings = (suggestions ?? []).filter(
    (s) => s.kind?.tipo === "aprendizado" && s.kind.origem_task === taskId,
  );
  if (learnings.length === 0) {
    settle(false); // keep the section hidden when none
    return;
  }
  learningsSection.hidden = false;
  learningsEmptyEl.hidden = true;
  for (const s of learnings) {
    learningsListEl.append(makeLearningRow(s, myOpen));
  }
  settle(true);
}

function makeLearningRow(s, rowOpen) {
  const li = document.createElement("li");
  li.className = "learning-item";

  const text = document.createElement("span");
  text.className = "learning-item-text";
  text.textContent = s.kind.texto;
  li.append(text);

  const actions = document.createElement("div");
  actions.className = "learning-item-actions";

  const promote = document.createElement("button");
  promote.type = "button";
  promote.className = "btn btn-sm btn-primary";
  promote.textContent = t("task-learnings-promote") || "Promover";
  promote.addEventListener("click", () => resolveLearning(s.id, true, li, rowOpen));

  const discard = document.createElement("button");
  discard.type = "button";
  discard.className = "btn btn-sm";
  discard.textContent = t("task-learnings-discard") || "Descartar";
  discard.addEventListener("click", () => resolveLearning(s.id, false, li, rowOpen));

  actions.append(promote, discard);
  li.append(actions);
  return li;
}

async function resolveLearning(suggestionId, aprovar, li, rowOpen) {
  try {
    await invoke("resolve_memory_suggestion", { suggestionId, aprovar });
    // A newer task may have opened in this modal while the resolve was in
    // flight (rowOpen captured when the row was built). If so, the DOM/flags
    // now belong to that task — don't touch them.
    if (rowOpen !== openGen) return;
    li.remove();
    if (!learningsListEl.children.length) {
      // Nothing left — hide the section so the review reads as cleared, and
      // drop the Revisão tab if review is also unavailable.
      learningsSection.hidden = true;
      learningsAvailable = false;
      syncTaskTabs();
    }
  } catch (e) {
    setStatus(typeof e === "string" ? e : t("task-error", { error: e }), "error");
  }
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


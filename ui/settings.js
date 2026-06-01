// Settings modal — abas Geral / Agentes / Projeto.
//
// State flow:
//   open() → get_config → populate form → user edits → submit →
//   save_config (which also hot-swaps the in-memory copy and persists
//   the JSON file). Locale changes apply *immediately* on select
//   change (don't wait for Save) so the UI reflects the choice live.
//
// Layout: three horizontal tabs under the header.
//   Geral   → Idioma + Armazenamento (files/sqlite/postgres).
//   Agentes → Agente padrão + Modelos + Skills do CLI (escopo global).
//   Projeto → projeto selecionado: editar Nome/Caminho, override de
//             agente, e Skills do CLI (escopo projeto).
// A single Save in the footer persists everything; actions that are
// already immediate (locale, storage backend, skill install/remove,
// model discovery) stay immediate.

import { loadLocale, t } from "./i18n.js";
import {
  loadAgentPresence,
  decorateKindSelect,
  onAgentPresenceRefresh,
} from "./agent-presence.js";
import { PROJECT_COLORS } from "./project-colors.js";
import { renderProjectMemory } from "./project-memory.js";

const { invoke } = window.__TAURI__.core;

const SVG_NS = "http://www.w3.org/2000/svg";

// Build an icon-only button (a Lucide-style line icon via the #ic-* sprite)
// whose text label shows only as a hover tooltip. Keeps dense, repetitive
// row actions compact while staying accessible (title + aria-label).
function iconButton(symbolId, label, { primary = false } = {}) {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "btn btn-icon btn-icon-sm" + (primary ? " btn-primary" : "");
  btn.title = label;
  btn.setAttribute("aria-label", label);
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "ic");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", "15");
  svg.setAttribute("height", "15");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(SVG_NS, "use");
  use.setAttribute("href", `#${symbolId}`);
  svg.append(use);
  btn.append(svg);
  return btn;
}

const dialog = document.getElementById("settings-modal");
const form = document.getElementById("settings-form");
const statusEl = document.getElementById("settings-status");
const agentCommandEl = document.getElementById("agent-command");
const localeSelectEl = document.getElementById("settings-locale");
const agentKindSelectEl = document.getElementById("settings-agent-kind");
const storageRestartBanner = document.getElementById("storage-restart-banner");
const pgBlock = document.getElementById("pg-config-block");
const pgStatusEl = document.getElementById("pg-status");
const pgSaveBtn = document.getElementById("btn-pg-save");
const modelsBodyEl = document.getElementById("settings-models-body");
const modelsStatusEl = document.getElementById("settings-models-status");

// Project tab elements.
const projectTabSelectEl = document.getElementById("project-tab-select");
const projectEmptyEl = document.getElementById("project-empty");
const projectDetailEl = document.getElementById("project-detail");
const projectEditNameEl = document.getElementById("project-edit-name");
const projectEditPathEl = document.getElementById("project-edit-path");
const projectEditIdEl = document.getElementById("project-edit-id");
const projectAgentKindEl = document.getElementById("project-agent-kind");
const projectAgentCommandEl = document.getElementById("project-agent-command");
const projectAgentCommandFieldEl = document.getElementById("project-agent-command-field");
const projectEditDefaultBranchEl = document.getElementById("project-edit-default-branch");
const btnProjectNew = document.getElementById("btn-project-new");
const btnProjectRemove = document.getElementById("btn-project-remove");
const btnProjectEditBrowse = document.getElementById("btn-project-edit-browse");
const projectColorSwatchesEl = document.getElementById("project-color-swatches");

let currentConfig = blankConfig();
// Which project the Projeto tab is showing. Tracked separately from
// config.active_project_id (which is the board filter) — the tab just
// needs *a* project to edit, defaulting to the active one.
let selectedProjectId = null;
let _refreshCallback = null;

export function setSettingsRefreshCallback(fn) {
  _refreshCallback = fn;
}
// Remember the backend at open() time. We only flag "restart needed"
// when the user actually changes it — re-opening the modal with the
// same backend shouldn't show a stale banner from a previous session.
let openingBackend = "files";
// "Salvar e migrar" only enables after a successful `test_db_connection`
// against the exact field values currently in the form.
let pgTestPassedFor = null;

function blankConfig() {
  return {
    data_version: 1,
    locale: null,
    skill_locale: null,
    projects: [],
    agente: null,
    storage_backend: "files",
    postgres: null,
  };
}

export async function openSettings() {
  setStatus("");
  try {
    currentConfig = (await invoke("get_config")) ?? blankConfig();
  } catch (e) {
    currentConfig = blankConfig();
    setStatus(t("settings-save-error", { error: e }), "error");
  }
  activateTab("geral");
  populateForm(currentConfig);
  await applyAgentPresence();
  if (!dialog.open) dialog.showModal();
}

async function applyAgentPresence() {
  // Force a fresh probe: the presence cache lives for the whole app
  // session, so without this an agent installed since boot would stay
  // flagged "(not installed)" — and the start-agent hard-block would
  // keep refusing it — until a full restart.
  const map = await loadAgentPresence({ force: true });
  decorateKindSelect(agentKindSelectEl, map);
  decorateKindSelect(projectAgentKindEl, map);
}

// Re-decorate when the locale flips while the modal is open. The
// translation pass overwrites the option labels, so the "(not
// installed)" suffix has to be re-stamped each time.
onAgentPresenceRefresh(() => {
  if (dialog.open) applyAgentPresence();
});

export function closeSettings() {
  if (dialog.open) dialog.close();
}

// ──────────────────────────────── tabs ──────────────────────────────

const tabButtons = [...document.querySelectorAll(".settings-tab")];
const tabPanels = [...document.querySelectorAll(".settings-panel")];

function activateTab(name) {
  for (const b of tabButtons) {
    const active = b.dataset.tab === name;
    b.classList.toggle("is-active", active);
    b.setAttribute("aria-selected", active ? "true" : "false");
    b.tabIndex = active ? 0 : -1;
  }
  for (const p of tabPanels) {
    p.hidden = p.dataset.panel !== name;
  }
}

for (const b of tabButtons) {
  b.addEventListener("click", () => activateTab(b.dataset.tab));
}

// ───────────────────────────── populate ─────────────────────────────

function populateForm(cfg) {
  localeSelectEl.value = cfg.locale ?? "pt-BR";
  agentKindSelectEl.value = cfg.agente?.kind ?? "claude_code";
  agentCommandEl.value = cfg.agente?.command ?? "";

  const backend = cfg.storage_backend ?? "files";
  openingBackend = backend;
  const radio = document.querySelector(
    `input[name="storage-backend"][value="${backend}"]`,
  );
  if (radio) radio.checked = true;
  storageRestartBanner.hidden = true;

  populatePgForm(cfg.postgres);
  pgBlock.hidden = backend !== "postgres";

  // Reset the project tab selection so a deleted/renamed project from a
  // previous session doesn't linger; renderProjectTab picks a default.
  selectedProjectId = null;
  renderProjectTab();
  globalSkills.refresh();
  refreshModelsStatus();
}

function populatePgForm(pg) {
  document.getElementById("pg-host").value = pg?.host ?? "";
  document.getElementById("pg-port").value = pg?.port ?? 5432;
  document.getElementById("pg-database").value = pg?.database ?? "";
  document.getElementById("pg-user").value = pg?.user ?? "";
  // Password field always starts empty — the value lives in the
  // keyring, never in JS state, and prefilling would leak it into the
  // DOM. The user re-enters it on every modal session.
  document.getElementById("pg-password").value = "";
  document.getElementById("pg-ssl-mode").value = pg?.ssl_mode ?? "require";
  pgTestPassedFor = null;
  pgSaveBtn.disabled = true;
  setPgStatus("");
}

function readPgForm() {
  return {
    host: document.getElementById("pg-host").value.trim(),
    port: Number(document.getElementById("pg-port").value) || 5432,
    database: document.getElementById("pg-database").value.trim(),
    user: document.getElementById("pg-user").value.trim(),
    password: document.getElementById("pg-password").value,
    ssl_mode: document.getElementById("pg-ssl-mode").value || "require",
  };
}

function pgFormFingerprint(form) {
  // Used to invalidate a prior "test passed" verdict when any field
  // changes — without this, the user could tweak host/db after a green
  // test and the save would use stale credentials.
  return [form.host, form.port, form.database, form.user, form.password, form.ssl_mode].join("|");
}

function setPgStatus(msg, kind) {
  pgStatusEl.textContent = msg ?? "";
  pgStatusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

// ────────────────────────────── projeto tab ─────────────────────────
//
// The Projeto tab edits the *selected* project in place: name, path,
// and the per-project agent override (`Project.agente`, surfaced here
// for the first time — the backend already honors it at spawn time).
// Edits write straight into `currentConfig.projects[i]`; the footer
// Save persists them.

function projectById(id) {
  return (currentConfig.projects ?? []).find((p) => p.id === id) ?? null;
}

function currentProject() {
  return projectById(selectedProjectId);
}

function renderProjectTab() {
  const projects = currentConfig.projects ?? [];
  const ids = projects.map((p) => p.id);

  // Keep the prior selection if still valid; else prefer the active
  // project, then fall back to the first entry.
  if (!ids.includes(selectedProjectId)) {
    selectedProjectId =
      currentConfig.active_project_id && ids.includes(currentConfig.active_project_id)
        ? currentConfig.active_project_id
        : ids[0] ?? null;
  }

  projectTabSelectEl.replaceChildren();
  for (const p of projects) {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = p.name || p.id;
    projectTabSelectEl.append(opt);
  }

  const empty = projects.length === 0;
  projectTabSelectEl.hidden = empty;
  projectTabSelectEl.disabled = empty;
  projectEmptyEl.hidden = !empty;
  projectDetailEl.hidden = empty;
  // Guard: never let the user remove the last project.
  btnProjectRemove.disabled = projects.length <= 1;

  if (selectedProjectId) {
    projectTabSelectEl.value = selectedProjectId;
    renderProjectDetail();
  } else {
    // No project to show: still refresh the (empty) skills table and
    // hide the per-project memory section.
    projectSkills.refresh();
    renderProjectMemory(null);
  }
}

function renderProjectDetail() {
  const p = currentProject();
  if (!p) return;
  projectEditNameEl.value = p.name ?? "";
  projectEditPathEl.value = p.path ?? "";
  projectEditIdEl.textContent = p.id;
  projectAgentKindEl.value = p.agente?.kind ?? "";
  projectAgentCommandEl.value = p.agente?.command ?? "";
  projectEditDefaultBranchEl.value = p.default_branch ?? "";
  updateProjectAgentCommandVisibility();

  // Rebuild color swatches for the selected project.
  projectColorSwatchesEl.replaceChildren();
  for (const [key, hex] of Object.entries(PROJECT_COLORS)) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "project-color-swatch" + (p.color === key ? " selected" : "");
    btn.style.background = hex;
    btn.title = key;
    btn.addEventListener("click", () => {
      p.color = p.color === key ? null : key;
      renderProjectDetail();
    });
    projectColorSwatchesEl.append(btn);
  }

  projectSkills.refresh();
  renderProjectMemory(p.id);
}

// The command field only makes sense once an explicit agent kind is
// chosen — "(herda global)" means "no override", so hide it.
function updateProjectAgentCommandVisibility() {
  projectAgentCommandFieldEl.hidden = !projectAgentKindEl.value;
}

// Fold the override select + command input back into `Project.agente`.
// Empty kind → null (inherit global). Empty command → null (PATH lookup).
function applyProjectAgent(p) {
  const kind = projectAgentKindEl.value;
  if (!kind) {
    p.agente = null;
    return;
  }
  const cmd = projectAgentCommandEl.value.trim();
  p.agente = { kind, command: cmd === "" ? null : cmd };
}

projectTabSelectEl.addEventListener("change", () => {
  selectedProjectId = projectTabSelectEl.value;
  renderProjectDetail();
});

projectEditNameEl.addEventListener("input", () => {
  const p = currentProject();
  if (!p) return;
  p.name = projectEditNameEl.value;
  // Keep the selector label in sync as the user types.
  const opt = [...projectTabSelectEl.options].find((o) => o.value === p.id);
  if (opt) opt.textContent = p.name || p.id;
});

// Writing the path on every keystroke is cheap; refreshing the skill
// status (a backend round-trip) is not — defer that to `change`/blur.
projectEditPathEl.addEventListener("input", () => {
  const p = currentProject();
  if (!p) return;
  p.path = projectEditPathEl.value;
});
projectEditPathEl.addEventListener("change", () => {
  if (currentProject()) projectSkills.refresh();
});

projectEditDefaultBranchEl.addEventListener("input", () => {
  const p = currentProject();
  if (!p) return;
  // Empty → null so an unset default falls back to the repo's current branch.
  p.default_branch = projectEditDefaultBranchEl.value.trim() || null;
});

projectAgentKindEl.addEventListener("change", () => {
  const p = currentProject();
  if (!p) return;
  applyProjectAgent(p);
  updateProjectAgentCommandVisibility();
});
projectAgentCommandEl.addEventListener("input", () => {
  const p = currentProject();
  if (p) applyProjectAgent(p);
});

btnProjectNew.addEventListener("click", () => {
  const name = t("settings-project-new-name");
  const id = generateProjectId(name, currentConfig.projects ?? []);
  const usedColors = new Set((currentConfig.projects ?? []).map((p) => p.color).filter(Boolean));
  const nextColor = Object.keys(PROJECT_COLORS).find((k) => !usedColors.has(k)) ?? null;
  currentConfig.projects = [
    ...(currentConfig.projects ?? []),
    { id, name, path: "", agente: null, color: nextColor },
  ];
  selectedProjectId = id;
  renderProjectTab();
  // Drop the user straight into renaming the fresh project.
  projectEditNameEl.focus();
  projectEditNameEl.select();
  setStatus("");
});

btnProjectRemove.addEventListener("click", () => {
  if ((currentConfig.projects ?? []).length <= 1) {
    setStatus(t("settings-projects-delete-last-error"), "error");
    return;
  }
  currentConfig.projects = currentConfig.projects.filter((p) => p.id !== selectedProjectId);
  selectedProjectId = null; // renderProjectTab picks a fallback
  renderProjectTab();
  setStatus("");
});

// Folder picker for the selected project's path. The input stays
// editable so users can still type or paste a path.
btnProjectEditBrowse.addEventListener("click", async () => {
  try {
    const selected = await invoke("plugin:dialog|open", {
      options: { directory: true, multiple: false },
    });
    if (typeof selected === "string" && selected.length > 0) {
      projectEditPathEl.value = selected;
      const p = currentProject();
      if (p) {
        p.path = selected;
        projectSkills.refresh();
      }
    }
  } catch (e) {
    setStatus(t("task-error", { error: e }), "error");
  }
});

function generateProjectId(name, existing) {
  const slug = name
    .toLowerCase()
    .normalize("NFD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 24) || "project";
  // Random suffix avoids collisions; loop is theoretical (4 chars ≈ 1.6M)
  // but cheap insurance.
  for (let i = 0; i < 8; i++) {
    const suffix = Math.floor(Math.random() * 36 * 36 * 36 * 36)
      .toString(36)
      .padStart(4, "0");
    const candidate = `${slug}-${suffix}`;
    if (!existing.some((p) => p.id === candidate)) return candidate;
  }
  return `${slug}-${Date.now().toString(36)}`;
}

function readForm() {
  const locale = localeSelectEl.value || currentConfig.locale || "pt-BR";
  const agentKind = agentKindSelectEl.value || "claude_code";
  const commandRaw = agentCommandEl.value.trim();
  const backendRadio = document.querySelector(
    'input[name="storage-backend"]:checked',
  );
  const storageBackend = backendRadio?.value ?? currentConfig.storage_backend ?? "files";

  // Spread currentConfig first so fields the form doesn't surface —
  // postgres, active_project_id, agent_models — survive the Save instead
  // of being dropped (save_config overwrites the whole config).
  return {
    ...currentConfig,
    data_version: currentConfig.data_version ?? 1,
    locale,
    skill_locale: currentConfig.skill_locale ?? null,
    projects: currentConfig.projects ?? [],
    agente: {
      kind: agentKind,
      command: commandRaw === "" ? null : commandRaw,
    },
    storage_backend: storageBackend,
  };
}

function setStatus(msg, kind) {
  statusEl.textContent = msg ?? "";
  statusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

// ─────────────────────────── models menu ────────────────────────────
//
// Model discovery is the slow (~15 s per agent) `/model` PTY probe. It
// lives here — triggered explicitly per agent via "Carregar" — instead
// of in the start-agent modal, which only reads the cached result. The
// backend persists discovered lists to config.json so they survive
// restarts (cached_only reads return them instantly).

const MODEL_KIND_LABELS = {
  claude_code: "settings-skills-agent-claude",
  codex: "settings-skills-agent-codex",
  copilot: "settings-skills-agent-copilot",
  antigravity: "settings-skills-agent-antigravity",
  opencode: "settings-skills-agent-opencode",
};

function setModelsStatus(msg, kind) {
  modelsStatusEl.textContent = msg ?? "";
  modelsStatusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

// Render one row per agent kind from the *cached* lists only (no probe),
// so opening Settings is instant.
async function refreshModelsStatus() {
  modelsBodyEl.replaceChildren();
  for (const kind of ["claude_code", "codex", "copilot", "antigravity", "opencode"]) {
    let entries = [];
    try {
      entries = await invoke("list_agent_models", { agentKind: kind, cachedOnly: true });
    } catch {
      entries = [];
    }

    const tr = document.createElement("tr");

    const tdAgent = document.createElement("td");
    tdAgent.textContent = t(MODEL_KIND_LABELS[kind]);

    const tdCount = document.createElement("td");
    if (entries.length) {
      tdCount.textContent = t("settings-models-loaded", { count: entries.length });
      tdCount.className = "skill-status-yes";
    } else {
      tdCount.textContent = t("settings-models-none");
      tdCount.className = "skill-status-no";
    }

    const tdCurrent = document.createElement("td");
    const current = entries.find((m) => m.current);
    tdCurrent.textContent = current ? current.label || current.id : "—";

    const tdAction = document.createElement("td");
    tdAction.className = "skill-action-cell";
    const btn = iconButton("ic-download", t("settings-models-load") || "Carregar");
    btn.addEventListener("click", () => loadModelsForKind(kind));
    tdAction.append(btn);

    tr.append(tdAgent, tdCount, tdCurrent, tdAction);
    modelsBodyEl.append(tr);
  }
}

// Run the discovery probe for one agent kind (refresh=true), then
// re-render the cached view.
async function loadModelsForKind(kind) {
  setModelsStatus(t("settings-models-loading") || "Carregando modelos…");
  try {
    await invoke("list_agent_models", { agentKind: kind, refresh: true });
    setModelsStatus("");
  } catch (e) {
    setModelsStatus(
      typeof e === "string" ? e : t("settings-skills-error", { error: e }),
      "error",
    );
  }
  await refreshModelsStatus();
}

// ─────────────────────── locale / storage wiring ────────────────────

// Live locale switching — applies before Save so the modal itself
// re-renders in the chosen language as the user picks an option.
localeSelectEl.addEventListener("change", async () => {
  try {
    const applied = await invoke("set_locale", { locale: localeSelectEl.value });
    await loadLocale(applied);
    currentConfig.locale = applied;
    localeSelectEl.value = applied;
    // The translation pass re-stamps data-i18n labels, but the JS-built
    // tables (projects, skills, models) need an explicit re-render.
    renderProjectTab();
    globalSkills.refresh();
    refreshModelsStatus();
  } catch (e) {
    setStatus(t("settings-save-error", { error: e }), "error");
  }
});

// Manual update check (Geral tab). The app already checks silently on
// boot and every 24h; this is an on-demand check that, unlike the silent
// path, reports the up-to-date case too. When a new version is found the
// command re-emits `update_available`, so the same banner still appears
// on top of the inline status here.
const updateStatusEl = document.getElementById("update-check-status");
function setUpdateStatus(msg, kind) {
  updateStatusEl.textContent = msg ?? "";
  updateStatusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}
document.getElementById("btn-check-update").addEventListener("click", async () => {
  const btn = document.getElementById("btn-check-update");
  btn.disabled = true;
  setUpdateStatus(t("settings-update-checking"));
  try {
    const res = await invoke("check_update");
    if (res && res.status === "available") {
      setUpdateStatus(t("settings-update-available", { version: res.version }), "ok");
    } else {
      setUpdateStatus(t("settings-update-uptodate"), "ok");
    }
  } catch (e) {
    setUpdateStatus(t("settings-update-error", { error: e }), "error");
  } finally {
    btn.disabled = false;
  }
});

// Close buttons (header × and footer Cancel)
document
  .querySelectorAll('[data-action="close-settings"]')
  .forEach((b) => b.addEventListener("click", closeSettings));

// Storage backend — radio change. For files/sqlite the switch is
// immediate (set_storage_backend writes config.json; restart applies
// it). For postgres we defer until the user fills the form and the
// connection test passes, because saving "postgres" with no keyring
// entry would just fall back to files on next boot.
for (const radio of document.querySelectorAll('input[name="storage-backend"]')) {
  radio.addEventListener("change", async () => {
    if (radio.disabled || !radio.checked) return;
    if (radio.value === "postgres") {
      pgBlock.hidden = false;
      // Don't touch storage_backend yet — that happens in btn-pg-save.
      return;
    }
    pgBlock.hidden = true;
    try {
      const saved = await invoke("set_storage_backend", { backend: radio.value });
      currentConfig = saved;
      storageRestartBanner.hidden = radio.value === openingBackend;
    } catch (e) {
      setStatus(t("settings-save-error", { error: e }), "error");
    }
  });
}

document.getElementById("btn-restart-now").addEventListener("click", async () => {
  try {
    await invoke("restart_app");
  } catch (e) {
    setStatus(t("settings-save-error", { error: e }), "error");
  }
});

// PG form — any edit invalidates a prior green test. The save button
// stays disabled until the next successful test_db_connection.
for (const id of ["pg-host", "pg-port", "pg-database", "pg-user", "pg-password", "pg-ssl-mode"]) {
  document.getElementById(id).addEventListener("input", () => {
    pgTestPassedFor = null;
    pgSaveBtn.disabled = true;
  });
}

document.getElementById("btn-pg-test").addEventListener("click", async () => {
  const form = readPgForm();
  if (!form.host || !form.database || !form.user || !form.password) {
    setPgStatus(t("settings-pg-fields-required"), "error");
    return;
  }
  setPgStatus(t("settings-pg-testing"));
  try {
    await invoke("test_db_connection", form);
    pgTestPassedFor = pgFormFingerprint(form);
    pgSaveBtn.disabled = false;
    setPgStatus(t("settings-pg-test-ok"), "ok");
  } catch (e) {
    pgTestPassedFor = null;
    pgSaveBtn.disabled = true;
    setPgStatus(t("settings-pg-test-error", { error: e }), "error");
  }
});

document.getElementById("btn-pg-save").addEventListener("click", async () => {
  const form = readPgForm();
  if (pgTestPassedFor !== pgFormFingerprint(form)) {
    setPgStatus(t("settings-pg-stale"), "error");
    pgSaveBtn.disabled = true;
    return;
  }
  try {
    // Step 1: password to keyring. Done BEFORE the config save so a
    // crash between the two leaves a usable keyring entry instead of
    // a config pointing at a missing secret.
    await invoke("set_pg_password", {
      host: form.host,
      port: form.port,
      database: form.database,
      user: form.user,
      password: form.password,
    });

    // Step 2: persist the connection settings (no password) +
    // switch storage_backend in one shot via save_config so they
    // stay in sync.
    const pgConfig = {
      host: form.host,
      port: form.port,
      database: form.database,
      user: form.user,
      ssl_mode: form.ssl_mode,
    };
    const next = {
      ...readForm(),
      storage_backend: "postgres",
      postgres: pgConfig,
    };
    const saved = await invoke("save_config", { config: next });
    currentConfig = saved;

    setPgStatus(t("settings-pg-saved"), "ok");
    storageRestartBanner.hidden = openingBackend === "postgres";
  } catch (e) {
    setPgStatus(t("settings-save-error", { error: e }), "error");
  }
});

document.getElementById("btn-pg-clear").addEventListener("click", async () => {
  const form = readPgForm();
  if (!form.host || !form.database || !form.user) {
    setPgStatus(t("settings-pg-fields-required"), "error");
    return;
  }
  try {
    await invoke("clear_pg_password", {
      host: form.host,
      port: form.port,
      database: form.database,
      user: form.user,
    });
    document.getElementById("pg-password").value = "";
    pgTestPassedFor = null;
    pgSaveBtn.disabled = true;
    setPgStatus(t("settings-pg-cleared"), "ok");
  } catch (e) {
    setPgStatus(t("settings-save-error", { error: e }), "error");
  }
});

// ─────────────────────── skills (CLI snippet) ────────────────────────
//
// The Settings modal lets the user push the cadenza-cli usage snippet
// into Claude Code, Codex, Copilot, Antigravity, and OpenCode. All filesystem work runs
// in the Tauri backend via skills-core; the UI is a thin form + status
// table.
//
// There are two panels with identical shape but fixed scope: the
// Agentes tab installs *global* (user home); the Projeto tab installs
// into the *selected project*. `createSkillsPanel` builds one from a
// prefix + scope, so the two share all behavior.
//
// Locale is the app's active locale (resolved server-side in
// skill_install) — switching the app language is the way to install in
// a different language.

function summarizeOutcomes(outcomes) {
  // One-liner suitable for the status area. We don't try to translate
  // each individual outcome — the per-row state lives in the table.
  const counts = { installed: 0, removed: 0, skipped: 0 };
  for (const o of outcomes) {
    counts[o.action] = (counts[o.action] ?? 0) + 1;
  }
  const parts = [];
  if (counts.installed) parts.push(t("settings-skills-summary-installed", { count: counts.installed }));
  if (counts.removed) parts.push(t("settings-skills-summary-removed", { count: counts.removed }));
  if (counts.skipped) parts.push(t("settings-skills-summary-skipped", { count: counts.skipped }));
  return parts.join(" · ");
}

// Builds a skills panel bound to its own DOM (by `prefix`) and a fixed
// `scope`. `getProjectPath` supplies the absolute path for the project
// scope (null/unused for global).
function createSkillsPanel({ prefix, scope, getProjectPath }) {
  const statusMsgEl = document.getElementById(`skill-${prefix}-status`);
  const tableBodyEl = document.getElementById(`skill-${prefix}-status-body`);

  function setSkillStatus(msg, kind) {
    statusMsgEl.textContent = msg ?? "";
    statusMsgEl.className = "modal-status" + (kind ? ` ${kind}` : "");
  }

  function projectPath() {
    return scope === "project" ? getProjectPath?.() ?? null : null;
  }

  // Install/remove a single agent's snippet (one table row). `force`
  // overwrites an existing install ("Atualizar"); without it a present
  // snippet is skipped.
  async function runAction(op, agent, force) {
    if (scope === "project" && !projectPath()) {
      setSkillStatus(t("settings-skills-project-required"), "error");
      return;
    }
    setSkillStatus(t("settings-skills-running"));
    try {
      const args = { agents: [agent], scope, force: Boolean(force) };
      const path = projectPath();
      if (path) args.project_path = path;
      const outcomes = await invoke(op, args);
      setSkillStatus(summarizeOutcomes(outcomes) || t("settings-saved"), "ok");
      await refresh();
    } catch (e) {
      setSkillStatus(t("settings-skills-error", { error: e }), "error");
    }
  }

  async function refresh() {
    tableBodyEl.replaceChildren();
    let rows;
    try {
      const args = {};
      const path = projectPath();
      if (path) args.project_path = path;
      rows = await invoke("skill_status", args);
    } catch (e) {
      setSkillStatus(t("settings-skills-error", { error: e }), "error");
      return;
    }
    // Each panel owns one scope — show only its rows so the global and
    // project tables don't echo each other.
    for (const r of rows.filter((r) => r.scope === scope)) {
      const tr = document.createElement("tr");

      const tdAgent = document.createElement("td");
      tdAgent.textContent = t(`settings-skills-agent-${r.agent}`);

      const tdScope = document.createElement("td");
      tdScope.textContent = t(`settings-skills-scope-${r.scope}`);

      const tdStatus = document.createElement("td");
      if (r.installed) {
        const installedText = r.locale
          ? t("settings-skills-status-installed-locale", { locale: r.locale })
          : t("settings-skills-status-installed");
        if (r.outdated) {
          tdStatus.textContent = `${installedText} — ${t("settings-skills-status-outdated")}`;
          tdStatus.className = "skill-status-outdated";
        } else {
          tdStatus.textContent = installedText;
          tdStatus.className = "skill-status-yes";
        }
      } else {
        tdStatus.textContent = t("settings-skills-status-not-installed");
        tdStatus.className = "skill-status-no";
      }

      const tdPath = document.createElement("td");
      tdPath.className = "skill-path";
      tdPath.textContent = r.path;
      tdPath.title = r.path;

      // Per-row primary action: install (download icon) when absent,
      // update (refresh icon, force-overwrite) when already installed.
      const tdAction = document.createElement("td");
      tdAction.className = "skill-action-cell";
      const actBtn = iconButton(
        r.installed ? "ic-refresh" : "ic-download",
        r.installed ? t("settings-skills-update") : t("settings-skills-install"),
        // Highlight the action when there's nothing installed yet, or when
        // an installed copy is outdated (force-reinstall picks up the new
        // skill body).
        { primary: !r.installed || r.outdated },
      );
      actBtn.addEventListener("click", () =>
        runAction("skill_install", r.agent, r.installed),
      );
      tdAction.append(actBtn);

      // Per-row remove (trash icon) — only meaningful once installed.
      const tdRemove = document.createElement("td");
      tdRemove.className = "skill-action-cell";
      const rmBtn = iconButton("ic-trash", t("settings-skills-remove"));
      rmBtn.disabled = !r.installed;
      rmBtn.addEventListener("click", () => runAction("skill_remove", r.agent, false));
      tdRemove.append(rmBtn);

      tr.append(tdAgent, tdScope, tdStatus, tdPath, tdAction, tdRemove);
      tableBodyEl.append(tr);
    }
  }

  document
    .getElementById(`btn-skill-${prefix}-refresh`)
    .addEventListener("click", async () => {
      setSkillStatus("");
      await refresh();
    });

  return { refresh };
}

const globalSkills = createSkillsPanel({ prefix: "g", scope: "global" });
const projectSkills = createSkillsPanel({
  prefix: "p",
  scope: "project",
  getProjectPath: () => currentProject()?.path || null,
});

document.getElementById("btn-models-refresh").addEventListener("click", async () => {
  setModelsStatus("");
  await refreshModelsStatus();
});

// Save
form.addEventListener("submit", async (e) => {
  e.preventDefault();
  const cfg = readForm();
  try {
    const saved = await invoke("save_config", { config: cfg });
    currentConfig = saved;
    setStatus(t("settings-saved"), "ok");
    // Tiny grace period so the success message is visible, then close.
    setTimeout(() => {
      setStatus("");
      closeSettings();
      _refreshCallback?.();
    }, 700);
  } catch (err) {
    setStatus(t("settings-save-error", { error: err }), "error");
  }
});

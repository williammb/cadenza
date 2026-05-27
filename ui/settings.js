// Settings modal — Idioma / Projects / Agente padrão.
//
// State flow:
//   open() → get_config → populate form → user edits → submit →
//   save_config (which also hot-swaps the in-memory copy and persists
//   the JSON file). Locale changes apply *immediately* on select
//   change (don't wait for Save) so the UI reflects the choice live.

import { loadLocale, t } from "./i18n.js";

const { invoke } = window.__TAURI__.core;

const dialog = document.getElementById("settings-modal");
const form = document.getElementById("settings-form");
const statusEl = document.getElementById("settings-status");
const projectListEl = document.getElementById("project-list");
const agentCommandEl = document.getElementById("agent-command");
const localeSelectEl = document.getElementById("settings-locale");
const agentKindSelectEl = document.getElementById("settings-agent-kind");
const projectPathEl = document.getElementById("new-project-path");
const storageRestartBanner = document.getElementById("storage-restart-banner");
const pgBlock = document.getElementById("pg-config-block");
const pgStatusEl = document.getElementById("pg-status");
const pgSaveBtn = document.getElementById("btn-pg-save");
const skillAgentClaudeEl = document.getElementById("skill-agent-claude");
const skillAgentCodexEl = document.getElementById("skill-agent-codex");
const skillForceEl = document.getElementById("skill-force");
const skillStatusEl = document.getElementById("skill-status");
const skillStatusTableBodyEl = document.getElementById("skill-status-table-body");
const skillProjectPickerEl = document.getElementById("skill-project-picker");
const skillProjectSelectEl = document.getElementById("skill-project-select");
const skillProjectEmptyEl = document.getElementById("skill-project-empty");

let currentConfig = blankConfig();
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
  populateForm(currentConfig);
  applySkillScopeVisibility();
  // populateForm → renderProjects already repopulates the skill project
  // select and refreshes status; no need to call them again here.
  if (!dialog.open) dialog.showModal();
}

export function closeSettings() {
  if (dialog.open) dialog.close();
}

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

  renderProjects(cfg.projects ?? []);
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

function renderProjects(projects) {
  projectListEl.replaceChildren();
  // The skill-scope project picker reads from currentConfig.projects;
  // keep it in lockstep with this list so adding/removing a project is
  // immediately reflected in the skills section.
  populateSkillProjectSelect();
  // After repopulating, the selected project may have changed (e.g. the
  // previously picked one was just deleted) — refresh status to match.
  if (readSkillScope() === "project") refreshSkillStatus();
  if (!projects.length) {
    const li = document.createElement("li");
    li.className = "empty-row";
    li.textContent = t("settings-projects-empty");
    projectListEl.append(li);
    return;
  }
  for (const p of projects) {
    const li = document.createElement("li");

    // Two-row meta cell: name on top, auto-generated id beneath
    // (muted, monospace) so users can still see what was minted.
    const meta = document.createElement("span");
    meta.className = "pmeta";
    const name = document.createElement("span");
    name.className = "pname";
    name.textContent = p.name;
    const id = document.createElement("span");
    id.className = "pid";
    id.textContent = p.id;
    meta.append(name, id);

    const path = document.createElement("span");
    path.className = "ppath";
    path.textContent = p.path;

    const del = document.createElement("button");
    del.type = "button";
    del.className = "btn btn-icon";
    del.textContent = "×";
    del.setAttribute("aria-label", t("action-delete"));
    del.addEventListener("click", () => {
      currentConfig.projects = currentConfig.projects.filter((q) => q.id !== p.id);
      renderProjects(currentConfig.projects);
    });

    li.append(meta, path, del);
    projectListEl.append(li);
  }
}

function readForm() {
  const locale = localeSelectEl.value || currentConfig.locale || "pt-BR";
  const agentKind = agentKindSelectEl.value || "claude_code";
  const commandRaw = agentCommandEl.value.trim();
  const backendRadio = document.querySelector(
    'input[name="storage-backend"]:checked',
  );
  const storageBackend = backendRadio?.value ?? currentConfig.storage_backend ?? "files";

  return {
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

// ─────────────────────────── event wiring ───────────────────────────

// Add-project button — id is auto-generated from the name so the user
// only fills name + path. We slugify (lowercase, strip diacritics,
// keep [a-z0-9-]) and tack on a 4-char random suffix to keep ids
// unique even if two projects share a slug.
document.getElementById("btn-add-project").addEventListener("click", () => {
  const name = document.getElementById("new-project-name").value.trim();
  const path = document.getElementById("new-project-path").value.trim();
  if (!name || !path) {
    setStatus(t("task-error", { error: "name/path required" }), "error");
    return;
  }
  const id = generateProjectId(name, currentConfig.projects ?? []);
  currentConfig.projects = [
    ...(currentConfig.projects ?? []),
    { id, name, path, agente: null },
  ];
  renderProjects(currentConfig.projects);
  document.getElementById("new-project-name").value = "";
  document.getElementById("new-project-path").value = "";
  setStatus("");
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

// Live locale switching — applies before Save so the modal itself
// re-renders in the chosen language as the user picks an option.
localeSelectEl.addEventListener("change", async () => {
  try {
    const applied = await invoke("set_locale", { locale: localeSelectEl.value });
    await loadLocale(applied);
    currentConfig.locale = applied;
    localeSelectEl.value = applied;
    renderProjects(currentConfig.projects ?? []);
  } catch (e) {
    setStatus(t("settings-save-error", { error: e }), "error");
  }
});

// Folder picker — the browse button opens the native directory
// dialog. The input itself stays editable so users can still type or
// paste a path; the dialog is a convenience, not the only entry.
async function pickProjectPath() {
  try {
    const selected = await invoke("plugin:dialog|open", {
      options: { directory: true, multiple: false },
    });
    if (typeof selected === "string" && selected.length > 0) {
      projectPathEl.value = selected;
    }
  } catch (e) {
    setStatus(t("task-error", { error: e }), "error");
  }
}

document
  .getElementById("btn-browse-path")
  .addEventListener("click", pickProjectPath);

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
// into Claude Code (~/.claude/skills/cadenza/SKILL.md) and Codex
// (AGENTS.md managed block). All filesystem work runs in the Tauri
// backend via skills-core; the UI is a thin form + status table.
//
// Locale is the app's active locale (resolved server-side in
// skill_install) — switching the app language is the way to install in
// a different language.

function readSkillAgents() {
  const agents = [];
  if (skillAgentClaudeEl.checked) agents.push("claude");
  if (skillAgentCodexEl.checked) agents.push("codex");
  return agents;
}

function readSkillScope() {
  const checked = document.querySelector('input[name="skill-scope"]:checked');
  return checked?.value ?? "project";
}

// Returns the absolute path of the project currently picked in the
// skill-scope project select, or null when scope is global / no project
// is configured.
function readSkillProjectPath() {
  if (readSkillScope() !== "project") return null;
  const id = skillProjectSelectEl.value;
  if (!id) return null;
  const proj = (currentConfig.projects ?? []).find((p) => p.id === id);
  return proj?.path ?? null;
}

// Like readSkillProjectPath, but ignores the scope radio — used by the
// status table, whose "Project" row should always reflect the picked
// project regardless of which scope is currently selected for
// install/remove. Without this the backend falls back to current_dir(),
// which is the directory Cadenza itself was launched from.
function selectedProjectPath() {
  const id = skillProjectSelectEl.value;
  if (!id) return null;
  const proj = (currentConfig.projects ?? []).find((p) => p.id === id);
  return proj?.path ?? null;
}

// Build invoke args; only include project_path for project scope so
// the backend can keep its current_dir fallback for the global case.
function skillInvokeArgs(extra) {
  const args = {
    agents: readSkillAgents(),
    scope: readSkillScope(),
    ...(extra ?? {}),
  };
  const projectPath = readSkillProjectPath();
  if (projectPath) args.project_path = projectPath;
  return args;
}

function populateSkillProjectSelect() {
  const projects = currentConfig.projects ?? [];
  const previous = skillProjectSelectEl.value;
  skillProjectSelectEl.replaceChildren();
  for (const p of projects) {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = `${p.name} — ${p.path}`;
    skillProjectSelectEl.append(opt);
  }
  // Restore prior pick if still valid; otherwise prefer the active
  // project from config, then fall back to the first entry.
  const ids = projects.map((p) => p.id);
  let selected = ids.includes(previous) ? previous : null;
  if (!selected && currentConfig.active_project_id && ids.includes(currentConfig.active_project_id)) {
    selected = currentConfig.active_project_id;
  }
  if (!selected && ids.length) selected = ids[0];
  if (selected) skillProjectSelectEl.value = selected;

  const empty = projects.length === 0;
  skillProjectSelectEl.hidden = empty;
  skillProjectSelectEl.disabled = empty;
  skillProjectEmptyEl.hidden = !empty;
}

function applySkillScopeVisibility() {
  skillProjectPickerEl.hidden = readSkillScope() !== "project";
}

function setSkillStatus(msg, kind) {
  skillStatusEl.textContent = msg ?? "";
  skillStatusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

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

async function refreshSkillStatus() {
  skillStatusTableBodyEl.replaceChildren();
  let rows;
  try {
    const args = {};
    const projectPath = selectedProjectPath();
    if (projectPath) args.project_path = projectPath;
    rows = await invoke("skill_status", args);
  } catch (e) {
    setSkillStatus(t("settings-skills-error", { error: e }), "error");
    return;
  }
  for (const r of rows) {
    const tr = document.createElement("tr");

    const tdAgent = document.createElement("td");
    tdAgent.textContent = t(`settings-skills-agent-${r.agent}`);

    const tdScope = document.createElement("td");
    tdScope.textContent = t(`settings-skills-scope-${r.scope}`);

    const tdStatus = document.createElement("td");
    if (r.installed) {
      const label = r.locale
        ? t("settings-skills-status-installed-locale", { locale: r.locale })
        : t("settings-skills-status-installed");
      tdStatus.textContent = label;
      tdStatus.className = "skill-status-yes";
    } else {
      tdStatus.textContent = t("settings-skills-status-not-installed");
      tdStatus.className = "skill-status-no";
    }

    const tdPath = document.createElement("td");
    tdPath.className = "skill-path";
    tdPath.textContent = r.path;
    tdPath.title = r.path;

    tr.append(tdAgent, tdScope, tdStatus, tdPath);
    skillStatusTableBodyEl.append(tr);
  }
}

async function runSkillAction(op, extra) {
  const agents = readSkillAgents();
  if (!agents.length) {
    setSkillStatus(t("settings-skills-no-agent"), "error");
    return;
  }
  const args = skillInvokeArgs(extra);
  if (args.scope === "project" && !args.project_path) {
    setSkillStatus(t("settings-skills-project-required"), "error");
    return;
  }
  setSkillStatus(t("settings-skills-running"));
  try {
    const outcomes = await invoke(op, args);
    const summary = summarizeOutcomes(outcomes);
    setSkillStatus(summary || t("settings-saved"), "ok");
    await refreshSkillStatus();
  } catch (e) {
    setSkillStatus(t("settings-skills-error", { error: e }), "error");
  }
}

document.getElementById("btn-skill-install").addEventListener("click", () => {
  runSkillAction("skill_install", { force: skillForceEl.checked });
});

document.getElementById("btn-skill-remove").addEventListener("click", () => {
  runSkillAction("skill_remove", {});
});

document.getElementById("btn-skill-refresh").addEventListener("click", async () => {
  setSkillStatus("");
  await refreshSkillStatus();
});

skillProjectSelectEl.addEventListener("change", () => {
  setSkillStatus("");
  refreshSkillStatus();
});

for (const radio of document.querySelectorAll('input[name="skill-scope"]')) {
  radio.addEventListener("change", () => {
    if (!radio.checked) return;
    setSkillStatus("");
    applySkillScopeVisibility();
    refreshSkillStatus();
  });
}

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
    }, 700);
  } catch (err) {
    setStatus(t("settings-save-error", { error: err }), "error");
  }
});

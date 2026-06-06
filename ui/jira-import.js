// Jira import modal controller. Read-only Jira integration: import a
// single issue (by key or picked from "assigned to me") into a project,
// spawning the chosen analyst agent. The backend (jira_import) does the
// worktree + analyst spawn; this module is a thin form + status surface.
//
//   openJiraImport({ projectId }) — reset + populate selectors + show.
//   doImport()                    — invoke jira_import, surface outcome.
//
// On a successful import the existing tasks/terminal flow picks up the
// spawned analyst run on refresh (onClosedRefresh), so there is no need
// to open a terminal here.

import { t, onLocaleChange } from "./i18n.js";
import { loadAgentPresence, decorateKindSelect } from "./agent-presence.js";

const { invoke } = window.__TAURI__.core;

const dialog = document.getElementById("jira-import-modal");

let onClosedRefresh = null;
// Remember the last assigned-list payload so a locale flip can re-render
// the JS-built rows + partial note (data-i18n only covers static markup).
let lastAssigned = null;

export function setJiraImportRefreshCallback(fn) {
  onClosedRefresh = fn;
}

export async function openJiraImport(prefill = {}) {
  document.getElementById("jira-import-key").value = "";
  document.getElementById("jira-assigned-list").replaceChildren();
  const note = document.getElementById("jira-assigned-note");
  note.hidden = true;
  note.textContent = "";
  lastAssigned = null;
  setStatus("");
  await populateProjects(prefill.projectId);
  await populateAnalyst();
  if (!dialog.open) dialog.showModal();
}

export function closeJiraImport() {
  if (dialog.open) dialog.close();
}

function setStatus(msg, kind) {
  const el = document.getElementById("jira-import-status");
  el.textContent = msg ?? "";
  el.className = "modal-status" + (kind ? ` ${kind}` : "");
}

async function populateProjects(preselected) {
  let cfg = null;
  try {
    cfg = await invoke("get_config");
  } catch (e) {
    console.warn("get_config in jira-import failed", e);
  }
  const select = document.getElementById("jira-import-project");
  select.replaceChildren();
  for (const p of cfg?.projects ?? []) {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = p.name;
    select.append(opt);
  }
  const want = preselected ?? cfg?.active_project_id;
  if (want) select.value = want;
}

async function populateAnalyst() {
  const map = await loadAgentPresence({ force: true });
  decorateKindSelect(document.getElementById("jira-import-analyst"), map);
}

async function loadAssigned() {
  setStatus(t("jira-import-loading"));
  try {
    const res = await invoke("jira_list_assigned"); // no args
    renderAssigned(res.issues, res.partial);
    setStatus("");
  } catch (e) {
    setStatus(typeof e === "string" ? e : t("task-error", { error: e }), "error");
  }
}

function renderAssigned(issues, partial) {
  lastAssigned = { issues, partial };
  const list = document.getElementById("jira-assigned-list");
  list.replaceChildren();
  const note = document.getElementById("jira-assigned-note");
  if (partial) {
    note.textContent = t("jira-import-partial", { count: issues.length });
    note.hidden = false;
  } else {
    note.hidden = true;
    note.textContent = "";
  }
  for (const it of issues) {
    const li = document.createElement("li");
    li.className = "jira-assigned-item";
    li.tabIndex = 0;
    const key = document.createElement("span");
    key.className = "jira-key";
    key.textContent = it.key;
    const sum = document.createElement("span");
    sum.className = "jira-summary";
    sum.textContent = it.summary;
    li.append(key, sum);
    li.setAttribute("role", "button");
    const select = () => {
      document.getElementById("jira-import-key").value = it.key;
      for (const el of list.children) el.classList.toggle("is-selected", el === li);
    };
    li.addEventListener("click", select);
    // The row is focusable (tabIndex=0); make it keyboard-activatable too.
    li.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        select();
      }
    });
    list.append(li);
  }
}

async function doImport() {
  const issueRef = document.getElementById("jira-import-key").value.trim();
  const projectId = document.getElementById("jira-import-project").value;
  const analystKind = document.getElementById("jira-import-analyst").value;
  if (!issueRef) {
    setStatus(t("jira-import-key-required"), "error");
    return;
  }
  if (!projectId) {
    setStatus(t("jira-import-project-required"), "error");
    return;
  }
  setStatus(t("jira-import-importing"));
  try {
    // jira_import wraps its fields in a single `args` object whose keys
    // are snake_case (the Rust struct is deserialized from `args`
    // directly — NOT the top-level camelCase auto-convert path).
    const res = await invoke("jira_import", {
      args: {
        issue_ref: issueRef,
        project_id: projectId,
        analyst_kind: analystKind,
      },
    });
    // Result is #[serde(tag = "outcome", rename_all = "snake_case")].
    if (res.outcome === "imported") {
      setStatus(t("jira-import-imported", { key: res.jira_key }), "ok");
      setTimeout(() => {
        closeJiraImport();
        onClosedRefresh?.();
      }, 800);
    } else {
      // "existing_active" — no session_id on this branch; just refresh so
      // the already-running work surfaces through the normal listing.
      setStatus(t("jira-import-existing", { key: res.jira_key }), "warn");
      onClosedRefresh?.();
    }
  } catch (e) {
    const msg = typeof e === "string" ? e : (e?.message ?? String(e));
    setStatus(t("jira-error", { error: msg }), "error");
  }
}

// ─────────────────────────── event wiring ───────────────────────────

for (const b of document.querySelectorAll('[data-action="close-jira-import"]')) {
  b.addEventListener("click", closeJiraImport);
}

document
  .getElementById("btn-jira-load-assigned")
  .addEventListener("click", loadAssigned);

document.getElementById("btn-jira-do-import").addEventListener("click", doImport);

// Re-render the JS-built assigned rows + partial note on a locale flip
// (data-i18n only re-stamps the static markup).
onLocaleChange(() => {
  if (dialog.open && lastAssigned) {
    renderAssigned(lastAssigned.issues, lastAssigned.partial);
  }
});

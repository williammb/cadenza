// Shared helper for "which agents are installed" detection. Used by
// the Settings and Start-Agent modals to mark Claude Code / Codex
// options as unavailable when neither the CLI binary nor the agent's
// config folder can be found on the host.
//
// The backend probe (`list_installed_agents`) returns
//   [{ kind: "claude_code" | "codex" | "antigravity" | "opencode", installed, on_path, has_config_dir }]
// — `installed` is `on_path || has_config_dir`. We never mutate values
// the user has already saved (a non-installed default still loads as
// the picked value) — we only annotate the label and disable the
// non-current options.

import { t, onLocaleChange } from "./i18n.js";

const { invoke } = window.__TAURI__.core;

// Map "claude_code" (CLI / config enum) ↔ "claude" (UI skill enum).
// Both backend Tauri commands accept their own form; we normalize here.
// For codex, antigravity, and opencode the kind and skill names coincide.
const KIND_TO_SKILL = {
  claude_code: "claude",
  codex: "codex",
  antigravity: "antigravity",
  opencode: "opencode",
};
const SKILL_TO_KIND = {
  claude: "claude_code",
  codex: "codex",
  antigravity: "antigravity",
  opencode: "opencode",
};

let cache = null;

export async function loadAgentPresence({ force = false } = {}) {
  if (cache && !force) return cache;
  try {
    const rows = await invoke("list_installed_agents");
    cache = new Map();
    for (const r of rows) cache.set(r.kind, r);
  } catch (e) {
    console.warn("list_installed_agents failed", e);
    cache = new Map();
  }
  return cache;
}

export function presenceByKind(map, kind) {
  return map.get(kind) ?? null;
}

export function presenceBySkillAgent(map, skillAgent) {
  return map.get(SKILL_TO_KIND[skillAgent] ?? skillAgent) ?? null;
}

// Annotate every <option> of an agent-kind <select> with "(not
// installed)" and disable the non-installed ones. The currently
// selected value is never disabled — that would silently invalidate
// the user's saved setting.
//
// `selectEl` is the <select>; option values must match the backend
// kind ("claude_code", "codex"). Safe to call repeatedly (idempotent).
export function decorateKindSelect(selectEl, presenceMap) {
  if (!selectEl) return;
  const selected = selectEl.value;
  const suffix = ` ${t("settings-agent-not-installed")}`;
  const tooltip = t("settings-agent-not-installed-tooltip");
  for (const opt of selectEl.options) {
    const presence = presenceMap.get(opt.value);
    // Reset state so re-applying after a locale change doesn't stack
    // suffixes ("(not installed) (not installed)"). data-i18n already
    // ran by now; we layer on top of the translated label.
    opt.disabled = false;
    opt.title = "";
    if (!presence || presence.installed) continue;
    if (!opt.textContent.endsWith(suffix)) opt.textContent += suffix;
    opt.title = tooltip;
    if (opt.value !== selected) opt.disabled = true;
  }
}

// Annotate a skill-agent checkbox + its label. When the agent isn't
// installed we disable the checkbox, uncheck it, and append the
// "(not installed)" tag to the label. `labelEl` is the <span> next to
// the checkbox.
export function decorateSkillCheckbox(checkboxEl, labelEl, presence) {
  if (!checkboxEl || !labelEl) return;
  const suffix = ` ${t("settings-agent-not-installed")}`;
  const tooltip = t("settings-agent-not-installed-tooltip");
  const installed = presence ? presence.installed : true;
  checkboxEl.disabled = !installed;
  if (!installed) {
    checkboxEl.checked = false;
    if (!labelEl.textContent.endsWith(suffix)) labelEl.textContent += suffix;
    labelEl.title = tooltip;
    checkboxEl.title = tooltip;
  } else {
    labelEl.title = "";
    checkboxEl.title = "";
  }
}

// Subscribe `cb(presenceMap)` to locale changes so callers can
// re-annotate when the user flips language. Returns an unsubscribe
// function. The presence cache itself doesn't depend on locale, so we
// reuse it.
export function onAgentPresenceRefresh(cb) {
  return onLocaleChange(async () => {
    const map = await loadAgentPresence();
    cb(map);
  });
}

export { KIND_TO_SKILL, SKILL_TO_KIND };

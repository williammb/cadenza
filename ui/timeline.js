// Run timeline modal (feature #8) — read-only view over the append-only
// audit event log. Opens from the topbar button; fetches the aggregate
// analytics and the most-recent events, renders them with createElement /
// textContent / append (NO innerHTML, no framework, no build step).

import { t } from "./i18n.js";

const { invoke } = window.__TAURI__.core;

const dialog = document.getElementById("timeline-modal");
const statsEl = document.getElementById("timeline-stats");
const emptyEl = document.getElementById("timeline-empty");
const bodyEl = document.getElementById("timeline-body");
const statusEl = document.getElementById("timeline-status");

// Cap the timeline to the most-recent N so a long-running install stays snappy.
const EVENT_LIMIT = 200;

/** Open the modal and (re)load events + analytics. */
export async function openTimeline() {
  setStatus("");
  try {
    const [events, stats] = await Promise.all([
      invoke("list_run_events", { taskId: null, limit: EVENT_LIMIT }),
      invoke("get_run_analytics"),
    ]);
    renderStats(stats);
    renderEvents(events);
  } catch (e) {
    bodyEl.replaceChildren();
    statsEl.replaceChildren();
    emptyEl.hidden = true;
    setStatus(t("timeline-load-error", { error: e }), "error");
  }
  if (!dialog.open) dialog.showModal();
}

function renderStats(stats) {
  statsEl.replaceChildren();
  if (!stats) return;
  const total = document.createElement("span");
  total.className = "timeline-stat";
  total.textContent = t("timeline-total", { n: stats.total_events ?? 0 });
  statsEl.append(total);

  // Per-agent start counts, if any.
  const byAgent = stats.by_agent || {};
  for (const [agent, n] of Object.entries(byAgent)) {
    const chip = document.createElement("span");
    chip.className = "timeline-stat";
    chip.textContent = `${agent}: ${n}`;
    statsEl.append(chip);
  }
}

function renderEvents(events) {
  bodyEl.replaceChildren();
  if (!events || events.length === 0) {
    emptyEl.hidden = false;
    return;
  }
  emptyEl.hidden = true;
  // Newest first reads best in a vertical timeline; the backend returns
  // oldest-first, so reverse a shallow copy for display.
  const rows = [...events].reverse().map(makeRow);
  bodyEl.replaceChildren(...rows);
}

function makeRow(ev) {
  const li = document.createElement("li");
  li.className = "timeline-row";

  const when = document.createElement("time");
  when.className = "timeline-when";
  when.textContent = formatTs(ev.ts_ms);
  li.append(when);

  const kind = document.createElement("strong");
  kind.className = "timeline-kind";
  kind.textContent = kindLabel(ev.kind && ev.kind.tipo);
  li.append(kind);

  const detail = document.createElement("span");
  detail.className = "timeline-detail";
  detail.textContent = describe(ev);
  li.append(detail);

  // Feature #6: a checkpoint row offers a one-click rewind of that run.
  const k = ev.kind || {};
  if (k.tipo === "checkpoint_criado" && ev.task_id && k.commit) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "btn btn-icon timeline-revert";
    btn.textContent = t("timeline-revert");
    btn.addEventListener("click", () => revertRun(ev.task_id, k.commit, btn, ev.ts_ms));
    li.append(btn);
  }

  return li;
}

async function revertRun(taskId, commit, btn, tsMs) {
  // Identify the exact checkpoint so back-to-back rows aren't ambiguous.
  const ok = window.confirm(
    t("timeline-revert-confirm", {
      task: taskId,
      when: formatTs(tsMs),
      commit: String(commit).slice(0, 8),
    }),
  );
  if (!ok) return;
  btn.disabled = true;
  try {
    const res = await invoke("revert_task_checkpoint", { taskId, commit });
    if (res.partial_leftovers && res.partial_leftovers.length > 0) {
      setStatus(
        t("timeline-revert-partial", {
          dir: res.dir,
          count: res.partial_leftovers.length,
        }),
        "error",
      );
    } else {
      setStatus(t("timeline-reverted", { dir: res.dir }), "");
    }
    // Re-render so the new checkpoint + run_revertido events show up.
    await openTimeline();
  } catch (e) {
    setStatus(t("timeline-revert-error", { error: e }), "error");
    btn.disabled = false;
  }
}

function kindLabel(tipo) {
  if (!tipo) return t("timeline-kind-desconhecido");
  const key = `timeline-kind-${tipo}`;
  const label = t(key);
  // t() returns the key itself when unresolved — fall back to the raw tag.
  return label === key ? tipo : label;
}

/** A short, human-readable detail line built from the kind payload. */
function describe(ev) {
  const k = ev.kind || {};
  const parts = [];
  if (ev.task_id) parts.push(ev.task_id);
  switch (k.tipo) {
    case "agente_iniciado":
      if (k.agente) parts.push(k.agente);
      if (k.model) parts.push(k.model);
      if (k.resumido) parts.push(t("timeline-resumed"));
      break;
    case "sessao_encerrada":
      if (k.motivo) parts.push(k.motivo);
      break;
    case "done_enviado":
      if (k.com_evidencia) parts.push(t("timeline-with-evidence"));
      if (k.resumo) parts.push(k.resumo);
      break;
    case "revisao_decidida":
      if (k.verdict) parts.push(k.verdict);
      if (k.nota) parts.push(k.nota);
      break;
    case "proposta_decidida":
      if (k.decisao) parts.push(k.decisao);
      if (k.proposta_id) parts.push(k.proposta_id);
      break;
    case "uso_observado": {
      const u = k.usage || {};
      // "total" = new tokens (input + output + cache writes); cache READS are
      // re-sent context, not new tokens, so they're excluded from the headline.
      const total =
        (u.input_tokens || 0) +
        (u.output_tokens || 0) +
        (u.cache_creation_tokens || 0);
      parts.push(t("timeline-usage-tokens", { total }));
      if (u.model) parts.push(u.model);
      break;
    }
    default:
      break;
  }
  return parts.join(" · ");
}

function formatTs(ms) {
  if (!ms) return "—";
  try {
    return new Date(Number(ms)).toLocaleString();
  } catch {
    return String(ms);
  }
}

function setStatus(msg, kind) {
  statusEl.textContent = msg ?? "";
  statusEl.className = "modal-status" + (kind ? ` ${kind}` : "");
}

function closeTimeline() {
  if (dialog.open) dialog.close();
}

// ─────────────────────────── event wiring ───────────────────────────

document
  .querySelectorAll('[data-action="close-timeline"]')
  .forEach((b) => b.addEventListener("click", closeTimeline));

// Systematic modal accessibility, applied once to every `<dialog class="modal">`
// without touching each modal's own open/close logic.
//
// Native `<dialog>.showModal()` already gives us three things for free:
//   - Esc closes the dialog (the browser fires a `cancel` then `close`).
//   - Tab is trapped inside the open dialog (the top layer is inert below it).
//   - Initial focus lands on the first focusable element (or the dialog).
//
// What it does NOT reliably do — and what this module adds — is:
//   1. Restore focus to the element that opened the dialog when it closes.
//      Native restoration breaks the moment one modal opens another (the task
//      modal closes itself, then opens start-agent), and when the trigger was
//      a card button that gets re-rendered. We capture the trigger ourselves
//      right before the dialog opens and restore it on close.
//   2. Expose the dialog's accessible name to assistive tech. We wire
//      `aria-labelledby` to the dialog's heading and mark it `aria-modal`.
//   3. Keep focus inside when content is revealed/hidden after open (the task
//      modal toggles whole tab panels via `.hidden`). A document-level focusin
//      guard pulls focus back in if it ever escapes the open dialog.
//
// Everything here is framework-free and runs off DOM events + a tiny
// MutationObserver on the `open` attribute, so it works for every modal —
// including ones opened from dynamically-created triggers — without each
// modal module having to opt in.

const FOCUSABLE = [
  "a[href]",
  "area[href]",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  "button:not([disabled])",
  "iframe",
  "object",
  "embed",
  '[tabindex]:not([tabindex="-1"])',
  "[contenteditable]",
  "audio[controls]",
  "video[controls]",
  "details > summary:first-of-type",
].join(",");

// Per-dialog trigger element to restore focus to on close. Keyed by the
// dialog node so chained/stacked modals each remember their own opener.
const triggers = new WeakMap();
// Dialogs we've already wired, so a re-scan never double-binds listeners.
const wired = new WeakSet();

// The element the user last interacted with (clicked / keyed). showModal()
// moves focus INTO the dialog synchronously, and our MutationObserver fires a
// microtask later — too late to read the opener from document.activeElement.
// So we track it here on the way in (capture phase) and read it on open.
let lastInteractive = null;
function rememberInteractive(e) {
  // The focusable ancestor of the event target is the real opener (e.g. an
  // <svg> inside a button → the button).
  const el = e.target instanceof Element ? e.target.closest(FOCUSABLE) : null;
  if (el) lastInteractive = el;
}

// Visible + actually-focusable descendants of `root`, in DOM order. Filters
// out elements hidden via `hidden`, `display:none`, `visibility:hidden`, or a
// zero client rect (covers `.hidden` panels and collapsed sections).
function focusableWithin(root) {
  return [...root.querySelectorAll(FOCUSABLE)].filter((el) => {
    if (el.hidden || el.closest("[hidden]")) return false;
    if (el.getAttribute("aria-hidden") === "true") return false;
    const style = window.getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden") return false;
    // offsetParent is null for display:none; rects catch position:fixed too.
    return el.offsetParent !== null || el.getClientRects().length > 0;
  });
}

// Give the dialog an accessible name + modal semantics. `<dialog>` already
// has an implicit dialog role and showModal() sets modal semantics in most
// engines, but being explicit makes the name reliable across screen readers.
function labelDialog(dialog) {
  if (!dialog.hasAttribute("aria-modal")) {
    dialog.setAttribute("aria-modal", "true");
  }
  if (
    dialog.hasAttribute("aria-label") ||
    dialog.hasAttribute("aria-labelledby")
  ) {
    return;
  }
  // Prefer an existing heading; fall back to the first heading of any level.
  const heading = dialog.querySelector("h1, h2, h3, [role='heading']");
  if (!heading) return;
  if (!heading.id) {
    heading.id = `${dialog.id || "modal"}-a11y-title`;
  }
  dialog.setAttribute("aria-labelledby", heading.id);
}

// When the dialog opens: remember the trigger and make sure focus is inside.
function onOpen(dialog) {
  // The opener is the last element the user clicked/keyed before showModal()
  // (captured by rememberInteractive). Fall back to the previously-focused
  // element if no interaction was recorded (e.g. modal opened programmatically
  // from a backend event). Skip openers inside this same dialog.
  const opener = lastInteractive || document.activeElement;
  if (
    opener &&
    opener !== document.body &&
    opener !== dialog &&
    !dialog.contains(opener) &&
    opener.isConnected
  ) {
    triggers.set(dialog, opener);
  }
  lastInteractive = null;
  // If the dialog's own initial-focus logic (each modal focuses a field) left
  // focus outside — or content shifted — pull the first focusable in. Deferred
  // a frame so it runs after the modal module's own `.focus()` call.
  requestAnimationFrame(() => {
    if (!dialog.open) return;
    if (dialog.contains(document.activeElement)) return;
    const first = focusableWithin(dialog)[0];
    (first || dialog).focus();
  });
}

// When the dialog closes: restore focus to the opener if it's still around and
// focusable. Guard against the opener having been removed (re-rendered card).
function onClose(dialog) {
  const trigger = triggers.get(dialog);
  triggers.delete(dialog);
  if (!trigger || !trigger.isConnected) return;
  // Another modal may have opened in the same tick (modal chaining). Don't
  // steal focus away from a now-open dialog.
  const openDialog = document.querySelector("dialog[open]");
  if (openDialog && openDialog !== dialog) return;
  try {
    trigger.focus();
  } catch {
    /* opener no longer focusable — leave focus where the browser put it */
  }
}

// Defensive focus guard. Modern engines already trap Tab inside an open modal
// `<dialog>` via the top layer, so we deliberately do NOT re-implement Tab
// wrapping (doing both would double-wrap). We only catch the failure case the
// native trap can miss: focus landing OUTSIDE the open dialog (e.g. a control
// that was hidden after open, or programmatic focus from late-loading content).
// A single document-level `focusin` listener pulls focus back to the dialog's
// first focusable element whenever it escapes the currently-open modal.
function onDocumentFocusIn(e) {
  const dialog = document.querySelector("dialog.modal[open]");
  if (!dialog) return;
  if (dialog.contains(e.target)) return; // focus is where it should be
  // Ignore focus moving to <body> (transient) — only redirect real targets.
  if (e.target === document.body || e.target === document.documentElement) {
    return;
  }
  const first = focusableWithin(dialog)[0];
  (first || dialog).focus();
}

function wire(dialog) {
  if (wired.has(dialog)) return;
  wired.add(dialog);
  labelDialog(dialog);
  // Native `close` fires for Esc, form-method=dialog submit, and .close().
  dialog.addEventListener("close", () => onClose(dialog));

  // Track the `open` attribute so we run onOpen regardless of which code path
  // (showModal from any modal module) opened it.
  const obs = new MutationObserver((records) => {
    for (const r of records) {
      if (r.attributeName === "open" && dialog.open) {
        onOpen(dialog);
      }
    }
  });
  obs.observe(dialog, { attributes: true, attributeFilter: ["open"] });

  // Already open at wire time (shouldn't happen at boot, but be safe).
  if (dialog.open) onOpen(dialog);
}

let installed = false;

export function initModalA11y() {
  const dialogs = document.querySelectorAll("dialog.modal");
  for (const d of dialogs) wire(d);
  if (!installed) {
    installed = true;
    // One document-level guard covers whichever modal is currently open.
    document.addEventListener("focusin", onDocumentFocusIn);
    // Track the opener synchronously, before showModal() steals focus. Capture
    // phase + pointerdown/keydown so we see it no matter how it was triggered.
    document.addEventListener("pointerdown", rememberInteractive, true);
    document.addEventListener("keydown", rememberInteractive, true);
  }
}

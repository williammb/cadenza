// Only file allowed to write innerHTML — output always goes through DOMPurify.
// DOMPurify is a UMD global loaded via <script src="vendor/purify.min.js"> in
// index.html before any ES module runs.
import { marked } from "./vendor/marked.min.js";

const purify = globalThis.DOMPurify;

/**
 * Render `md` as sanitized HTML into `el`.
 * Falls back to plain text if DOMPurify is not available — never leaves
 * the user with a blank pane, since proposals must be readable even when
 * the sanitizer fails to load.
 */
export function renderMarkdown(el, md) {
  const src = md ?? "";
  if (!purify) {
    el.textContent = src;
    return;
  }
  el.innerHTML = purify.sanitize(marked.parse(src));
}

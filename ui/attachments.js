// Image attachments for task / ideia bodies.
//
// Three input paths, all resolved in the webview by reading the image
// *bytes* in JS and handing them to the Rust `save_attachment` command:
//   1. paste (Ctrl+V) into the textarea
//   2. HTML5 drag-and-drop onto the textarea
//   3. an "Attach image" <input type=file accept="image/*" multiple>
//
// The body stores a clean relative path `![](attachments/<kind>/<id>/<hash>.<ext>)`.
// An Edit / Preview toggle renders the markdown; before rendering, every
// `attachments/...` reference is swapped for a `data:` URL fetched from
// Rust so the image shows without an asset-protocol round-trip.
//
// New (unsaved) owners have no id yet, so their images are buffered in
// memory under a `cadenza-pending:N` token and flushed to disk right
// after the create call mints the id.

import { t } from "./i18n.js";
import { renderMarkdown } from "./markdown.js";

const { invoke } = window.__TAURI__.core;

const MAX_BYTES = 5 * 1024 * 1024;
// `![alt](src)` — capture the src (no spaces, no closing paren).
const IMAGE_REF_RE = /!\[[^\]]*\]\(([^)\s]+)\)/g;

// Base64-encode a Uint8Array in chunks so large images don't blow the
// argument limit of String.fromCharCode(...spread).
function bytesToBase64(bytes) {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

// Client-side mirror of the backend magic-byte allowlist (PNG, JPEG,
// GIF, WebP). Lets us reject unsupported formats immediately — the
// backend re-validates and stays the source of truth.
function isSupportedImage(u8) {
  if (u8.length >= 4 && u8[0] === 0x89 && u8[1] === 0x50 && u8[2] === 0x4e && u8[3] === 0x47)
    return true; // PNG
  if (u8.length >= 3 && u8[0] === 0xff && u8[1] === 0xd8 && u8[2] === 0xff) return true; // JPEG
  if (u8.length >= 4 && u8[0] === 0x47 && u8[1] === 0x49 && u8[2] === 0x46 && u8[3] === 0x38)
    return true; // GIF ("GIF8")
  if (
    u8.length >= 12 &&
    u8[0] === 0x52 && u8[1] === 0x49 && u8[2] === 0x46 && u8[3] === 0x46 && // "RIFF"
    u8[8] === 0x57 && u8[9] === 0x45 && u8[10] === 0x42 && u8[11] === 0x50 // "WEBP"
  )
    return true;
  return false;
}

/**
 * Render `md` into `el`, swapping every attachment reference for a
 * `data:` URL first. `pendingMap` resolves in-memory tokens for not-yet-
 * saved owners. External (`http(s)://`) and orphaned refs are left as-is
 * so the render never breaks — an orphan just shows its alt text.
 */
export async function renderMarkdownWithImages(el, md, pendingMap = new Map()) {
  const src = md ?? "";
  const refs = new Set();
  let m;
  IMAGE_REF_RE.lastIndex = 0;
  while ((m = IMAGE_REF_RE.exec(src))) refs.add(m[1]);

  let out = src;
  for (const ref of refs) {
    let dataUrl = null;
    if (pendingMap.has(ref)) {
      dataUrl = pendingMap.get(ref);
    } else if (ref.startsWith("attachments/")) {
      try {
        const a = await invoke("read_attachment", { relPath: ref });
        dataUrl = `data:${a.mime};base64,${a.base64}`;
      } catch {
        continue; // orphaned reference — leave it, alt text renders
      }
    } else {
      continue; // external URL or unrelated link
    }
    out = out.split(`(${ref})`).join(`(${dataUrl})`);
  }
  renderMarkdown(el, out);
}

/**
 * Wire up attachment input + the Edit/Preview toggle for one modal.
 *
 * @param {object} cfg
 * @param {HTMLTextAreaElement} cfg.textarea  body editor
 * @param {HTMLElement}        cfg.preview    rendered-markdown container
 * @param {HTMLElement}        cfg.editBtn    "Edit" toggle button
 * @param {HTMLElement}        cfg.previewBtn "Preview" toggle button
 * @param {HTMLInputElement}   cfg.fileInput  hidden <input type=file>
 * @param {HTMLElement}        cfg.attachBtn  visible "Attach image" button
 * @param {string}             cfg.kind       "tasks" | "ideias"
 * @param {() => (string|null)} cfg.getOwnerId  owner id, or null if unsaved
 * @param {(msg: string) => void} cfg.onError  inline error sink
 * @returns {{ reset(readOnly?: boolean): void, flush(ownerId: string): Promise<string>, showEdit(): void }}
 */
export function setupAttachments(cfg) {
  const { textarea, preview, editBtn, previewBtn, fileInput, attachBtn, kind, getOwnerId, onError } =
    cfg;

  // Buffered images for a not-yet-created owner: { token, bytes, dataUrl }.
  let pending = [];
  let tokenSeq = 0;

  function insertRef(ref) {
    const md = `![](${ref})`;
    const start = textarea.selectionStart ?? textarea.value.length;
    const end = textarea.selectionEnd ?? textarea.value.length;
    textarea.value = textarea.value.slice(0, start) + md + textarea.value.slice(end);
    const pos = start + md.length;
    textarea.selectionStart = textarea.selectionEnd = pos;
    textarea.focus();
  }

  async function handleFiles(files) {
    for (const file of files) {
      if (!file) continue;
      const type = file.type || "";
      if (type && !type.startsWith("image/")) continue; // ignore non-images
      if (file.size > MAX_BYTES) {
        onError?.(t("attachment-error-too-large"));
        continue;
      }
      const u8 = new Uint8Array(await file.arrayBuffer());
      if (!isSupportedImage(u8)) {
        onError?.(t("attachment-error-unsupported-format"));
        continue;
      }
      const bytes = Array.from(u8);
      const ownerId = getOwnerId();
      if (ownerId) {
        try {
          const rel = await invoke("save_attachment", { kind, ownerId, bytes });
          insertRef(rel);
        } catch (e) {
          // Backend returns a stable i18n key as the error string.
          onError?.(t(String(e)));
        }
      } else {
        const token = `cadenza-pending:${tokenSeq++}`;
        const mime = type || "image/png";
        pending.push({ token, bytes, dataUrl: `data:${mime};base64,${bytesToBase64(u8)}` });
        insertRef(token);
      }
    }
  }

  function showEdit() {
    preview.hidden = true;
    textarea.hidden = false;
    editBtn?.classList.add("is-active");
    previewBtn?.classList.remove("is-active");
  }

  async function showPreview() {
    const pendingMap = new Map(pending.map((p) => [p.token, p.dataUrl]));
    await renderMarkdownWithImages(preview, textarea.value, pendingMap);
    textarea.hidden = true;
    preview.hidden = false;
    previewBtn?.classList.add("is-active");
    editBtn?.classList.remove("is-active");
  }

  /** Clear buffered images and return to Edit view. Pass `readOnly` to
   *  hide the attach affordances (e.g. the read-only ideia edit modal). */
  function reset(readOnly = false) {
    pending = [];
    tokenSeq = 0;
    showEdit();
    if (attachBtn) attachBtn.hidden = !!readOnly;
  }

  /** Persist any buffered images for the now-known `ownerId`, rewrite the
   *  body tokens to their saved paths, and return the final body text. */
  async function flush(ownerId) {
    let body = textarea.value;
    for (const p of pending) {
      try {
        const rel = await invoke("save_attachment", { kind, ownerId, bytes: p.bytes });
        body = body.split(`(${p.token})`).join(`(${rel})`);
      } catch (e) {
        onError?.(t(String(e)));
        // Drop the broken reference rather than leave a dangling token.
        body = body.split(`![](${p.token})`).join("");
      }
    }
    pending = [];
    textarea.value = body;
    return body;
  }

  // ── listeners (attached once; modals call reset() on open) ──
  editBtn?.addEventListener("click", showEdit);
  previewBtn?.addEventListener("click", () => {
    showPreview().catch((e) => console.warn("preview render failed", e));
  });

  attachBtn?.addEventListener("click", () => fileInput?.click());
  fileInput?.addEventListener("change", async () => {
    await handleFiles(Array.from(fileInput.files || []));
    fileInput.value = ""; // allow re-selecting the same file
  });

  textarea.addEventListener("paste", (e) => {
    const items = e.clipboardData?.items;
    if (!items) return;
    const files = [];
    for (const it of items) {
      if (it.kind === "file") {
        const f = it.getAsFile();
        if (f) files.push(f);
      }
    }
    if (files.length) {
      e.preventDefault();
      handleFiles(files);
    }
  });

  // HTML5 drag-and-drop (the Tauri window keeps `dragDropEnabled: false`,
  // so the webview receives normal DOM drop events).
  textarea.addEventListener("dragover", (e) => {
    if (Array.from(e.dataTransfer?.types || []).includes("Files")) {
      e.preventDefault();
      textarea.classList.add("is-dragover");
    }
  });
  textarea.addEventListener("dragleave", () => textarea.classList.remove("is-dragover"));
  textarea.addEventListener("drop", (e) => {
    const files = Array.from(e.dataTransfer?.files || []).filter((f) => f.type.startsWith("image/"));
    textarea.classList.remove("is-dragover");
    if (files.length) {
      e.preventDefault();
      handleFiles(files);
    }
  });

  return { reset, flush, showEdit };
}

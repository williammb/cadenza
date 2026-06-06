//! ADF (Atlassian Document Format) → Markdown converter.
//!
//! PURE and deterministic. MUST NEVER panic on malformed input: every
//! field access goes through `.get(...).and_then(...)` with a default —
//! no indexing, no `unwrap`. Unknown node types render as
//! `[unsupported: <type>]` (NEVER raw JSON), so a future ADF node can
//! never leak issue data verbatim into the markdown.

use serde_json::Value;

/// Convert an ADF document `Value` to Markdown. `Null`/non-object/`None`
/// input → `""`. Never panics.
pub fn adf_to_markdown(doc: &Value) -> String {
    if !doc.is_object() {
        return String::new();
    }
    let mut out = String::new();
    render_node(doc, &mut out, 0);
    // Normalize trailing whitespace/newlines for stable output.
    out.trim_end().to_string()
}

/// Render a single node by its `type`. Block nodes append their own
/// trailing newlines; inline nodes (`text`, `hardBreak`) do not.
fn render_node(node: &Value, out: &mut String, depth: usize) {
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
    match node_type {
        "doc" => {
            render_block_children(node, out, depth);
        }
        "paragraph" => {
            render_children(node, out, depth);
            out.push('\n');
        }
        "heading" => {
            let level = node
                .get("attrs")
                .and_then(|a| a.get("level"))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 6) as usize;
            for _ in 0..level {
                out.push('#');
            }
            out.push(' ');
            render_children(node, out, depth);
            out.push('\n');
        }
        "text" => {
            render_text(node, out);
        }
        "hardBreak" => {
            out.push('\n');
        }
        "rule" => {
            out.push_str("---\n");
        }
        "bulletList" => {
            render_list(node, out, depth, None);
        }
        "orderedList" => {
            // ADF `attrs.order` sets the starting number (default 1).
            let start = node
                .get("attrs")
                .and_then(|a| a.get("order"))
                .and_then(Value::as_u64)
                .unwrap_or(1);
            render_list(node, out, depth, Some(start));
        }
        "listItem" => {
            // Normally reached via render_list, which handles the marker.
            // If reached directly, render its children as a block.
            render_block_children(node, out, depth);
        }
        "codeBlock" => {
            let lang = node
                .get("attrs")
                .and_then(|a| a.get("language"))
                .and_then(Value::as_str)
                .unwrap_or("");
            out.push_str("```");
            out.push_str(lang);
            out.push('\n');
            // codeBlock children are plain text nodes; emit raw (no marks).
            if let Some(children) = node.get("content").and_then(Value::as_array) {
                for child in children {
                    if let Some(t) = child.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                    }
                }
            }
            out.push('\n');
            out.push_str("```\n");
        }
        "blockquote" => {
            let mut inner = String::new();
            render_block_children(node, &mut inner, depth);
            for line in inner.trim_end().lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
        }
        "" => {
            // A node with no type at all — treat as unsupported but with a
            // stable placeholder; never dump the raw JSON.
            out.push_str("[unsupported: ]\n");
        }
        other => {
            out.push('[');
            out.push_str("unsupported: ");
            out.push_str(other);
            out.push_str("]\n");
        }
    }
}

/// Render the `content` array as block-level children (each child decides
/// its own trailing newlines).
fn render_block_children(node: &Value, out: &mut String, depth: usize) {
    if let Some(children) = node.get("content").and_then(Value::as_array) {
        for child in children {
            render_node(child, out, depth);
        }
    }
}

/// Render the `content` array as inline children (used by paragraph /
/// heading — no per-child block newlines).
fn render_children(node: &Value, out: &mut String, depth: usize) {
    if let Some(children) = node.get("content").and_then(Value::as_array) {
        for child in children {
            render_node(child, out, depth);
        }
    }
}

/// Render a bullet/ordered list. `start` is `Some(n)` for ordered lists.
fn render_list(node: &Value, out: &mut String, depth: usize, start: Option<u64>) {
    let indent = "  ".repeat(depth);
    // For ordered lists `index` is the next number to emit (starts at the
    // `attrs.order` value, default 1); for bullet lists it is unused.
    let mut index = start.unwrap_or(1);
    if let Some(items) = node.get("content").and_then(Value::as_array) {
        for item in items {
            let is_list_item = item.get("type").and_then(Value::as_str) == Some("listItem");
            if !is_list_item {
                // Unexpected child inside a list — render it as a node so
                // unknown types still surface as [unsupported: ...].
                render_node(item, out, depth);
                continue;
            }
            // Render the item's children into a buffer so we can prefix the
            // first line with the marker and indent continuation lines.
            let mut item_buf = String::new();
            if let Some(children) = item.get("content").and_then(Value::as_array) {
                for child in children {
                    render_node(child, &mut item_buf, depth + 1);
                }
            }
            let marker = match start {
                Some(_) => {
                    let m = format!("{indent}{index}. ");
                    index += 1;
                    m
                }
                None => format!("{indent}- "),
            };
            emit_list_item(out, &marker, &indent, &item_buf);
        }
    }
}

/// Emit a list item: first line gets the marker, nested-list continuation
/// lines keep their own indent, plain continuation lines get aligned.
fn emit_list_item(out: &mut String, marker: &str, indent: &str, item_buf: &str) {
    let trimmed = item_buf.trim_end();
    if trimmed.is_empty() {
        out.push_str(marker);
        out.push('\n');
        return;
    }
    let cont_indent = " ".repeat(marker.len());
    for (i, line) in trimmed.lines().enumerate() {
        if i == 0 {
            out.push_str(marker);
            out.push_str(line);
        } else if line.starts_with(indent) && (line.contains("- ") || line_is_ordered(line)) {
            // Nested list line — preserve its own indentation as-is.
            out.push_str(line);
        } else {
            out.push_str(&cont_indent);
            out.push_str(line.trim_start());
        }
        out.push('\n');
    }
}

/// Heuristic: does a (trimmed-of-indent) line look like an ordered-list
/// marker, e.g. "1. text"? Used only to preserve nested list indentation.
fn line_is_ordered(line: &str) -> bool {
    let t = line.trim_start();
    let mut chars = t.chars();
    let mut saw_digit = false;
    for c in chars.by_ref() {
        if c.is_ascii_digit() {
            saw_digit = true;
        } else {
            return saw_digit && c == '.';
        }
    }
    false
}

/// Render a `text` node, applying its `marks`.
fn render_text(node: &Value, out: &mut String) {
    let text = node.get("text").and_then(Value::as_str).unwrap_or("");
    let marks = node.get("marks");
    let rendered = match marks {
        Some(m) => apply_marks(text, m),
        None => text.to_string(),
    };
    out.push_str(&rendered);
}

/// Apply ADF marks to `text`. Supported: strong, em, code, link. Unknown
/// marks are ignored (the text still renders). Never panics.
fn apply_marks(text: &str, marks: &Value) -> String {
    let Some(arr) = marks.as_array() else {
        return text.to_string();
    };
    let mut result = text.to_string();
    // `code` should wrap innermost; apply in a deterministic order:
    // code, then em, then strong, then link (outermost).
    let has = |name: &str| -> bool {
        arr.iter()
            .any(|m| m.get("type").and_then(Value::as_str) == Some(name))
    };
    if has("code") {
        result = format!("`{result}`");
    }
    if has("em") {
        result = format!("*{result}*");
    }
    if has("strong") {
        result = format!("**{result}**");
    }
    // Link mark carries attrs.href.
    if let Some(href) = arr.iter().find_map(|m| {
        if m.get("type").and_then(Value::as_str) == Some("link") {
            m.get("attrs")
                .and_then(|a| a.get("href"))
                .and_then(Value::as_str)
        } else {
            None
        }
    }) {
        result = format!("[{result}]({href})");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adf_null_description_is_empty() {
        assert_eq!(adf_to_markdown(&Value::Null), "");
    }

    #[test]
    fn adf_non_object_is_empty() {
        assert_eq!(adf_to_markdown(&json!("just a string")), "");
        assert_eq!(adf_to_markdown(&json!(42)), "");
        assert_eq!(adf_to_markdown(&json!([1, 2, 3])), "");
    }

    #[test]
    fn adf_paragraph_and_text() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "Hello world" }
                ]}
            ]
        });
        assert_eq!(adf_to_markdown(&doc), "Hello world");
    }

    #[test]
    fn adf_text_marks_strong_em_code_link() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "bold", "marks": [{"type": "strong"}] },
                    { "type": "text", "text": " " },
                    { "type": "text", "text": "italic", "marks": [{"type": "em"}] },
                    { "type": "text", "text": " " },
                    { "type": "text", "text": "code", "marks": [{"type": "code"}] },
                    { "type": "text", "text": " " },
                    { "type": "text", "text": "link", "marks": [
                        {"type": "link", "attrs": {"href": "https://x.test"}}
                    ]}
                ]}
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("**bold**"), "got: {md}");
        assert!(md.contains("*italic*"), "got: {md}");
        assert!(md.contains("`code`"), "got: {md}");
        assert!(md.contains("[link](https://x.test)"), "got: {md}");
    }

    #[test]
    fn adf_heading_levels() {
        for level in 1..=6u64 {
            let doc = json!({
                "type": "doc",
                "content": [
                    { "type": "heading", "attrs": {"level": level}, "content": [
                        { "type": "text", "text": "Title" }
                    ]}
                ]
            });
            let md = adf_to_markdown(&doc);
            let expected = format!("{} Title", "#".repeat(level as usize));
            assert_eq!(md, expected, "level {level}");
        }
    }

    #[test]
    fn adf_bullet_list() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "bulletList", "content": [
                    { "type": "listItem", "content": [
                        { "type": "paragraph", "content": [{"type": "text", "text": "one"}] }
                    ]},
                    { "type": "listItem", "content": [
                        { "type": "paragraph", "content": [{"type": "text", "text": "two"}] }
                    ]}
                ]}
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("- one"), "got: {md}");
        assert!(md.contains("- two"), "got: {md}");
    }

    #[test]
    fn adf_ordered_list() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "orderedList", "content": [
                    { "type": "listItem", "content": [
                        { "type": "paragraph", "content": [{"type": "text", "text": "first"}] }
                    ]},
                    { "type": "listItem", "content": [
                        { "type": "paragraph", "content": [{"type": "text", "text": "second"}] }
                    ]}
                ]}
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("1. first"), "got: {md}");
        assert!(md.contains("2. second"), "got: {md}");
    }

    #[test]
    fn adf_nested_list() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "bulletList", "content": [
                    { "type": "listItem", "content": [
                        { "type": "paragraph", "content": [{"type": "text", "text": "outer"}] },
                        { "type": "bulletList", "content": [
                            { "type": "listItem", "content": [
                                { "type": "paragraph", "content": [{"type": "text", "text": "inner"}] }
                            ]}
                        ]}
                    ]}
                ]}
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("- outer"), "got: {md}");
        assert!(md.contains("inner"), "got: {md}");
        // Inner item should be more deeply indented than the outer marker.
        assert!(
            md.contains("  - inner"),
            "expected nested indent, got: {md}"
        );
    }

    #[test]
    fn adf_code_block_with_language() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "codeBlock", "attrs": {"language": "rust"}, "content": [
                    { "type": "text", "text": "fn main() {}" }
                ]}
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("```rust"), "got: {md}");
        assert!(md.contains("fn main() {}"), "got: {md}");
        assert!(md.trim_end().ends_with("```"), "got: {md}");
    }

    #[test]
    fn adf_blockquote() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "blockquote", "content": [
                    { "type": "paragraph", "content": [{"type": "text", "text": "quoted"}] }
                ]}
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("> quoted"), "got: {md}");
    }

    #[test]
    fn adf_hard_break_and_rule() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "a" },
                    { "type": "hardBreak" },
                    { "type": "text", "text": "b" }
                ]},
                { "type": "rule" }
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("a\nb"), "got: {md}");
        assert!(md.contains("---"), "got: {md}");
    }

    #[test]
    fn adf_unknown_node_emits_unsupported_block() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "mediaSingle", "content": [] }
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("[unsupported: mediaSingle]"), "got: {md}");
    }

    #[test]
    fn adf_unknown_node_never_dumps_raw_json() {
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "weirdNode", "attrs": {"secret": "do-not-leak"}, "content": [
                    { "type": "text", "text": "leak-me" }
                ]}
            ]
        });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("[unsupported: weirdNode]"), "got: {md}");
        // The unsupported placeholder must NOT render the node's inner data.
        assert!(!md.contains("do-not-leak"), "leaked attrs: {md}");
        assert!(!md.contains("leak-me"), "leaked children: {md}");
        assert!(!md.contains('{'), "dumped raw json: {md}");
    }

    #[test]
    fn adf_malformed_missing_fields_no_panic() {
        // Nodes missing type / content / attrs / text — must not panic.
        let docs = vec![
            json!({ "type": "doc" }),
            json!({ "type": "doc", "content": "not-an-array" }),
            json!({ "type": "doc", "content": [ {} ] }),
            json!({ "type": "doc", "content": [ {"type": "paragraph"} ] }),
            json!({ "type": "doc", "content": [ {"type": "text"} ] }),
            json!({ "type": "doc", "content": [ {"type": "heading"} ] }),
            json!({ "type": "doc", "content": [ {"type": "text", "marks": "bad"} ] }),
            json!({ "type": "doc", "content": [ {"type": "orderedList", "attrs": {"order": "x"}} ] }),
            json!({}),
        ];
        for d in &docs {
            // Just must not panic; result content is irrelevant here.
            let _ = adf_to_markdown(d);
        }
    }

    #[test]
    fn adf_deeply_nested_no_panic() {
        // Build a deeply nested bulletList to exercise recursion safety.
        let mut node = json!({
            "type": "listItem",
            "content": [
                { "type": "paragraph", "content": [{"type": "text", "text": "leaf"}] }
            ]
        });
        for _ in 0..200 {
            node = json!({
                "type": "bulletList",
                "content": [
                    { "type": "listItem", "content": [ node ] }
                ]
            });
        }
        let doc = json!({ "type": "doc", "content": [ node ] });
        let md = adf_to_markdown(&doc);
        assert!(md.contains("leaf"), "expected leaf in output");
    }
}

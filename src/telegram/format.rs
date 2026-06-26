//! Make assistant markdown readable in Telegram.
//!
//! Telegram replies are sent WITHOUT a `parse_mode` (the safest option — one
//! malformed entity makes the Bot API reject the whole message). That means
//! Telegram renders no markdown at all: `**bold**` shows the asterisks,
//! `# Heading` shows the hashes, and — worst — a markdown TABLE shows up as a
//! wall of `|` and `-` that's almost unreadable on a phone.
//!
//! `markdown_to_telegram` flattens that into clean plain text:
//!   * tables become `• Header: value · Header: value` bullet lists
//!   * headings / bold / italic / inline-code / blockquote markers are dropped
//!   * `[text](url)` becomes `text (url)` so the source stays visible+tappable
//!   * paragraph and list structure is preserved
//!
//! It's best-effort and panic-free, so it can also run on the partial text we
//! edit into the live "typing" bubble mid-stream.

/// Convert assistant markdown to Telegram-friendly plain text.
pub fn markdown_to_telegram(src: &str) -> String {
    // Resolve images (drop) and links (`[text](url)` → `text (url)`) once over
    // the whole text so the per-line pass below only has to strip emphasis.
    let no_images = regex::Regex::new(r"!\[[^\]]*\]\([^)]*\)")
        .unwrap()
        .replace_all(src, "");
    let linked = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)")
        .unwrap()
        .replace_all(&no_images, "$1 ($2)");

    let lines: Vec<&str> = linked.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut in_fence = false;

    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();

        // Fenced code block: drop the ``` marker, keep the contents verbatim.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            out.push(raw.to_string());
            i += 1;
            continue;
        }

        // Markdown table: a `|`-delimited header row immediately followed by a
        // `---` separator row. Flatten each data row to "Header: value · …".
        if is_table_row(trimmed)
            && i + 1 < lines.len()
            && is_separator_row(lines[i + 1].trim())
        {
            let headers = split_row(trimmed);
            i += 2; // skip header + separator
            while i < lines.len() && is_table_row(lines[i].trim()) {
                let cells = split_row(lines[i].trim());
                let mut parts: Vec<String> = Vec::new();
                for (j, cell) in cells.iter().enumerate() {
                    let val = strip_emphasis(cell);
                    if val.is_empty() {
                        continue;
                    }
                    match headers.get(j).map(|h| strip_emphasis(h)) {
                        Some(h) if !h.is_empty() => parts.push(format!("{h}: {val}")),
                        _ => parts.push(val),
                    }
                }
                if !parts.is_empty() {
                    out.push(format!("• {}", parts.join(" · ")));
                }
                i += 1;
            }
            continue;
        }

        out.push(strip_block_line(raw));
        i += 1;
    }

    // Collapse 3+ blank lines (left by stripped separators/rules) to one.
    let joined = out.join("\n");
    regex::Regex::new(r"\n{3,}")
        .unwrap()
        .replace_all(&joined, "\n\n")
        .trim()
        .to_string()
}

/// A pipe-delimited row (we require a leading `|`, which models reliably emit,
/// to avoid mistaking prose containing a single `|` for a table).
fn is_table_row(s: &str) -> bool {
    s.starts_with('|') && s.matches('|').count() >= 2
}

/// The `|---|:--:|` divider under a table header.
fn is_separator_row(s: &str) -> bool {
    is_table_row(s) && s.contains('-') && s.chars().all(|c| matches!(c, '-' | ':' | '|' | ' '))
}

/// Split a `| a | b | c |` row into trimmed cell strings.
fn split_row(s: &str) -> Vec<String> {
    s.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

/// Drop inline emphasis / code / strikethrough markers, keep the words.
fn strip_emphasis(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Strip block-level markers (heading `#`, blockquote `>`, bullet `-/*/+` →
/// `•`, horizontal rule → blank) while preserving leading indentation, then
/// strip inline emphasis from what's left.
fn strip_block_line(raw: &str) -> String {
    let lead_len = raw.len() - raw.trim_start().len();
    let lead = &raw[..lead_len]; // ASCII whitespace → byte-safe slice
    let mut body = raw.trim_start();

    if body.starts_with('#') {
        body = body.trim_start_matches('#').trim_start();
    }
    while let Some(rest) = body.strip_prefix('>') {
        body = rest.trim_start();
    }
    // Horizontal rule (`---`, `***`, `___`) → blank line.
    if body.len() >= 3 && body.chars().all(|c| c == '-' || c == '*' || c == '_') {
        return String::new();
    }
    let mut bullet = "";
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = body.strip_prefix(marker) {
            bullet = "• ";
            body = rest;
            break;
        }
    }
    format!("{lead}{bullet}{}", strip_emphasis(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_becomes_bullet_list() {
        let md = "\
| Rank | Team | Pts |
|------|------|-----|
| 1 | Colombia | 9 |
| 2 | Mexico | 4 |";
        let out = markdown_to_telegram(md);
        assert_eq!(
            out,
            "• Rank: 1 · Team: Colombia · Pts: 9\n• Rank: 2 · Team: Mexico · Pts: 4"
        );
        assert!(!out.contains('|'));
        assert!(!out.contains("---"));
    }

    #[test]
    fn strips_headings_and_emphasis() {
        let md = "## Standing\nColombia is **first** with _9_ points.";
        let out = markdown_to_telegram(md);
        assert_eq!(out, "Standing\nColombia is first with 9 points.");
    }

    #[test]
    fn links_keep_url_visible() {
        let md = "See [BBC Sport](https://bbc.com/x) for details.";
        assert_eq!(
            markdown_to_telegram(md),
            "See BBC Sport (https://bbc.com/x) for details."
        );
    }

    #[test]
    fn bullets_normalized() {
        let md = "- one\n- two\n  - nested";
        assert_eq!(markdown_to_telegram(md), "• one\n• two\n  • nested");
    }

    #[test]
    fn plain_prose_unchanged() {
        let md = "How did Germany play today? They won 2-1.";
        assert_eq!(markdown_to_telegram(md), md);
    }

    #[test]
    fn partial_table_midstream_does_not_panic() {
        // Mid-stream we may see a header + separator but no data rows yet, or
        // even a half-written separator. Must never panic.
        let _ = markdown_to_telegram("| A | B |\n|---");
        let _ = markdown_to_telegram("| A | B |\n|---|---|\n| 1 |");
    }
}

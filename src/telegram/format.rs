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

/// Is this character part of a word, for the purposes of the underscore
/// rule below? Unicode-aware: `Straße_Nr` is as much one word as `a_b`.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Punctuation that can follow a link in prose without belonging to it.
fn is_sentence_tail(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'' | '»' | '”' | '’'
    )
}

/// If a URL starts at `i`, how many characters of it belong to the address.
///
/// The address runs to the next whitespace, minus any emphasis markers that
/// merely wrap it: a model writing `**https://…**` is bolding the link, not
/// extending it. Those markers are often not the last thing on the line —
/// `**https://…pdf**.` ends a sentence — so we step over trailing sentence
/// punctuation to look behind it, and shorten the address ONLY when a
/// marker is actually there. When nothing but punctuation trails, the span
/// is left alone, which keeps a genuine trailing `_` (rare, but real, and
/// preserving it is the whole point of this function). A trailing `_` counts
/// as a marker only when the character before the URL is also `_`, i.e. a
/// symmetric `_…_`.
fn url_len(chars: &[char], i: usize) -> Option<usize> {
    const SCHEMES: [&str; 2] = ["https://", "http://"];
    let head: String = chars[i..].iter().take(8).collect();
    let scheme = SCHEMES.iter().find(|s| head.starts_with(**s))?;
    let min_end = i + scheme.chars().count();
    let mut end = min_end;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    let opened_with_underscore = i > 0 && chars[i - 1] == '_';
    let is_marker = |c: char| matches!(c, '*' | '`' | '~') || (c == '_' && opened_with_underscore);

    let mut tail = end;
    while tail > min_end && is_sentence_tail(chars[tail - 1]) {
        tail -= 1;
    }
    let mut trimmed = tail;
    while trimmed > min_end && is_marker(chars[trimmed - 1]) {
        trimmed -= 1;
    }
    // Only give the tail back to the emphasis stripper when it really was
    // emphasis; otherwise the punctuation stays part of the span as before.
    if trimmed < tail {
        end = trimmed;
    }
    (end > min_end).then(|| end - i)
}

/// Strip inline emphasis markers, keeping the text they wrapped.
///
/// Two things have to survive this, and once did not:
///
/// * **URLs.** Everything from `http(s)://` to the next whitespace is
///   copied verbatim. An underscore inside a path is part of the address,
///   and deleting it produced a link that silently 404s — a family member
///   hit exactly that with a PDF whose filename carried six of them.
/// * **snake_case words.** CommonMark only treats `_` as emphasis at a word
///   boundary, precisely so identifiers and file names survive; we follow
///   the same rule. `*` keeps working inside a word, because `**bold**` is
///   written that way.
fn strip_emphasis(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(len) = url_len(&chars, i) {
            out.extend(chars[i..i + len].iter());
            i += len;
            continue;
        }
        match chars[i] {
            // Inside a word an underscore is literal, not emphasis.
            '_' if i > 0
                && i + 1 < chars.len()
                && is_word_char(chars[i - 1])
                && is_word_char(chars[i + 1]) =>
            {
                out.push('_');
            }
            '*' | '_' | '`' | '~' => {}
            c => out.push(c),
        }
        i += 1;
    }
    out.trim().to_string()
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

    /// The bug this module was fixed for: a link whose path carries
    /// underscores reached Telegram with them deleted, so it 404'd. The
    /// shape is the real one a family member hit — a bolded bare URL.
    #[test]
    fn a_bold_url_keeps_every_underscore_in_its_path() {
        let src = "**https://example.org/files/aam/Asia-Book_A_03_Social_Credit_System.pdf**";
        let out = markdown_to_telegram(src);
        assert_eq!(
            out,
            "https://example.org/files/aam/Asia-Book_A_03_Social_Credit_System.pdf",
            "the wrapping ** goes, the path survives intact"
        );
        assert_eq!(out.matches('_').count(), 5, "no underscore may be lost");
    }

    #[test]
    fn links_survive_in_every_shape_they_arrive_in() {
        // Markdown link: text (url), url untouched.
        assert_eq!(
            markdown_to_telegram("[the report](https://example.org/a_b_c.pdf)"),
            "the report (https://example.org/a_b_c.pdf)"
        );
        // Bare, in a sentence, with trailing punctuation.
        assert_eq!(
            markdown_to_telegram("See https://example.org/a_b_c.pdf for details."),
            "See https://example.org/a_b_c.pdf for details."
        );
        // In a bullet, and in a heading.
        assert_eq!(
            markdown_to_telegram("- https://example.org/x_y.pdf"),
            "• https://example.org/x_y.pdf"
        );
        assert_eq!(
            markdown_to_telegram("## https://example.org/x_y.pdf"),
            "https://example.org/x_y.pdf"
        );
        // Query strings and fragments keep their underscores too.
        assert_eq!(
            markdown_to_telegram("https://example.org/p?a_b=c_d&e=f#sec_2"),
            "https://example.org/p?a_b=c_d&e=f#sec_2"
        );
        // Inside a flattened table cell.
        let table = "| Source | Link |\n|---|---|\n| Study | https://example.org/a_b.pdf |";
        assert_eq!(
            markdown_to_telegram(table),
            "• Source: Study · Link: https://example.org/a_b.pdf"
        );
    }

    /// The shape a model emits most: a bolded link ending a sentence. The
    /// closing markers sit BEHIND the full stop, so a trim that only looks
    /// at the very last character leaves them glued to the address.
    #[test]
    fn a_link_ending_a_sentence_sheds_its_markers_and_keeps_the_stop() {
        for (src, want) in [
            ("The report is at **https://ex.org/a_b_c.pdf**.", "The report is at https://ex.org/a_b_c.pdf."),
            ("See **https://ex.org/a_b.pdf**, page 4.", "See https://ex.org/a_b.pdf, page 4."),
            ("Try `https://ex.org/a_b.pdf`; it works.", "Try https://ex.org/a_b.pdf; it works."),
            ("Read ~~https://ex.org/a_b.pdf~~!", "Read https://ex.org/a_b.pdf!"),
            ("_https://ex.org/a_b.pdf_?", "https://ex.org/a_b.pdf?"),
            ("(**https://ex.org/a_b.pdf**)", "(https://ex.org/a_b.pdf)"),
        ] {
            assert_eq!(markdown_to_telegram(src), want, "input: {src}");
        }
    }

    #[test]
    fn punctuation_with_no_marker_behind_it_leaves_the_address_alone() {
        // Nothing to shed here, so the span is untouched — in particular a
        // URL that genuinely ends in an underscore keeps it.
        assert_eq!(
            markdown_to_telegram("https://example.org/trailing_."),
            "https://example.org/trailing_."
        );
        assert_eq!(
            markdown_to_telegram("[doc](https://ex.org/a_b.pdf)."),
            "doc (https://ex.org/a_b.pdf)."
        );
    }

    #[test]
    fn emphasis_around_a_link_is_still_removed() {
        // Asterisks and backticks that wrap a link are markup, not address.
        assert_eq!(
            markdown_to_telegram("*https://example.org/a_b*"),
            "https://example.org/a_b"
        );
        assert_eq!(
            markdown_to_telegram("`https://example.org/a_b`"),
            "https://example.org/a_b"
        );
        // A symmetric _…_ around a link is emphasis; a trailing underscore
        // with no opener is assumed to belong to the address.
        assert_eq!(
            markdown_to_telegram("_https://example.org/a_b_"),
            "https://example.org/a_b"
        );
        assert_eq!(
            markdown_to_telegram("https://example.org/trailing_"),
            "https://example.org/trailing_"
        );
    }

    #[test]
    fn snake_case_prose_keeps_its_underscores_but_emphasis_still_goes() {
        // CommonMark's rule: `_` is emphasis only at a word boundary.
        assert_eq!(markdown_to_telegram("the file_name_here setting"), "the file_name_here setting");
        assert_eq!(markdown_to_telegram("set MAX_RETRY_COUNT to 3"), "set MAX_RETRY_COUNT to 3");
        assert_eq!(markdown_to_telegram("_really_ important"), "really important");
        assert_eq!(markdown_to_telegram("**bold** and *italic*"), "bold and italic");
        // Intra-word asterisks still strip — that is how **bold** is written.
        assert_eq!(markdown_to_telegram("a**b**c"), "abc");
        // Unicode words count as words on both sides.
        assert_eq!(markdown_to_telegram("Straße_Nr_7"), "Straße_Nr_7");
    }

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

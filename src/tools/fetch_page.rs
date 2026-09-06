//! Fetch a URL the user gave and return its text.
//!
//! Closes the "I can't open that link" gap: web_search only ever sees
//! engine snippets, so a linked 50-page paper reached the model as three
//! sentences of abstract. This tool downloads the page itself — HTML is
//! stripped to prose, PDFs run through the SAME extractor that chat
//! attachments use — so "read this" works for links the way it always
//! worked for attached files.
//!
//! Safety: only http(s); hosts resolving to private, loopback,
//! link-local or CGNAT addresses are refused BEFORE any request — a web
//! page must never be able to talk the model into probing the family's
//! own LAN (the llama servers, the host API, the router). Redirects are
//! followed manually so every hop is re-checked, the vetted DNS answer
//! is pinned against rebinding, and IPv6 forms that merely *wrap* an
//! IPv4 address (`::ffff:192.168.1.210` and friends) are unwrapped before
//! judging — an adversarial review found that exact bypass reaching the
//! llama servers.

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::sync::Semaphore;

const MAX_BYTES: usize = 25 * 1024 * 1024;
/// Pre-cap before the token budget trims further. Matches
/// `loop_pipeline::cap_for_tool("fetch_page")` so a document that
/// survives here is not silently re-cut a second time.
const MAX_TEXT_CHARS: usize = 48_000;
const MAX_REDIRECTS: usize = 3;
/// A legitimate 50-page paper parses in a second or two; a crafted PDF
/// can allocate gigabytes and run for minutes. See PDF_GATE.
const PDF_PARSE_TIMEOUT: Duration = Duration::from_secs(15);

/// Only one PDF parse at a time, process-wide.
///
/// `pdf_extract` offers no time or allocation budget, and the parse runs
/// on a blocking thread that nothing can cancel once started — a review
/// repro turned a 994 KB crafted PDF into 7.9 GB of RSS. The timeout
/// below bounds how long the TURN waits; this gate bounds how much
/// damage concurrent fetches can do, by holding the permit for the
/// lifetime of the blocking work rather than of the awaiting task.
/// Residual risk is one runaway parse, not four.
static PDF_GATE: Semaphore = Semaphore::const_new(1);

/// Marker prefix on refusals that can never succeed for this URL, so the
/// pipeline can tell the model "don't retry THIS url" instead of its
/// default "temporary failure, offer to try again".
pub(crate) const URL_REFUSED: &str = "URL REFUSED";

pub async fn fetch(url: &str) -> Result<String> {
    let mut current = url.trim().to_string();
    for _hop in 0..=MAX_REDIRECTS {
        let parsed = reqwest::Url::parse(&current)
            .map_err(|_| anyhow!("{URL_REFUSED}: that is not a valid URL: {current}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(anyhow!("{URL_REFUSED}: only http and https URLs can be fetched"));
        }
        let vetted = guard_host(&parsed).await?;

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(25))
            .redirect(reqwest::redirect::Policy::none());
        // Pin the addresses we just vetted — otherwise a DNS-rebinding
        // server could answer public for our check and private for the
        // request reqwest makes a moment later.
        if let Some(host) = parsed.domain() {
            builder = builder.resolve_to_addrs(host, &vetted);
        }
        let client = builder.build()?;
        let resp = client
            .get(parsed.clone())
            .header("User-Agent", user_agent())
            .send()
            .await?;

        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("redirect without a location"))?;
            current = parsed
                .join(loc)
                .map_err(|_| anyhow!("redirect to an invalid URL"))?
                .to_string();
            continue;
        }
        if !resp.status().is_success() {
            return Err(anyhow!("the server answered {} for {current}", resp.status()));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if let Some(len) = resp.content_length() {
            if len as usize > MAX_BYTES {
                return Err(anyhow!("that file is too large ({} MB; max 25 MB)", len / (1024 * 1024)));
            }
        }
        // Stream with the cap enforced mid-download — Content-Length is
        // the server's claim, not a promise.
        let mut bytes: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > MAX_BYTES {
                return Err(anyhow!("that file is too large (over 25 MB)"));
            }
            bytes.extend_from_slice(&chunk);
        }

        let text = if bytes.starts_with(b"%PDF") || content_type.contains("application/pdf") {
            extract_pdf(bytes).await?
        } else if looks_like_html(&bytes) || content_type.contains("html") || content_type.contains("xml") {
            let body = decode_body(&bytes, &content_type);
            html_to_text(&body)
        } else if content_type.starts_with("text/") || content_type.contains("json") || content_type.is_empty() {
            decode_body(&bytes, &content_type)
        } else {
            return Err(anyhow!(
                "{URL_REFUSED}: unsupported content type '{content_type}' — fetch_page reads web pages, PDFs and plain text"
            ));
        };

        let cleaned = collapse_whitespace(&text);
        if cleaned.trim().is_empty() {
            return Err(anyhow!("the page fetched but contained no readable text"));
        }
        let total = cleaned.chars().count();
        return Ok(if total > MAX_TEXT_CHARS {
            // Note goes FIRST: everything downstream truncates the head,
            // so a trailing note would be the first thing cut and the
            // model would never learn it was reading a fragment.
            let capped: String = cleaned.chars().take(MAX_TEXT_CHARS).collect();
            format!(
                "[fetch_page: this document is long — showing the first {MAX_TEXT_CHARS} of {total} characters. \
Say so if the answer depends on what was cut.]\n\n{capped}"
            )
        } else {
            cleaned
        });
    }
    Err(anyhow!("too many redirects fetching {url}"))
}

/// Parse a PDF off the async threads, under the gate and a wall clock.
async fn extract_pdf(bytes: Vec<u8>) -> Result<String> {
    // The permit is moved INTO the blocking closure, so it is released
    // when the parse actually ends — not when we stop waiting for it.
    let permit = PDF_GATE
        .acquire()
        .await
        .map_err(|_| anyhow!("PDF parser is unavailable"))?;
    let handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // pdf-extract panics on malformed input on some PDF versions —
        // same catch_unwind the attachment path uses.
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes))
            .map_err(|_| anyhow!("PDF parser crashed on this file"))?
            .map_err(|e| anyhow!("could not extract PDF text: {e}"))
    });
    match tokio::time::timeout(PDF_PARSE_TIMEOUT, handle).await {
        Ok(joined) => joined?,
        Err(_) => Err(anyhow!(
            "that PDF is too complex to read (parsing took over {}s). If you have the file, \
attaching it to the chat uses a different path that may still work.",
            PDF_PARSE_TIMEOUT.as_secs()
        )),
    }
}

/// Refuse hosts that resolve anywhere private, and return the vetted
/// addresses so the caller can pin them. Every resolved address is
/// checked — one public A record does not launder a private one.
async fn guard_host(url: &reqwest::Url) -> Result<Vec<SocketAddr>> {
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = match url.host() {
        None => return Err(anyhow!("{URL_REFUSED}: URL has no host")),
        Some(url::Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Some(url::Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Some(url::Host::Domain(host)) => {
            let lower = host.to_ascii_lowercase();
            if lower == "localhost"
                || lower.ends_with(".local")
                || lower.ends_with(".localhost")
                || lower.ends_with(".internal")
                || lower.ends_with(".home")
                || lower.ends_with(".lan")
            {
                return Err(anyhow!(
                    "{URL_REFUSED}: fetching local or internal addresses is not allowed"
                ));
            }
            tokio::net::lookup_host((host, port))
                .await
                .map_err(|_| anyhow!("could not resolve {host}"))?
                .collect()
        }
    };
    if addrs.is_empty() {
        return Err(anyhow!("could not resolve that host"));
    }
    if addrs.iter().any(|a| is_private(a.ip())) {
        return Err(anyhow!(
            "{URL_REFUSED}: fetching local or internal addresses is not allowed"
        ));
    }
    Ok(addrs)
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            // An IPv6 address that merely WRAPS an IPv4 one must be judged
            // as that IPv4 address: dual-stack sockets route
            // ::ffff:192.168.1.210 straight to the LAN. Checking only the
            // v6 prefixes let exactly that through (review finding, high).
            if let Some(v4) = unwrap_v4(v6) {
                return is_private_v4(v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique-local, fe80::/10 link-local,
                // fec0::/10 deprecated site-local
                || (v6.segments()[0] & 0xFE00) == 0xFC00
                || (v6.segments()[0] & 0xFFC0) == 0xFE80
                || (v6.segments()[0] & 0xFFC0) == 0xFEC0
        }
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        // CGNAT 100.64.0.0/10 — Tailscale-style overlay networks
        // are exactly as internal as the LAN.
        || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
        // 0.0.0.0/8 "this network" — 0.0.0.0 reaches localhost on Linux.
        || v4.octets()[0] == 0
}

/// Pull the IPv4 address out of every IPv6 form that carries one:
/// v4-mapped (::ffff:a.b.c.d), v4-compatible (::a.b.c.d), NAT64
/// (64:ff9b::/96) and 6to4 (2002:AABB:CCDD::/48).
fn unwrap_v4(v6: std::net::Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    let last_two = |s: [u16; 8]| {
        Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xFF) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xFF) as u8,
        )
    };
    // ::ffff:a.b.c.d  and  ::a.b.c.d (excluding :: and ::1 themselves)
    if seg[0..5] == [0, 0, 0, 0, 0] && (seg[5] == 0xFFFF || seg[5] == 0) {
        let v4 = last_two(seg);
        if !v4.is_unspecified() && v4 != Ipv4Addr::new(0, 0, 0, 1) {
            return Some(v4);
        }
        // ::1 / :: are handled by the plain v6 loopback checks.
        return None;
    }
    // 64:ff9b::/96 and 64:ff9b:1::/48 NAT64
    if seg[0] == 0x0064 && seg[1] == 0xFF9B {
        return Some(last_two(seg));
    }
    // 2002:AABB:CCDD::/48 — 6to4 embeds the v4 in segments 1..2
    if seg[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            (seg[1] & 0xFF) as u8,
            (seg[2] >> 8) as u8,
            (seg[2] & 0xFF) as u8,
        ));
    }
    None
}

/// Decode a body using the charset the server declared (or the one the
/// HTML itself declares), not a blind UTF-8 assumption — a
/// windows-1252 German page otherwise arrives as a field of U+FFFD.
fn decode_body(bytes: &[u8], content_type: &str) -> String {
    let label = charset_param(content_type).or_else(|| meta_charset(bytes));
    let enc = label
        .as_deref()
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    // encoding_rs honours a leading BOM over the declared label.
    enc.decode(bytes).0.into_owned()
}

fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|p| {
        let p = p.trim();
        p.strip_prefix("charset=")
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

/// `<meta charset=…>` / `<meta http-equiv=… content="…charset=…">` from
/// the head, where legacy pages usually declare it.
fn meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(2048)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let idx = text.find("charset")?;
    let rest = &text[idx + "charset".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let val: String = rest
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!val.is_empty()).then_some(val)
}

/// Does this look like markup regardless of what the server claimed?
/// Tolerates a BOM, leading whitespace and uppercase tags.
fn looks_like_html(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let s = String::from_utf8_lossy(head);
    let s = s.trim_start_matches('\u{feff}').trim_start().to_ascii_lowercase();
    s.starts_with("<!doctype") || s.starts_with("<html") || s.starts_with("<?xml") || s.starts_with("<head")
}

/// HTML to readable prose: drop comments, then script/style/head
/// wholesale, then strip the remaining tags. Deliberately dumb — the
/// goal is "the model can read the article", not a perfect render.
fn html_to_text(html: &str) -> String {
    // Comments FIRST: a commented-out `<script>` opener would otherwise
    // pair with the next real `</script>` and silently delete the
    // article between them (review finding).
    let mut s = remove_comments(html);
    // Lift the page's own summary out of <head> BEFORE it is dropped.
    // On a JavaScript app the head IS the content: a YouTube Shorts page
    // carries its title in `<meta name="title">` and nothing in the body
    // but the footer, so stripping the head left KinAI with "the page
    // only returned YouTube's standard footer" (field report,
    // 2026-09-03). News, social and product pages behave the same way.
    let meta = extract_page_metadata(&s);
    for tag in ["script", "style", "noscript", "head", "svg", "template"] {
        s = remove_element(&s, tag);
    }
    let mut out = String::with_capacity(s.len() / 2);
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                // Tags break words in source; keep them breaking words in text.
                out.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let body = decode_entities(&out);
    match meta {
        // The summary goes FIRST: everything downstream truncates the
        // head of the text, so a trailing header would be the first
        // thing cut on a long page.
        Some(m) => format!("{m}\n\n{body}"),
        None => body,
    }
}

/// Pull a page's own one-line summary out of `<head>`: `<title>` plus the
/// Open Graph / meta title and description.
///
/// Deliberately tolerant — attribute order varies (`content=` may precede
/// `property=`), quoting may be single or double, and the tag may span
/// lines. Scans only as far as `</head>` when present; YouTube's head is
/// ~700 KB, so a fixed byte window would miss it entirely.
fn extract_page_metadata(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    // Cap the scan, but land on a CHARACTER boundary: `fetch_page` eats
    // arbitrary pages, and a >1 MB document with no `</head>` and a
    // multibyte character straddling the cut would slice mid-character
    // and panic. `to_ascii_lowercase` preserves byte offsets, so the
    // index is valid in both strings.
    let head_end = lower.find("</head>").unwrap_or_else(|| {
        let mut cut = lower.len().min(1_000_000);
        while cut > 0 && !html.is_char_boundary(cut) {
            cut -= 1;
        }
        cut
    });
    let head = &html[..head_end];
    let head_lower = &lower[..head_end];

    let title = meta_content(head, head_lower, &["og:title", "twitter:title", "title"])
        .or_else(|| element_text(head, head_lower, "title"));
    let desc = meta_content(
        head,
        head_lower,
        &["og:description", "twitter:description", "description"],
    );

    let mut out: Vec<String> = Vec::new();
    if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
        out.push(t);
    }
    // Skip a description that merely repeats the title.
    if let Some(d) = desc.filter(|d| !d.trim().is_empty()) {
        if out.first().map(|t| t.trim() != d.trim()).unwrap_or(true) {
            out.push(d);
        }
    }
    (!out.is_empty()).then(|| out.join("\n"))
}

/// First `<meta>` whose `name`/`property` matches one of `keys`, in the
/// order given, returning its decoded `content`.
fn meta_content(head: &str, head_lower: &str, keys: &[&str]) -> Option<String> {
    for key in keys {
        let mut from = 0;
        while let Some(rel) = head_lower[from..].find("<meta") {
            let start = from + rel;
            let end = head_lower[start..].find('>').map(|e| start + e)?;
            let tag = &head[start..end];
            let tag_lower = &head_lower[start..end];
            let matches_key = attr(tag, tag_lower, "property")
                .or_else(|| attr(tag, tag_lower, "name"))
                .map(|v| v.trim().eq_ignore_ascii_case(key))
                .unwrap_or(false);
            if matches_key {
                if let Some(c) = attr(tag, tag_lower, "content") {
                    let c = decode_entities(&c).trim().to_string();
                    if !c.is_empty() {
                        return Some(c);
                    }
                }
            }
            from = end;
        }
    }
    None
}

/// Value of `name="..."` (or `'...'`) within a single tag.
fn attr(tag: &str, tag_lower: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=");
    let mut from = 0;
    while let Some(rel) = tag_lower[from..].find(&pat) {
        let at = from + rel;
        // Must be a attribute boundary, not the tail of another name
        // (e.g. "og:title=" must not satisfy a search for "title=").
        let boundary = at == 0
            || tag
                .as_bytes()
                .get(at - 1)
                .is_some_and(|b| b.is_ascii_whitespace());
        let rest = &tag[at + pat.len()..];
        if boundary {
            let quote = rest.chars().next()?;
            if quote == '"' || quote == '\'' {
                let body = &rest[quote.len_utf8()..];
                if let Some(close) = body.find(quote) {
                    return Some(body[..close].to_string());
                }
            }
        }
        from = at + pat.len();
    }
    None
}

/// Text of the first `<title>` element.
fn element_text(head: &str, head_lower: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let s = head_lower.find(&open)?;
    let gt = head_lower[s..].find('>')? + s + 1;
    let e = head_lower[gt..].find(&close)? + gt;
    let text = decode_entities(head[gt..e].trim());
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

fn remove_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    while let Some(rel) = s[pos..].find("<!--") {
        let start = pos + rel;
        out.push_str(&s[pos..start]);
        match s[start..].find("-->") {
            Some(end) => pos = start + end + 3,
            None => return out, // unterminated comment — drop the rest
        }
    }
    out.push_str(&s[pos..]);
    out
}

/// Decode the handful of entities that matter for prose.
///
/// `&amp;` MUST be replaced last: doing it first turns the escaped text
/// `&amp;lt;` into `<`, so a page ABOUT html grows phantom tags.
pub(crate) fn decode_entities(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn remove_element(s: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    while let Some(rel) = find_tag_start(&lower[pos..], &open) {
        let start = pos + rel;
        out.push_str(&s[pos..start]);
        pos = match find_tag_start(&lower[start..], &close)
            .and_then(|c| lower[start + c..].find('>').map(|gt| start + c + gt + 1))
        {
            Some(after_close) => after_close,
            // No close tag — legal for <head> in HTML5. Skip only the
            // opening tag and keep scanning; leaking a little of the
            // element's content beats blanking the whole document.
            None => match lower[start..].find('>') {
                Some(gt) => start + gt + 1,
                None => return out,
            },
        };
    }
    out.push_str(&s[pos..]);
    out
}

/// Find `pat` only where the next byte ends the tag name, so "<head"
/// matches "<head>" but not "<header>" — the header-vs-head collision
/// silently swallowed everything after Wikipedia's first <header>.
fn find_tag_start(haystack: &str, pat: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = haystack[from..].find(pat) {
        let at = from + i;
        match haystack.as_bytes().get(at + pat.len()) {
            None | Some(b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/') => return Some(at),
            _ => from = at + 1,
        }
    }
    None
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_lines = 0;
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank_lines += 1;
            if blank_lines <= 1 {
                out.push('\n');
            }
        } else {
            blank_lines = 0;
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

fn user_agent() -> String {
    format!(
        "Mozilla/5.0 (compatible; KinAI/{} +{})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses_are_refused() {
        for ip in ["192.168.1.210", "10.0.0.1", "127.0.0.1", "169.254.1.1", "100.64.0.5", "0.0.0.0"] {
            assert!(is_private(ip.parse().unwrap()), "{ip} must be private");
        }
        for ip in ["142.250.72.14", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip} must be public");
        }
    }

    #[test]
    fn ipv6_wrapped_ipv4_cannot_smuggle_the_lan() {
        // The high-severity review finding: every one of these routes to
        // an IPv4 host on a dual-stack socket, so every one must be
        // judged as that IPv4 address, not as an opaque v6 address.
        for ip in [
            "::ffff:192.168.1.210",  // v4-mapped — the llama server
            "::ffff:192.168.1.211",  // the other llama server
            "::ffff:127.0.0.1",     // loopback / host API
            "::ffff:10.0.0.1",
            "::192.168.1.210",       // v4-compatible
            "64:ff9b::192.168.1.210", // NAT64
            "2002:c0a8:0119::1",    // 6to4 wrapping 192.168.1.210
            "fec0::1",              // deprecated site-local
        ] {
            assert!(is_private(ip.parse().unwrap()), "{ip} must be refused");
        }
        // A real public address in each of those shapes still resolves.
        assert!(!is_private("::ffff:1.1.1.1".parse().unwrap()));
        assert!(!is_private("2002:0101:0101::1".parse().unwrap()));
    }

    #[test]
    fn html_becomes_readable_prose() {
        let html = "<html><head><title>x</title><style>body{}</style></head>\
<body><script>evil()</script><h1>The Paper</h1><p>Findings &amp; results.</p></body></html>";
        let text = collapse_whitespace(&html_to_text(html));
        assert!(text.contains("The Paper"));
        assert!(text.contains("Findings & results."));
        assert!(!text.contains("evil"));
        assert!(!text.contains("body{}"));
    }

    /// The 2026-09-03 field report: "https://www.youtube.com/shorts/… —
    /// watch this" came back as "the page behind the link only returned
    /// YouTube's standard footer (no title, description, or transcript)".
    /// The title was in the page the whole time, in a <meta> inside
    /// <head>, which this function used to drop wholesale.
    #[test]
    fn a_javascript_app_still_yields_its_title_and_description() {
        // Shape copied from the real Shorts page: og: tags in a big head,
        // body carrying only chrome.
        let html = "<!DOCTYPE html><html><head><title>Nvidia's RELEASING A New Flagship Gaming GPU! - YouTube</title>\
<meta name=\"title\" content=\"Nvidia&#39;s RELEASING A New Flagship Gaming GPU!\">\
<meta property=\"og:title\" content=\"Nvidia&#39;s RELEASING A New Flagship Gaming GPU!\">\
<meta property=\"og:description\" content=\"Visit https://www.meldbytes.com to join the newsletter!\">\
<script>var ytInitialData = {\"junk\":1};</script></head>\
<body><div>About Press Copyright Contact us Creators Advertise Developers</div></body></html>";
        let text = collapse_whitespace(&html_to_text(html));
        assert!(text.contains("Nvidia's RELEASING A New Flagship Gaming GPU!"), "no title: {text:?}");
        assert!(text.contains("meldbytes.com"), "no description: {text:?}");
        // The summary must lead, because everything downstream cuts the head.
        assert!(text.starts_with("Nvidia's"), "summary must come first: {text:?}");
        // And the script junk must still be gone.
        assert!(!text.contains("ytInitialData"), "script leaked: {text:?}");
    }

    #[test]
    fn metadata_parsing_tolerates_real_world_markup() {
        // content= before property=, single quotes, mixed case.
        let html = "<html><head><META CONTENT='Reversed &amp; quoted' PROPERTY='og:title'>\
<meta name='description' content='Desc here'></head><body>x</body></html>";
        let text = collapse_whitespace(&html_to_text(html));
        assert!(text.contains("Reversed & quoted"), "{text:?}");
        assert!(text.contains("Desc here"), "{text:?}");
    }

    #[test]
    fn a_description_that_repeats_the_title_is_not_printed_twice() {
        let html = "<html><head><meta property=\"og:title\" content=\"Same thing\">\
<meta property=\"og:description\" content=\"Same thing\"></head><body>body text</body></html>";
        let text = collapse_whitespace(&html_to_text(html));
        assert_eq!(text.matches("Same thing").count(), 1, "duplicated: {text:?}");
    }

    #[test]
    fn an_ordinary_article_is_unchanged_apart_from_its_title() {
        // Regression guard: the arXiv/Wikipedia path must keep working.
        let html = "<html><head><title>The Paper</title></head><body><h1>The Paper</h1>\
<p>Findings &amp; results.</p></body></html>";
        let text = collapse_whitespace(&html_to_text(html));
        assert!(text.contains("Findings & results."), "{text:?}");
        assert!(text.starts_with("The Paper"), "{text:?}");
    }

    /// `fetch_page` eats arbitrary pages, so the head scan must not
    /// panic on one. A document over the 1 MB scan window with no
    /// `</head>` and a multibyte character straddling the cut would
    /// slice mid-character — an instant panic on untrusted input.
    #[test]
    fn a_huge_headless_page_with_multibyte_text_does_not_panic() {
        let mut html = String::from("<html><body>");
        html.push_str(&"a".repeat(999_999 - html.len()));
        html.push_str(&"€".repeat(200)); // 3 bytes each, straddles 1_000_000
        html.push_str("</body></html>");
        assert!(html.len() > 1_000_000);
        let _ = html_to_text(&html); // must not panic
    }

    #[test]
    fn a_page_with_no_metadata_is_untouched() {
        let html = "<html><head><style>body{}</style></head><body><p>Just body.</p></body></html>";
        let text = collapse_whitespace(&html_to_text(html));
        assert_eq!(text.trim(), "Just body.");
    }

    #[test]
    fn header_element_does_not_get_eaten_as_head() {
        // The prefix collision that blanked Wikipedia: <header> must not
        // match the removal of <head>, and content after it must survive.
        let html = "<html><head><title>t</title></head><body>\
<header><nav>Menu</nav></header><main><p>The article body.</p></main></body></html>";
        let text = collapse_whitespace(&html_to_text(html));
        assert!(text.contains("The article body."), "body vanished: {text:?}");
        // <header> must survive the <head> removal — that prefix collision
        // is what this test exists for.
        assert!(text.contains("Menu"), "<header> was eaten as <head>: {text:?}");
        // The <title> now legitimately leads the text (metadata extraction),
        // but the head's MARKUP must still be gone.
        assert!(!text.contains("<title"), "head markup leaked: {text:?}");
    }

    #[test]
    fn unclosed_head_does_not_blank_the_document() {
        // HTML5 allows omitting </head>; the parser must not treat that
        // as "head runs to end of file".
        let html = "<html><head><title>t</title><body><p>Real content here.</p>";
        let text = collapse_whitespace(&html_to_text(html));
        assert!(text.contains("Real content here."), "document blanked: {text:?}");
    }

    #[test]
    fn commented_out_script_opener_does_not_delete_the_article() {
        // Review finding: a disabled analytics snippet inside a comment
        // paired with the NEXT real </script> and ate the body between.
        let html = "<p>A</p><!-- <script> disabled --><p>Important body text</p>\
<script>x()</script><p>B</p>";
        let text = collapse_whitespace(&html_to_text(html));
        assert!(text.contains("Important body text"), "body deleted: {text:?}");
        assert!(text.contains('B'));
        assert!(!text.contains("x()"), "real script leaked: {text:?}");
        assert!(!text.contains("disabled"), "comment leaked: {text:?}");
    }

    #[test]
    fn escaped_entities_are_not_double_decoded() {
        // A page about HTML writes &amp;lt; to show the literal "&lt;".
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_entities("a &lt;b&gt; c &amp; d"), "a <b> c & d");
    }

    #[test]
    fn declared_charset_is_honoured() {
        // "Preisstück" in windows-1252: ü = 0xFC.
        let bytes = b"Preisst\xFCck";
        let decoded = decode_body(bytes, "text/html; charset=windows-1252");
        assert_eq!(decoded, "Preisstück");
        // Without the header, the meta tag carries it.
        let meta = b"<html><head><meta charset=\"iso-8859-1\"></head><body>caf\xE9</body></html>";
        assert!(decode_body(meta, "text/html").contains("café"));
        // UTF-8 stays correct when nothing is declared.
        assert_eq!(decode_body("Grüße".as_bytes(), ""), "Grüße");
    }

    #[test]
    fn html_is_detected_despite_bom_whitespace_and_case() {
        assert!(looks_like_html("\u{feff}  <!DOCTYPE html><html>".as_bytes()));
        assert!(looks_like_html(b"\n\n<HTML><body>hi</body></HTML>"));
        assert!(!looks_like_html(b"Just some plain text."));
    }

    #[tokio::test]
    async fn policy_refusals_are_marked_permanent() {
        // The pipeline keys off this marker to stop the model offering a
        // retry that can never work. No network is touched: the scheme
        // and the literal-IP guard both reject before any request.
        for bad in ["ftp://example.com/x", "http://192.168.1.210:8081/", "http://[::ffff:192.168.1.210]/"] {
            let err = fetch(bad).await.unwrap_err();
            assert!(err.to_string().starts_with(URL_REFUSED), "{bad} got: {err}");
        }
    }
}

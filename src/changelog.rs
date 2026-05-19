//! In-app changelog ("What's new in 0.2.x") modal.
//!
//! On first launch of a freshly-updated KinAI, we pop a modal that
//! shows the user what changed since they last opened the app. The
//! source of truth is `CHANGELOG.md` at the repo root — we embed it
//! into the binary at compile time via `include_str!`, then extract
//! the section for the current `CARGO_PKG_VERSION` on demand.
//!
//! Why "embed" rather than "fetch from GitHub":
//!   - Works offline.
//!   - Doesn't expose KinAI traffic to GitHub on first launch.
//!   - The user reads what they actually have installed, not what's
//!     on GitHub (could drift if someone edits a release body after
//!     the fact).
//!
//! Bookkeeping lives in `AppConfig::last_seen_changelog_version`.
//! When the user dismisses the modal, the current version stamps
//! into that field; we only re-open on the next version bump.

/// Raw CHANGELOG.md content baked into the binary. Updated by editing
/// the file at the repo root and rebuilding.
const CHANGELOG_MD: &str = include_str!("../CHANGELOG.md");

/// Pull the markdown body for `version` out of the embedded changelog.
///
/// Matches lines like `## [0.2.13]` (Keep-a-Changelog format) and
/// returns everything up to the next `## [` heading. Whitespace at
/// the start of the section is trimmed. Returns `None` when no entry
/// for that version exists (e.g. on a dev build that hasn't been
/// added to CHANGELOG.md yet).
pub fn section_for_version(version: &str) -> Option<String> {
    let needle = format!("## [{version}]");
    let start = CHANGELOG_MD.find(&needle)?;
    let after_heading = &CHANGELOG_MD[start..];
    // Locate the next `## [` heading (next version entry) or EOF.
    let end_rel = after_heading[needle.len()..]
        .find("\n## [")
        .map(|i| i + needle.len())
        .unwrap_or(after_heading.len());
    Some(after_heading[..end_rel].trim().to_string())
}

/// Current binary version, as advertised by Cargo.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_existing_version() {
        let entry = section_for_version("0.2.0").expect("0.2.0 entry must exist");
        assert!(entry.starts_with("## [0.2.0]"));
        // Must not bleed into the next section.
        assert!(!entry.contains("## [0.1.x]"));
    }

    #[test]
    fn missing_version_returns_none() {
        assert!(section_for_version("99.99.99").is_none());
    }
}

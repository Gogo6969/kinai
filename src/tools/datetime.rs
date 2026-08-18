//! Current date / time on the host machine.

use chrono::Local;

/// Date-only form for the SYSTEM prompt. Deliberately no clock: llama.cpp
/// caches the prompt by comparing tokens from position 0, and a
/// minute-resolution timestamp in the first ~40 tokens invalidated the
/// entire cache every minute — measured on the Olares fast slot as a full
/// ~2.3k-token reprocess per turn (830 ms) versus 41 tokens (165 ms) with
/// a stable prefix. The precise clock now rides with the newest user
/// message, which is uncached anyway.
pub fn today_pretty() -> String {
    let now = Local::now();
    format!(
        "{} (timezone {})",
        now.format("%A, %B %-d, %Y"),
        now.format("%Z")
    )
}

pub fn now_pretty() -> String {
    let now = Local::now();
    format!(
        "{} (timezone {})",
        now.format("%A, %B %-d, %Y %-I:%M %p"),
        now.format("%Z")
    )
}

/// The current year on the host — used to anchor tool descriptions in real
/// time (models anchor "recent" to their training years otherwise).
pub fn current_year() -> i32 {
    use chrono::Datelike;
    chrono::Local::now().year()
}

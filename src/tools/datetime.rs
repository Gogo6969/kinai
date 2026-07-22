//! Current date / time on the host machine.

use chrono::Local;

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

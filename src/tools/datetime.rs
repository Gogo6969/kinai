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

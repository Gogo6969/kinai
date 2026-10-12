//! Put every crash in the app log — where it happened, never what was being
//! processed.
//!
//! A panic in a spawned task (one Telegram update, one family message) ends
//! only that task. The app carries on, so nothing else notices, and the
//! default hook prints to stderr, which a Finder-launched app throws away.
//! That is how a whole class of messages vanished without a trace on
//! 2026-10-12: no reply, no stored row, no log line.
//!
//! The panic MESSAGE is deliberately left out. It can quote the very text
//! being processed (that night's said `byte index 6 is not a char boundary …
//! of` followed by the user's sentence), and the log must never hold what a
//! family member typed.

use std::sync::Once;

/// The log line for a crash: the thread and the source location only.
pub fn log_line(thread: &str, location: Option<(&str, u32)>) -> String {
    const NOTE: &str = "message withheld: it can quote what a family member typed";
    match location {
        Some((file, line)) => format!("panic in thread '{thread}' at {file}:{line} ({NOTE})"),
        None => format!("panic in thread '{thread}' at an unknown location ({NOTE})"),
    }
}

/// Log panics through `tracing`, then run the hook that was there before
/// (so stderr and test output behave as usual). Safe to call twice.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let line = log_line(
                thread.name().unwrap_or("unnamed"),
                info.location().map(|l| (l.file(), l.line())),
            );
            tracing::error!("{line}");
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    #[test]
    fn the_line_names_the_place_and_nothing_else() {
        let l = log_line("tokio-rt-worker", Some(("src/telegram/router.rs", 1408)));
        assert!(l.contains("tokio-rt-worker") && l.contains("src/telegram/router.rs:1408"), "{l}");
        assert!(log_line("main", None).contains("unknown location"));
    }

    /// The real hook, end to end: a panic that quotes private text lands in the
    /// log as a located line, and the text itself does not.
    #[test]
    fn a_crash_reaches_the_log_without_the_text_it_quoted() {
        install();
        install(); // a second call must not double-log
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let outcome = std::panic::catch_unwind(|| {
                let private = String::from("What\u{2019}s my private question");
                let _ = &private[..6]; // cuts the curly apostrophe in half
            });
            assert!(outcome.is_err(), "the cut must panic for this test to mean anything");
        });
        let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains("panic in thread"), "no crash line in: {log}");
        assert!(log.contains("panic_log.rs"), "the location is the point: {log}");
        assert!(!log.contains("private question"), "the user's text leaked into the log: {log}");
        assert_eq!(log.matches("panic in thread").count(), 1, "logged twice: {log}");
    }
}

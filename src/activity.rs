//! Is somebody using this device right now?
//!
//! One question, asked by the self-installing update (`auto_update.rs`):
//! may KinAI restart itself this minute without taking the app away from
//! a person? Two things say no —
//!
//!   * somebody touched KinAI lately: a key, a click, a question sent, a
//!     piece of an answer arriving;
//!   * a question is out and its answer has not finished. A slow model
//!     can think for minutes without sending a word, so "nothing arrived
//!     lately" is not enough to call the device idle;
//!   * somebody is at the computer, in whatever program. KinAI sitting
//!     untouched in the tray while its owner writes a letter is not an
//!     idle device — and on Windows the installer shows a small progress
//!     window, which has no business appearing in the middle of that
//!     letter. The system knows when a key or the mouse was last used
//!     anywhere (`os_idle_secs`); on Linux there is no one way to ask,
//!     so there the first two have to do.
//!
//! Everything here is timestamps. Nothing about WHAT was typed or asked,
//! or in which program, is read or kept.

use std::sync::atomic::{AtomicI64, Ordering};

/// How long the device has to sit untouched before it counts as free.
pub const QUIET_SECS: i64 = 10 * 60;

/// How long an unanswered question keeps the device "in use". An answer
/// that never comes (the host went away mid-question) must not hold an
/// update back for ever, so the claim lapses.
pub const ANSWER_GRACE_SECS: i64 = 30 * 60;

/// Unix time of the last sign of use. `0` = not started yet.
static LAST_USE: AtomicI64 = AtomicI64::new(0);
/// Unix time the oldest still-unanswered question was sent. `0` = none.
static AWAITING_SINCE: AtomicI64 = AtomicI64::new(0);

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Somebody (or an arriving answer) just used the app.
pub fn touch() {
    LAST_USE.store(now_unix(), Ordering::Relaxed);
}

/// A question went out to the host.
pub fn question_sent() {
    let now = now_unix();
    LAST_USE.store(now, Ordering::Relaxed);
    // Keep the OLDEST open question: a second one sent while the first
    // is still running must not make the first look younger.
    let _ = AWAITING_SINCE.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
}

/// An answer finished (or the host said it failed).
pub fn answer_finished() {
    LAST_USE.store(now_unix(), Ordering::Relaxed);
    AWAITING_SINCE.store(0, Ordering::Relaxed);
}

/// Why the device is not free right now — `None` when it is.
pub fn busy_reason() -> Option<&'static str> {
    let now = now_unix();
    let last_use = LAST_USE.load(Ordering::Relaxed);
    let awaiting = AWAITING_SINCE.load(Ordering::Relaxed);
    // KinAI's own record first; the system is only asked once KinAI
    // itself looks free.
    busy_reason_at(now, last_use, awaiting, None)
        .or_else(|| busy_reason_at(now, last_use, awaiting, os_idle_secs()))
}

/// Seconds since a key or the mouse was last used ANYWHERE on this
/// computer — `None` where it cannot be asked, or when the system does
/// not answer within two seconds.
///
/// The time limit is not decoration. The first version asked
/// CoreGraphics (`CGEventSourceSecondsSinceLastEventType`), and in the
/// test binary that one call sat for 260 seconds before it answered. The
/// question is asked from a background task every 30 seconds while an
/// update waits; a call that can take minutes there would take the task
/// down with it. So: a different source on the Mac (below), and whatever
/// the source, the answer is fetched on its own thread and abandoned if
/// it is late. While one question is still out, no second one is sent —
/// a call that hangs costs one thread, not one every 30 seconds.
pub fn os_idle_secs() -> Option<i64> {
    use std::sync::atomic::AtomicBool;
    static ASKING: AtomicBool = AtomicBool::new(false);
    if ASKING.swap(true, Ordering::SeqCst) {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let answer = os_idle_secs_raw();
        ASKING.store(false, Ordering::SeqCst);
        let _ = tx.send(answer);
    });
    rx.recv_timeout(std::time::Duration::from_secs(2)).ok().flatten()
}

/// macOS: the HID system's own idle counter, read from the I/O registry
/// (`HIDIdleTime`, nanoseconds). Needs no window-server connection and no
/// permission; it is a single number, not a view of what was typed.
#[cfg(target_os = "macos")]
fn os_idle_secs_raw() -> Option<i64> {
    use std::ffi::{c_char, c_void, CString};
    type CfRef = *const c_void;
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const c_char) -> CfRef;
        fn IOServiceGetMatchingService(main_port: u32, matching: CfRef) -> u32;
        fn IORegistryEntryCreateCFProperty(entry: u32, key: CfRef, allocator: CfRef, options: u32) -> CfRef;
        fn IOObjectRelease(object: u32) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: CfRef, c_str: *const c_char, encoding: u32) -> CfRef;
        fn CFGetTypeID(cf: CfRef) -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFNumberGetValue(number: CfRef, the_type: isize, value_ptr: *mut c_void) -> u8;
        fn CFRelease(cf: CfRef);
    }
    const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8
    const SINT64: isize = 4; // kCFNumberSInt64Type

    let class = CString::new("IOHIDSystem").ok()?;
    let key = CString::new("HIDIdleTime").ok()?;
    unsafe {
        // `IOServiceGetMatchingService` takes over the matching dictionary.
        let matching = IOServiceMatching(class.as_ptr());
        if matching.is_null() {
            return None;
        }
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        let key = CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), UTF8);
        let value = if key.is_null() {
            std::ptr::null()
        } else {
            let v = IORegistryEntryCreateCFProperty(service, key, std::ptr::null(), 0);
            CFRelease(key);
            v
        };
        IOObjectRelease(service);
        if value.is_null() {
            return None;
        }
        let mut nanos: i64 = -1;
        let ok = CFGetTypeID(value) == CFNumberGetTypeID()
            && CFNumberGetValue(value, SINT64, (&mut nanos as *mut i64).cast()) != 0;
        CFRelease(value);
        (ok && nanos >= 0).then_some(nanos / 1_000_000_000)
    }
}

/// Windows: when this session last saw a key or the mouse.
#[cfg(windows)]
fn os_idle_secs_raw() -> Option<i64> {
    #[repr(C)]
    struct LastInputInfo {
        cb_size: u32,
        dw_time: u32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetLastInputInfo(plii: *mut LastInputInfo) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTickCount() -> u32;
    }
    let mut info = LastInputInfo {
        cb_size: std::mem::size_of::<LastInputInfo>() as u32,
        dw_time: 0,
    };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // Both are milliseconds since boot in 32 bits; they wrap together
    // after 49 days, and the wrapping difference is still right.
    let idle_ms = unsafe { GetTickCount() }.wrapping_sub(info.dw_time);
    Some(i64::from(idle_ms / 1000))
}

/// Linux: X11 and Wayland each answer differently, or not at all.
#[cfg(not(any(target_os = "macos", windows)))]
fn os_idle_secs_raw() -> Option<i64> {
    None
}

/// How long the device has been untouched, in whole minutes (for the log).
pub fn quiet_minutes() -> i64 {
    let last = LAST_USE.load(Ordering::Relaxed);
    if last == 0 {
        return 0;
    }
    let in_kinai = (now_unix() - last).max(0);
    // The shorter of the two is how long NOBODY has been here.
    let quiet = os_idle_secs().map_or(in_kinai, |anywhere| in_kinai.min(anywhere));
    quiet / 60
}

/// The decision itself, with the clock passed in so it can be tested.
/// `os_idle` is the system's own "seconds since anybody touched this
/// computer", where it can be asked.
pub(crate) fn busy_reason_at(
    now: i64,
    last_use: i64,
    awaiting_since: i64,
    os_idle: Option<i64>,
) -> Option<&'static str> {
    if last_use == 0 {
        // Never touched since launch means the launch has not been
        // recorded yet — treat that as "just started", not as "idle".
        return Some("the app has only just started");
    }
    if awaiting_since != 0 && now - awaiting_since < ANSWER_GRACE_SECS {
        return Some("an answer is still on its way");
    }
    if now - last_use < QUIET_SECS {
        return Some("KinAI was used a moment ago");
    }
    if os_idle.is_some_and(|idle| idle < QUIET_SECS) {
        return Some("somebody is at this computer");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 2_000_000_000;

    #[test]
    fn a_device_nobody_touched_for_ten_minutes_is_free() {
        assert_eq!(busy_reason_at(T, T - QUIET_SECS, 0, None), None);
        assert_eq!(busy_reason_at(T, T - 3 * 60 * 60, 0, Some(3 * 60 * 60)), None);
    }

    #[test]
    fn a_recent_key_or_click_keeps_it_in_use() {
        assert!(busy_reason_at(T, T - 5, 0, None).is_some());
        assert!(busy_reason_at(T, T - (QUIET_SECS - 1), 0, None).is_some());
    }

    /// A slow model can be silent for many minutes in the middle of an
    /// answer. Restarting then would throw the question away in front of
    /// the person waiting for it.
    #[test]
    fn a_question_without_its_answer_keeps_it_in_use_even_when_nothing_arrives() {
        let asked = T - 20 * 60;
        assert_eq!(
            busy_reason_at(T, asked, asked, None),
            Some("an answer is still on its way"),
            "twenty silent minutes into an answer is still an answer"
        );
    }

    /// …but a question whose host vanished must not block updates for ever.
    #[test]
    fn an_answer_that_never_comes_stops_counting_after_half_an_hour() {
        let asked = T - ANSWER_GRACE_SECS;
        assert_eq!(busy_reason_at(T, asked, asked, None), None);
    }

    /// KinAI in the tray for hours, its owner busy in another program: the
    /// app looks idle, the computer is not. An update now would put an
    /// installer window into the middle of their work.
    #[test]
    fn somebody_working_in_another_program_keeps_it_in_use() {
        let untouched_for_hours = T - 3 * 60 * 60;
        assert_eq!(
            busy_reason_at(T, untouched_for_hours, 0, Some(4)),
            Some("somebody is at this computer")
        );
        assert_eq!(busy_reason_at(T, untouched_for_hours, 0, Some(QUIET_SECS)), None);
        // Where the system cannot be asked (Linux), KinAI's own record decides.
        assert_eq!(busy_reason_at(T, untouched_for_hours, 0, None), None);
    }

    /// The real call, on the machine running the tests: it has to answer
    /// with a sane number, not garbage, on the systems that claim to
    /// support it.
    #[test]
    fn the_system_reports_a_plausible_idle_time_and_does_so_at_once() {
        let asked = std::time::Instant::now();
        let answer = os_idle_secs_raw();
        let took = asked.elapsed();
        match answer {
            Some(secs) => assert!((0..=10 * 365 * 24 * 3600).contains(&secs), "implausible: {secs}"),
            None => assert!(cfg!(not(any(target_os = "macos", windows))), "should be available here"),
        }
        // This is the test that caught the first version: 260 seconds.
        assert!(took.as_secs() < 2, "the system took {took:?} to answer");
    }

    /// Whatever the system does, the caller gets an answer or a "don't
    /// know" within the time limit, and the guarded call agrees with the
    /// raw one on whether an answer exists at all.
    #[test]
    fn the_guarded_question_never_waits_longer_than_its_limit() {
        let asked = std::time::Instant::now();
        let guarded = os_idle_secs();
        assert!(asked.elapsed().as_secs() < 3);
        if cfg!(any(target_os = "macos", windows)) {
            assert!(guarded.is_some(), "an answer was available and was lost");
        } else {
            assert_eq!(guarded, None);
        }
    }

    /// The first minutes after a launch are somebody opening the app, or a
    /// restart we caused ourselves — neither is a moment to restart again.
    #[test]
    fn a_fresh_launch_is_not_idle() {
        assert!(busy_reason_at(T, 0, 0, None).is_some(), "launch not recorded yet");
        assert!(busy_reason_at(T, T, 0, None).is_some(), "launched this second");
    }
}

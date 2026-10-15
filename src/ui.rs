//! v0.3 — the session view. When stdout is a real terminal, the session loop
//! redraws this frame every second: countdown, progress bar, reading-allowance
//! gauge, and a status line (asking for movement / paused / welcome back).
//! When stdout is piped or scripted, main.rs prints a plain one-liner instead.
//!
//! This module is a pure renderer: no cursor moves, no screen clearing, no
//! I/O — it just formats text. All terminal control codes live in main.rs,
//! which decides whether to use fancy output at all.

/// Gauge width, in cells. Filled bars use `█`, empty track uses `░`.
pub const BAR_WIDTH: usize = 20;

/// Which half of the loop we're in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Your focus work: the cell.
    Focus,
    /// Your earned break: the yard is open.
    Rest,
}

/// One second of session state, ready to be painted.
pub struct Frame<'a> {
    pub task: &'a str,
    pub plan: &'a str,
    pub phase: Phase,
    /// Elapsed credit this phase (focused seconds / rest seconds used).
    pub done_secs: u64,
    /// Phase length in seconds.
    pub total_secs: u64,
    /// Reading allowance granted for this session (focus only).
    pub allowance_total_secs: u64,
    /// Saved idle seconds inside the allowance (focus only).
    pub allowance_left_secs: u64,
    /// Status line under the bar: "Still there?", paused, welcome back…
    pub status: String,
    /// ANSI colour-code number (e.g. Some("32") green) or None for plain.
    pub accent: Option<&'static str>,
}

impl<'a> Frame<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn focus(
        task: &'a str,
        plan: &'a str,
        done_secs: u64,
        total_secs: u64,
        allowance_total_secs: u64,
        allowance_left_secs: u64,
        status: String,
        accent: Option<&'static str>,
    ) -> Self {
        Self {
            task,
            plan,
            phase: Phase::Focus,
            done_secs,
            total_secs,
            allowance_total_secs,
            allowance_left_secs,
            status,
            accent,
        }
    }

    pub fn rest(
        task: &'a str,
        plan: &'a str,
        done_secs: u64,
        total_secs: u64,
        status: String,
        accent: Option<&'static str>,
    ) -> Self {
        Self {
            task,
            plan,
            phase: Phase::Rest,
            done_secs,
            total_secs,
            allowance_total_secs: 0,
            allowance_left_secs: 0,
            status,
            accent,
        }
    }
}

/// `125` -> "02:05". Minutes can exceed 59 for deep plans? No: capped by
/// 90 min focus, so "90:00" is the largest value shown.
pub fn mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Filled cells for a progress bar of [done..=total], rounded to halves.
fn bar_fill(done: u64, total: u64) -> usize {
    if total == 0 {
        return BAR_WIDTH;
    }
    // round-half-up without floats
    let fill = ((done * 2 * BAR_WIDTH as u64 + total) / (total * 2)) as usize;
    fill.min(BAR_WIDTH)
}

fn bar(done: u64, total: u64) -> String {
    let fill = bar_fill(done, total);
    format!("{}{}", "█".repeat(fill), "░".repeat(BAR_WIDTH - fill))
}

/// Paint the frame as a fixed-shape block of 5 lines separated by `\n`.
/// The caller repositions the cursor and clears; this module only draws.
pub fn render(f: &Frame) -> String {
    let (open, close) = match f.accent {
        Some(code) => (format!("\x1b[{code}m"), "\x1b[0m"),
        None => (String::new(), ""),
    };
    let phase_label = match f.phase {
        Phase::Focus => "focus · the cell",
        Phase::Rest => "rest · the yard is open",
    };

    let mut out = String::from(" yard · ");
    out.push_str(f.task);
    out.push_str(" · ");
    out.push_str(f.plan);
    out.push_str(" — ");
    out.push_str(phase_label);
    out.push('\n');

    let left = f.total_secs.saturating_sub(f.done_secs);
    let pct = (f.done_secs * 100)
        .checked_div(f.total_secs)
        .unwrap_or(100)
        .min(100);
    out.push_str(&open);
    out.push_str(&mmss(left));
    out.push_str(close);
    out.push_str("  ");
    out.push_str(&open);
    out.push_str(&bar(f.done_secs, f.total_secs));
    out.push_str(close);
    out.push_str(&format!(" {pct:>3}%\n"));

    if f.phase == Phase::Focus {
        if f.allowance_total_secs > 0 {
            let used = f
                .allowance_total_secs
                .saturating_sub(f.allowance_left_secs);
            out.push_str(" allowance ");
            out.push_str(&bar(used, f.allowance_total_secs));
            out.push_str(&format!("  {} saved idle", mmss(f.allowance_left_secs)));
        } else {
            out.push_str(" allowance off (no reading top-up)");
        }
        out.push('\n');
    } else {
        out.push_str(" Focus credit is already banked — this is free time.\n");
    }

    if f.status.is_empty() {
        out.push('\n');
    } else {
        out.push_str(&open);
        out.push_str(&f.status);
        out.push_str(close);
        out.push('\n');
    }

    out.push_str(match f.phase {
        Phase::Focus => " Ctrl+C bails (nothing is banked) · move to prove you're alive",
        Phase::Rest => " Ctrl+C back in early · the countdown is not a chore, it's your reward",
    });
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(status: &str, accent: Option<&'static str>) -> Frame<'static> {
        Frame::focus(
            "Rust ownership",
            "pomodoro",
            300,
            1500,
            225,
            150,
            status.to_string(),
            accent,
        )
    }

    #[test]
    fn mmss_formats_two_digits_padded() {
        assert_eq!(mmss(0), "00:00");
        assert_eq!(mmss(65), "01:05");
        assert_eq!(mmss(5400), "90:00"); // deep-plan max, deliberate: hours never split
    }

    #[test]
    fn bar_fills_round_and_clamp() {
        assert_eq!(bar_fill(0, 100), 0);
        assert_eq!(bar_fill(50, 100), BAR_WIDTH / 2);
        assert_eq!(bar_fill(100, 100), BAR_WIDTH);
        assert_eq!(bar_fill(150, 100), BAR_WIDTH); // never overpaints
    }

    #[test]
    fn focus_frame_has_all_regions_and_no_escapes_when_plain() {
        let f = frame("", None);
        let r = render(&f);
        assert!(!r.contains('\x1b'));
        assert!(r.starts_with(" yard · Rust ownership · pomodoro — focus"));
        assert!(r.contains("20:00")); // 1500-300 left
        assert!(r.contains("02:30"), "allowance line wrong: {r}"); // 150s left
        assert!(r.contains("Ctrl+C bails"));
        assert_eq!(r.lines().count(), 5); // fixed shape: cursor math is stable
    }

    #[test]
    fn rest_frame_hides_the_allowance_and_marks_the_phase() {
        let f = Frame::rest("Rust ownership", "pomodoro", 30, 300, "yard time!".into(), None);
        let r = render(&f);
        assert!(r.contains("the yard is open"));
        assert!(r.contains("04:30")); // 300 - 30 seconds left
        assert!(!r.contains("allowance"), "rest shouldn't show allowance: {r}");
        assert!(r.contains("already banked"));
    }

    #[test]
    fn colored_frame_wraps_the_line_in_that_colour() {
        let f = frame("PAUSED", Some("31"));
        let r = render(&f);
        assert!(r.contains("\x1b[31m"));
        assert!(r.contains("\x1b[0m"));
        assert!(r.contains("PAUSED"));
    }

    #[test]
    fn completed_phase_shows_full_bar_and_zero_left() {
        let f = frame("", None);
        let mut f = f;
        f.done_secs = f.total_secs;
        let r = render(&f);
        assert!(r.contains("00:00"));
        assert!(r.contains("100%"));
        assert!(r.contains(&"█".repeat(BAR_WIDTH)));
    }

    #[test]
    fn zero_total_is_rendered_not_panic() {
        let mut f = frame("", None);
        f.total_secs = 0;
        f.done_secs = 0;
        let r = render(&f);
        assert!(r.contains("100%")); // nothing left to do
    }
}

//! v0.4 — the session view, drawn exactly as the user mocked it up: a boxed
//! frame with the task name, plan, big countdown, progress bar, focused
//! elapsed, PROJECTED yard reward (mirrors the ledger's math exactly), idle
//! allowance, an honest audio line (the feature ships later — we never fake
//! it), a rotating memory tip, and the live key hints.
//!
//! This module is a pure renderer: no cursor moves, no screen clearing, no
//! I/O — it just formats text. All terminal control codes live in main.rs,
//! which decides whether to use fancy output at all.

/// Interior width of the box (between the two │ borders), in cells.
pub const INNER: usize = 52;
/// Width of the central progress bar, in cells.
pub const BAR_W: usize = 24;

/// Memory tips that rotate through the quote slot every ~60 s.
pub const TIPS: [&str; 6] = [
    "Understand it. Recall it. Explain it.",
    "Retrieval beats rereading — close the book, sketch it.",
    "You remember what you can teach. Teach it out loud.",
    "One hard problem beats ten easy ones.",
    "Wandered off-topic? Park it on paper, back to task.",
    "Slow-but-right recall beats fast-but-fake recall.",
];

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
    pub plan_label: &'a str,
    pub phase: Phase,
    /// Counted credit this phase (focus seconds / rest seconds elapsed).
    pub done_secs: u64,
    /// Phase length in seconds.
    pub total_secs: u64,
    /// rest/focus ratio of the plan, for the yard projection (0.0 in rest).
    pub ratio: f64,
    /// Current clean streak, for the multiplier (0 in rest).
    pub clean_streak: u64,
    /// Focused credit already banked today (rest view shows it).
    pub banked_focus_secs: u64,
    /// Reading allowance granted for this session (focus only).
    pub allowance_total_secs: u64,
    /// Unspent idle seconds inside the allowance (focus only).
    pub allowance_left_secs: u64,
    pub tip: &'a str,
    /// Real keypresses available? (false on Windows / non-tty stdin)
    pub keys: bool,
    pub status: String,
    /// ANSI colour-code number (e.g. Some("32") green) or None for plain.
    pub accent: Option<&'static str>,
}

impl<'a> Frame<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn focus(
        task: &'a str,
        plan_label: &'a str,
        done_secs: u64,
        total_secs: u64,
        ratio: f64,
        clean_streak: u64,
        allowance_total_secs: u64,
        allowance_left_secs: u64,
        tip: &'a str,
        keys: bool,
        status: String,
        accent: Option<&'static str>,
    ) -> Self {
        Self {
            task,
            plan_label,
            phase: Phase::Focus,
            done_secs,
            total_secs,
            ratio,
            clean_streak,
            banked_focus_secs: 0,
            allowance_total_secs,
            allowance_left_secs,
            tip,
            keys,
            status,
            accent,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn rest(
        task: &'a str,
        plan_label: &'a str,
        done_secs: u64,
        total_secs: u64,
        banked_focus_secs: u64,
        keys: bool,
        status: String,
        accent: Option<&'static str>,
    ) -> Self {
        Self {
            task,
            plan_label,
            phase: Phase::Rest,
            done_secs,
            total_secs,
            ratio: 0.0,
            clean_streak: 0,
            banked_focus_secs,
            allowance_total_secs: 0,
            allowance_left_secs: 0,
            tip: "Credit is banked — this break is freely yours.",
            keys,
            status,
            accent,
        }
    }
}

/// Yard projection — identical math to `state::record_session` so the frame
/// never overpromises: round(focused_min × ratio × (1 + 0.10×streak)).
pub fn projected_yard_min(done_secs: u64, ratio: f64, clean_streak: u64) -> u64 {
    let m = (done_secs / 60) as f64;
    (m * ratio * (1.0 + 0.10 * clean_streak as f64)).round() as u64
}

/// `125` -> "02:05".
pub fn mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Filled cells for a progress bar of [done..=total], rounded to halves.
fn bar_fill(done: u64, total: u64) -> usize {
    if total == 0 {
        return BAR_W;
    }
    let fill = ((done * 2 * BAR_W as u64 + total) / (total * 2)) as usize;
    fill.min(BAR_W)
}

fn bar(done: u64, total: u64) -> String {
    let fill = bar_fill(done, total);
    format!("{}{}", "█".repeat(fill), "░".repeat(BAR_W - fill))
}

/// Truncate to `w` chars then pad with spaces to exactly `w` visible cells
/// (box chars and ASCII are all single-width here).
fn fit(s: &str, w: usize) -> String {
    let mut out: String = s.chars().take(w).collect();
    for _ in out.chars().count()..w {
        out.push(' ');
    }
    out
}

/// A `│`-wrapped row: content sits after `l` spaces and exactly fills the
/// remaining interior, so the border can never be pushed out of shape.
fn row(l: usize, s: &str) -> String {
    format!("│{}{}│\n", " ".repeat(l), fit(s, INNER - l))
}

/// Same as [`row`], with the content painted in `accent` — codes are
/// inserted after padding, so the visible width is untouched.
fn colored_row(l: usize, s: &str, accent: &str) -> String {
    format!("│\x1b[{accent}m{}{}│\n", " ".repeat(l), fit(s, INNER - l))
}

fn center(s: &str) -> String {
    let n: String = s.chars().take(INNER).collect();
    let l = (INNER - n.chars().count()) / 2;
    format!("{}{}", " ".repeat(l), s.chars().take(INNER - l).collect::<String>())
}

/// Top border line, e.g. `┌─ YARD / CELL ──────…┐`.
fn bordered(title: &str) -> String {
    let t = title.chars().count();
    let fill = INNER.saturating_sub(t + 3);
    format!("┌─ {title} {}┐\n", "─".repeat(fill))
}

/// Paint the frame as a fixed-shape block of lines separated by `\n`.
/// The caller repositions the cursor and clears; this module only draws.
pub fn render(f: &Frame) -> String {
    let mut out = String::new();
    match f.phase {
        Phase::Focus => out.push_str(&bordered("YARD / CELL")),
        Phase::Rest => out.push_str(&bordered("YARD / OPEN")),
    }

    let blank = row(0, "");
    out.push_str(&blank);
    out.push_str(&row(0, &center(f.task)));
    let plan_line = match f.phase {
        Phase::Focus => format!("{} · {} focused", f.plan_label, mmss(f.total_secs)),
        Phase::Rest => format!("{} · rest", f.plan_label),
    };
    out.push_str(&row(0, &center(&plan_line)));
    out.push_str(&blank);

    let left = f.total_secs.saturating_sub(f.done_secs);
    let time = mmss(left);
    let barline = bar(f.done_secs, f.total_secs);

    if let Some(accent) = f.accent {
        out.push_str(&colored_row(0, &center(&time), accent));
        out.push_str(&colored_row(0, &center(&barline), accent));
    } else {
        out.push_str(&row(0, &center(&time)));
        out.push_str(&row(0, &center(&barline)));
    }
    out.push_str(&blank);

    match f.phase {
        Phase::Focus => {
            let proj = projected_yard_min(f.done_secs, f.ratio, f.clean_streak);
            let line1 = format!(
                "Focused {}        Yard projected {}m",
                mmss(f.done_secs),
                proj
            );
            let line2 = if f.allowance_total_secs > 0 {
                format!(
                    "Idle allowance {}  Audio: off (ships v0.4+)",
                    mmss(f.allowance_left_secs)
                )
            } else {
                "Idle allowance off Audio: off (ships v0.4+)".to_string()
            };
            out.push_str(&row(3, &line1));
            out.push_str(&row(3, &line2));
        }
        Phase::Rest => {
            let line = format!("Focus banked today: {}", mmss(f.banked_focus_secs));
            out.push_str(&row(3, &line));
            out.push_str(&row(3, "Stand up. Look 20 ft away. Breathe."));
        }
    }
    out.push_str(&blank);

    // The quote slot shows a live status when the gate is speaking, else the
    // rotating memory tip / rest reassurance.
    let quote = if !f.status.is_empty() {
        f.status.clone()
    } else {
        f.tip.to_string()
    };
    let quoted = format!("\"{quote}\"");
    match (f.accent, f.status.is_empty()) {
        (Some(accent), false) => out.push_str(&colored_row(3, &quoted, accent)),
        _ => out.push_str(&row(3, &quoted)),
    }
    out.push_str(&blank);

    let keys_hint = if f.keys {
        match f.phase {
            Phase::Focus => "[P] Pause  [R] Reading +1m  [X] Exit (banks)".to_string(),
            Phase::Rest => "[X] Back early (credit already banked)".to_string(),
        }
    } else {
        "No keys here — Ctrl+C: bail (nothing banks)".to_string()
    };
    out.push_str(&row(3, &keys_hint));
    out.push_str(&row(0, ""));
    out.push_str(&format!(
        "└{}┘\n",
        "─".repeat(INNER + 2)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(status: &str, accent: Option<&'static str>, keys: bool) -> Frame<'static> {
        Frame::focus(
            "Generative AI",
            "Pomodoro",
            918, // 15:18 done of a 25 min plan
            1500,
            0.5,
            2, // streak 2 → ×1.2
            225,
            150,
            TIPS[0],
            keys,
            status.to_string(),
            accent,
        )
    }

    /// Strip ANSI colour codes so width measurements see visible cells only.
    fn plain(r: &str) -> String {
        let mut out = String::new();
        let mut in_esc = false;
        for c in r.chars() {
            if in_esc {
                if c == 'm' {
                    in_esc = false;
                }
            } else if c == '\x1b' {
                in_esc = true;
            } else {
                out.push(c);
            }
        }
        out
    }

    fn inner_width_ok(r: &str) -> bool {
        let r = &plain(r);
        r.lines()
            .filter(|l| l.starts_with('│') && l.ends_with('│'))
            .all(|l| l.chars().count() == INNER + 2)
    }

    #[test]
    fn box_rows_never_let_content_break_the_border() {
        for keys in [true, false] {
            let r = render(&f("", None, keys));
            assert!(inner_width_ok(&r), "width broke:\n{r}");
        }
    }

    #[test]
    fn mockup_regions_are_all_present() {
        let r = render(&f("", None, true));
        assert!(r.starts_with("┌─ YARD / CELL ─"));
        assert!(r.contains("Generative AI"));
        assert!(r.contains("Pomodoro · 25:00 focused"));
        assert!(r.contains("09:42")); // 1500-918s remaining
        assert!(r.contains("Focused 15:18"));
        assert!(r.contains("Yard projected 9m")); // 15 min × 0.5 × 1.2
        assert!(r.contains("Idle allowance 02:30"));
        assert!(r.contains("Audio: off (ships v0.4+)"));
        assert!(r.contains("Understand it. Recall it."));
        assert!(r.contains("[P] Pause"));
        assert!(r.contains("[R] Reading +1m"));
        assert!(r.contains("[X] Exit (banks)"));
        assert!(r.ends_with('\n'));
        assert!(inner_width_ok(&r), "width broke:\n{r}");
    }

    #[test]
    fn status_overrides_the_tip_slot_and_gets_the_accent() {
        let r = render(&f("PAUSED BY YOU", Some("33"), true));
        assert!(r.contains("\"PAUSED BY YOU\""));
        assert!(r.contains("\x1b[33m"));
        assert!(!r.contains("Understand it. Recall it."));
    }

    #[test]
    fn colored_rows_keep_the_visible_width() {
        let r = render(&f("counting", Some("32"), true));
        assert!(inner_width_ok(&r), "colour codes broke width:\n{r}");
    }

    #[test]
    fn no_keys_is_honest_about_it() {
        let r = render(&f("", None, false));
        assert!(!r.contains("[P] Pause"));
        assert!(r.contains("Ctrl+C: bail (nothing banks)"));
    }

    #[test]
    fn projection_matches_the_ledger_formula() {
        // state.rs: round(focused_min × ratio × (1 + 0.10 × streak))
        assert_eq!(projected_yard_min(3600, 0.5, 0), 30);
        assert_eq!(projected_yard_min(3600, 0.5, 1), 33);
        assert_eq!(projected_yard_min(1800, 0.5, 2), 18); // 30 min × 0.5 × 1.2
        assert_eq!(projected_yard_min(0, 0.5, 5), 0);
    }

    #[test]
    fn long_status_truncates_never_overflows() {
        let mut f = f("", None, true);
        f.status = "STILL THERE? move to keep counting — the timer stops for good in 60s, plus extra words to force truncation".to_string();
        let r = render(&f);
        assert!(inner_width_ok(&r), "long status overflowed:\n{r}");
    }

    #[test]
    fn rest_frame_matches_its_own_mockup() {
        let r = render(&Frame::rest(
            "Generative AI",
            "Pomodoro",
            30,
            300,
            900,
            true,
            String::new(),
            Some("36"),
        ));
        assert!(r.starts_with("┌─ YARD / OPEN ─"));
        assert!(r.contains("Focus banked today: 15:00"));
        assert!(r.contains("[X] Back early"));
        assert!(!r.contains("allowance"));
        assert!(inner_width_ok(&r));
    }

    #[test]
    fn completed_shows_full_bar_and_zero_left() {
        let mut f = f("", None, true);
        f.done_secs = f.total_secs;
        let r = render(&f);
        assert!(r.contains("00:00"));
        assert!(r.contains(&"█".repeat(BAR_W)));
    }

    #[test]
    fn zero_total_renders_without_panic() {
        let mut f = f("", None, true);
        f.total_secs = 0;
        f.done_secs = 0;
        let r = render(&f);
        // The mockup bar carries no percentage: full bar + 00:00 left is the
        // way "done" reads.
        assert!(r.contains("00:00"));
        assert!(r.contains(&"█".repeat(BAR_W)));
        assert!(inner_width_ok(&r));
    }
}

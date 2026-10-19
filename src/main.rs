//! Yard v0.5 — core loop, REAL idle detection, the full session frame with
//! raw-mode keys ([P] pause, [R] reading, [X] emergency exit that banks
//! partial credit as a bail), AND the lockdown watcher (plan doc section 6):
//! process + window watching that calls out offenders — DRY RUN by default,
//! `--lock enforce` arms the kill. Audio still doesn't play — one slice each.

mod idle;
mod keys;
mod lock;
mod state;
mod ui;

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use idle::Action;
use state::{Day, Session, Task, YardState};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    name = "yard",
    version,
    about = "Focus to earn yard time. Misbehave and the yard shrinks."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
    /// State file location (default ~/.local/share/yard/state.json)
    #[arg(long, global = true)]
    state: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add a task with a daily goal (e.g. `yard add "Rust ownership" --daily 2h`).
    Add {
        name: String,
        #[arg(long = "daily", value_parser = parse_duration_min)]
        daily: u64,
    },
    /// Start a focus session. Idle > reading allowance pauses the timer.
    /// `--lock dry` (default when the flag is set) only FLAGS offenders;
    /// `--lock enforce` additionally closes them. Never locks you out.
    Start {
        /// Task name (must exist; see `yard add`).
        task: String,
        /// Plan preset: quick (1 min, for testing), pomodoro 25/5, hour 60/15, deep 90/20.
        #[arg(long, default_value = "pomodoro")]
        plan: String,
        /// Lockdown mode: "dry" (flag only) or "enforce" (flag + close).
        #[arg(long = "lock")]
        lock: Option<String>,
    },
    /// Tell Yard you're reading for N minutes: extra idle allowance next session.
    Read { minutes: u64 },
    /// Spend earned yard time: `yard spend 30`.
    Spend { minutes: u64 },
    /// Today's report: focus vs goal, yard earned vs spent, streak, strikes.
    Report {},
}

/// "2h", "90m", "1h30" -> minutes. Rejects 0 and unparsable input.
fn parse_duration_min(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    let (hours, mins) = if let Some((h, m)) = s.split_once('h') {
        (h.trim(), m.trim().trim_end_matches('m').trim())
    } else {
        ("0", s.trim().trim_end_matches('m').trim())
    };
    let hours: u64 =
        if hours.is_empty() { 0 } else { hours.parse().map_err(|_| format!("bad duration: {s}"))? };
    let mins: u64 =
        if mins.is_empty() { 0 } else { mins.parse().map_err(|_| format!("bad duration: {s}"))? };
    let total = hours * 60 + mins;
    if total == 0 {
        return Err(format!("duration must be more than zero: {s}"));
    }
    Ok(total)
}

struct Plan {
    name: &'static str,
    focus_min: u64,
    rest_min: u64,
}

const PLANS: [Plan; 4] = [
    Plan { name: "quick", focus_min: 1, rest_min: 1 },
    Plan { name: "pomodoro", focus_min: 25, rest_min: 5 },
    Plan { name: "hour", focus_min: 60, rest_min: 15 },
    Plan { name: "deep", focus_min: 90, rest_min: 20 },
];

/// Friendly plan name for the frame (plan ids stay stable for `--plan`).
fn plan_label(id: &str) -> &'static str {
    match id {
        "quick" => "Warmup",
        "hour" => "Hour",
        "deep" => "Deep Work",
        _ => "Pomodoro",
    }
}

/// Free idle seconds per session: 15% of its length (starter value, tune me).
const IDLE_ALLOWANCE_PCT: u64 = 15;

/// Lockdown cadence: one scan per this many ticks (1s each), tune me.
const LOCK_SCAN_EVERY: u64 = 5;
const LOCK_SHOW_TICKS: u16 = 12; // status lingers 12s after a catch

fn state_path(cli: &Cli) -> PathBuf {
    cli.state.clone().unwrap_or_else(|| {
        let mut p = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        p.push(".local/share/yard");
        p.push("state.json");
        p
    })
}

fn main() {
    let cli = Cli::parse();
    let path = state_path(&cli);
    if let Err(e) = run(&cli, &path) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli, path: &Path) -> Result<()> {
    let mut st = YardState::load(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let today = state::today();
    if st.day.date != today {
        // New day: earnings reset, clean streak survives (consistency is the point).
        st.day = Day { date: today, ..Default::default() };
    }

    match &cli.command {
        Cmd::Add { name, daily } => {
            let name = name.trim().to_string();
            ensure!(!name.is_empty(), "task name cannot be empty");
            ensure!(st.task_mut(&name).is_none(), "task {name:?} already exists");
            st.tasks.push(Task {
                name: name.clone(),
                daily_goal_min: *daily,
                done_min: 0,
            });
            println!("Task added: {name} ({daily} min/day)");
        }
        Cmd::Start { task, plan, lock } => {
            if let Some(mode) = lock {
                ensure!(
                    mode == "dry" || mode == "enforce",
                    "unknown lock mode {mode:?} — try dry or enforce"
                );
            }
            run_session(&mut st, path, task, plan, lock.as_deref())?
        }
        Cmd::Read { minutes } => {
            ensure!(*minutes > 0, "read for how many minutes?");
            st.day.read_allowance_min += minutes; // consumed by the next session
            println!(
                "Noted: reading {minutes} min. That tops up your next session's idle allowance — reading is studying."
            );
        }
        Cmd::Spend { minutes } => {
            ensure!(*minutes > 0, "spend how many minutes?");
            let avail = st.day.yard_earned_min.saturating_sub(st.day.yard_spent_min);
            ensure!(
                *minutes <= avail,
                "only {avail} min available (earned {} − spent {}); the yard is strict but honest",
                st.day.yard_earned_min,
                st.day.yard_spent_min
            );
            st.day.yard_spent_min += minutes;
            println!("Spent {minutes} min of yard time. Enjoy — you earned it.");
        }
        Cmd::Report {} => print_report(&st),
    }
    st.save(path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn run_session(
    st: &mut YardState,
    path: &Path,
    task_name: &str,
    plan_name: &str,
    lock_mode: Option<&str>,
) -> Result<()> {
    ensure!(
        st.task_mut(task_name).is_some(),
        "unknown task {task_name:?} — `yard add` it first"
    );
    let plan = PLANS
        .iter()
        .find(|p| p.name == plan_name)
        .with_context(|| format!("unknown plan {plan_name:?} (try: quick, pomodoro, hour, deep)"))?;
    let ratio = plan.rest_min as f64 / plan.focus_min as f64;

    // Reading allowance topped up via `yard read` is one-shot per session.
    let allowance_secs = plan.focus_min * 60 * IDLE_ALLOWANCE_PCT / 100
        + st.day.read_allowance_min * 60;

    // Fancy frames only on a real terminal; piped/scripted runs get plain text.
    let tty = std::io::stdout().is_terminal();
    // Show how verified you are up front.
    if !tty {
        let sensor_known = idle::idle_seconds().is_some();
        if !sensor_known {
            println!("v0.2 note: no idle sensor on this platform — falling back to the v0.1 honesty check.");
        }
        println!(
            "Plan {}: {} min focus, {} min rest. Task: {}",
            plan.name, plan.focus_min, plan.rest_min, task_name
        );
        println!("Idle beyond the reading allowance pauses the timer. Ctrl+C to bail.");
    }

    if tty {
        // Hide the cursor while the frame paints, and guarantee the restore
        // even when the process dies via Ctrl+C, panics, or errors.
        print!("\x1b[?25l\x1b[2J");
        std::io::stdout().flush().ok();
        ctrlc::set_handler(|| {
            print!("\x1b[?25h");
            keys::restore(); // raw mode must not outlive the process
            std::io::stdout().flush().ok();
            std::process::exit(130);
        })
        .ok();
    }

    let target_secs = plan.focus_min * 60;
    let mut gate = idle::IdleGate::new(allowance_secs);
    // v0.2 admits only VERIFIED focus: second-ticks with real key/mouse input.
    let mut focused_secs: u64 = 0;
    let mut idle_paused_secs: u64 = 0;
    let mut first_frame = true;
    let mut manual_pause = false;
    let mut bail_now = false; // [X] emergency exit
    let mut tick: u64 = 0; // drives the tip rotation
    let mut notice = String::new(); // transient status (e.g. after [R])
    let mut notice_ticks = 0u8;
    let mut lock_show_ticks: u16 = 0; // lockdown status lingers a few ticks
    let clean_streak = st.day.clean_streak; // multiplier context for projection
    let label = plan_label(plan.name);
    // Raw keys are a real-terminal privilege; without them the honest prose
    // hints stay up and Ctrl+C remains the only way out.
    let raw_keys = if tty { keys::RawKeys::enable() } else { None };
    let keys_available = raw_keys.is_some();

    // The cell's watcher (v0.5): dry run unless armed. Absent Hyprland/hyprctl
    // means processes-only; said here up front, never discovered later.
    let mut watcher = lock_mode.map(|m| lock::Watcher::new(m == "enforce"));
    if let Some(mode) = lock_mode {
        println!(
            "Lockdown ({mode}): watching processes{}, every {}s. {}",
            if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
                " + Hyprland window titles"
            } else {
                " — no Hyprland session, so window titles are off"
            },
            LOCK_SCAN_EVERY,
            if mode == "enforce" {
                "offenders get CLOSED. `--lock dry` to just watch."
            } else {
                "dry run: offenders are flagged, nothing is closed."
            }
        );
    }

    while focused_secs < target_secs {
        tick += 1;

        // Keys first: poll every second (raw mode, nonblocking).
        if let Some(rk) = &raw_keys {
            match rk.try_read() {
                Some(c) if c == keys::KEY_PAUSE => {
                    if manual_pause {
                        // Resuming a deliberate break forgives the gate so
                        // the idle allowance isn't burnt by the break.
                        gate.forgive_idle();
                        manual_pause = false;
                    } else {
                        manual_pause = true;
                    }
                }
                Some(c) if c == keys::KEY_READ => {
                    gate.grant_allowance(60);
                    notice = "Reading +1:00 — free idle extended (reading is studying)".to_string();
                    notice_ticks = 4;
                }
                Some(c) if c == keys::KEY_EXIT => {
                    bail_now = true;
                    break;
                }
                _ => {}
            }
        }

        // Lockdown (v0.5): scan the cell every few seconds, honestly.
        let mut lock_status = String::new();
        if let Some(w) = watcher.as_mut() {
            if tick.is_multiple_of(LOCK_SCAN_EVERY) {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let scan = w.scan(now);
                st.day.strikes += scan.strikes_added;
                if !scan.offenders.is_empty() {
                    let labels: Vec<String> = scan.offenders.iter().map(|o| o.label()).collect();
                    lock_status = if w.enforce && !scan.closed.is_empty() {
                        format!("LOCK: closed {} — strike logged", scan.closed.join(", "))
                    } else {
                        format!(
                            "LOCK dry-run: {} — flagged, nothing closed (arm: --lock enforce)",
                            labels.join(", ")
                        )
                    };
                    if !tty {
                        println!("\r[lock] {lock_status}");
                    }
                    lock_show_ticks = LOCK_SHOW_TICKS;
                }
            }
        }
        lock_show_ticks = lock_show_ticks.saturating_sub(1);

        // A deliberate pause [P] stops the whole clock: the gate is frozen
        // (nothing accrues, nothing burns) and resumes forgiven.
        let action = if manual_pause {
            idle::Action::Paused
        } else {
            gate.tick(idle::idle_seconds())
        };
        match action {
            Action::Count => {
                focused_secs += 1;
            }
            Action::Allowance => { /* free idle: no credit, no nag */ }
            Action::Ask { .. } => { /* the frame says it all */ }
            Action::Paused => {
                idle_paused_secs += 1;
            }
            Action::Resumed => {
                if idle_paused_secs > 0 {
                    println!("\nResumed. Welcome back.");
                    idle_paused_secs = 0;
                }
                // Re-entered after pause/ask: credit the tick like Count does
                // (the gate already reset the allowance).
                focused_secs += 1;
            }
        }

        let (status, accent) = if manual_pause {
            (
                "PAUSED BY YOU — the clock is stopped, no credit accrues. [P] to resume".to_string(),
                Some("33"),
            )
        } else if lock_show_ticks > 0 && !lock_status.is_empty() {
            (lock_status.clone(), Some("31"))
        } else if notice_ticks > 0 {
            (notice.clone(), Some("36"))
        } else {
            match action {
                Action::Count => (String::new(), Some("32")),
                Action::Allowance => (
                    format!(
                        "free reading time — {} of idle still absorbed",
                        ui::mmss(gate.allowance_left())
                    ),
                    Some("33"),
                ),
                Action::Ask { ticks_left } => (
                    format!(
                        "STILL THERE? move to keep counting — the timer stops for good in {ticks_left}s"
                    ),
                    Some("33"),
                ),
                Action::Paused => (
                    "PAUSED — no credit accruing. Move to resume".to_string(),
                    Some("31"),
                ),
                Action::Resumed => ("welcome back — counting again".to_string(), Some("32")),
            }
        };
        notice_ticks = notice_ticks.saturating_sub(1);

        if tty {
            let f = ui::Frame::focus(
                task_name,
                label,
                focused_secs,
                target_secs,
                ratio,
                clean_streak,
                allowance_secs,
                gate.allowance_left(),
                ui::TIPS[((tick / 60) as usize) % ui::TIPS.len()],
                keys_available,
                status,
                accent,
            );
            draw_frame(&f, first_frame);
            first_frame = false;
        } else {
            let left = target_secs.saturating_sub(focused_secs);
            print!("\r  {:>2}:{:02} focused, in the cell", left / 60, left % 60);
            std::io::stdout().flush().ok();
        }
        std::thread::sleep(idle::sleep_secs());
    }

    // Show the cursor again before any text prompt/read.
    if tty {
        print!("\x1b[?25h");
        std::io::stdout().flush().ok();
    }
    keys::restore(); // Drop would handle it; restore early is idempotent
    println!("\nThe cell opens.");
    let bailed = bail_now; // [X] banks partial credit AS A BAIL (streak resets)
    let sensor_known = idle::idle_seconds().is_some();
    if !sensor_known && !bailed {
        // Honest fallback — no sensor platform; asked on a terminal OR piped.
        // (A bail already carries its own penalty; double-asking is not honest.)
        print!("Focused the whole time? [Y/n]: ");
        std::io::stdout().flush().ok();
        let mut ans = String::new();
        std::io::stdin().lock().read_line(&mut ans).ok();
        if matches!(ans.trim().to_uppercase().as_str(), "N") {
            focused_secs /= 2;
        }
    }

    let focused_min = focused_secs / 60;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    st.record_session(
        Session {
            task: task_name.to_string(),
            started_at: ts,
            focused_min,
            idle_min: idle_paused_secs / 60,
            bailed,
        },
        ratio,
    );
    if let Some(t) = st.task_mut(task_name) {
        t.done_min += focused_min;
    }
    st.day.read_allowance_min = 0; // one-shot top-up consumed
    // The yard-open countdown that follows is uninterruptible-by-sleep only;
    // a Ctrl+C here must not lose the ledger, so persist before resting.
    st.save(path).with_context(|| format!("writing {}", path.display()))?;

    println!(
        "Focused {focused_min} min (paused {p} min). Yard earned today: {e} min (clean streak: {}).",
        st.day.clean_streak,
        p = idle_paused_secs / 60,
        e = st.day.yard_earned_min
    );
    if bailed {
        println!("Emergency exit: this banks as a bail — the clean streak reset, but the work wasn't wasted.");
    }
    if st.day.strikes > 0 {
        println!(
            "Strikes today: {} — the cell caught apps drifting; they cut the multiplier, see the report.",
            st.day.strikes
        );
    }
    if let Some(t) = st.tasks.iter().find(|t| t.name == task_name) {
        println!("Task progress: {}/{} min today", t.done_min, t.daily_goal_min);
    }

    // The rest phase (plan doc section 4): the yard is open. This is your
    // reward break; the focus credit is already banked and safe. A bail
    // earns no rest — that's what makes the streak worth keeping.
    let rest_secs = plan.rest_min * 60;
    if rest_secs > 0 && !bailed {
        if tty {
            println!("\n  Yard time — stretch, look far away, refill the cup.\n");
            std::io::stdout().flush().ok();
            print!("\x1b[?25l");
            std::io::stdout().flush().ok();
            let raw_rest = keys::RawKeys::enable();
            let banked = st
                .tasks
                .iter()
                .find(|t| t.name == task_name)
                .map_or(0, |t| t.done_min * 60);
            first_frame = true;
            let mut rest_done = 0u64;
            let mut back_early = false;
            while rest_done < rest_secs {
                // Rest view: countdown + bar + already-banked reassurance.
                if let Some(rk) = &raw_rest {
                    if matches!(rk.try_read(), Some(c) if c == keys::KEY_EXIT) {
                        back_early = true;
                        break;
                    }
                }
                let f = ui::Frame::rest(
                    task_name,
                    label,
                    rest_done,
                    rest_secs,
                    banked,
                    raw_rest.is_some(),
                    String::new(),
                    Some("36"),
                );
                draw_frame(&f, first_frame);
                first_frame = false;
                rest_done += 1;
                // One-second fine-grained countdown keeps the bar honest.
                std::thread::sleep(idle::sleep_secs());
            }
            if back_early {
                println!("\nThe yard closed early — back in the cell when you are. `yard start <task>`.\n");
            } else {
                println!("\nThe yard closes. Ready for the next cell? `yard start <task>`.\n");
            }
            print!("\x1b[?25h");
            keys::restore();
            std::io::stdout().flush().ok();
        } else if !bailed {
            println!(
                "Rest: {} min — the yard is open. Come back for the next cell: `yard start <task>`.",
                plan.rest_min
            );
        }
    }
    Ok(())
}

/// Repaint the terminal from the top with the frame (and clean up below it).
/// `full_clear` wipes the whole screen once — the rest/focus transition wants
/// a clean slate; every later tick overwrites only its own frame region.
fn draw_frame(f: &ui::Frame, full_clear: bool) {
    let text = if full_clear {
        "\x1b[2J\x1b[H".to_string() + &ui::render(f)
    } else {
        "\x1b[H".to_string() + &ui::render(f)
    };
    print!("{text}\x1b[J");
    std::io::stdout().flush().ok();
}

fn print_report(st: &YardState) {
    println!("Today ({}):", st.day.date);
    println!(
        "  Yard earned: {} min | spent: {} min | available: {} min",
        st.day.yard_earned_min,
        st.day.yard_spent_min,
        st.day.yard_earned_min - st.day.yard_spent_min
    );
    println!(
        "  Clean streak: {} {}",
        st.day.clean_streak,
        if st.day.clean_streak == 1 { "session" } else { "sessions" }
    );
    if st.day.strikes > 0 {
        println!(
            "  Strikes: {} — blocked-app catches this day (each cut the multiplier when scored)",
            st.day.strikes
        );
    }
    if st.day.read_allowance_min > 0 {
        println!("  Queued reading allowance: {} min (next session)", st.day.read_allowance_min);
    }
    if st.tasks.is_empty() {
        println!("  No tasks yet — `yard add \"<name>\" --daily 2h`");
    }
    for t in &st.tasks {
        let pct = t
            .done_min
            .checked_mul(100)
            .and_then(|n| n.checked_div(t.daily_goal_min))
            .unwrap_or(0) as usize;
        let filled = pct.min(100) / 5;
        let bar = "#".repeat(filled) + &".".repeat(20 - filled);
        println!("  {bar} {:>3}% {}/{} min — {}", pct, t.done_min, t.daily_goal_min, t.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser_accepts_the_documented_forms() {
        assert_eq!(parse_duration_min("2h").unwrap(), 120);
        assert_eq!(parse_duration_min("90m").unwrap(), 90);
        assert_eq!(parse_duration_min("1h30").unwrap(), 90);
        assert_eq!(parse_duration_min("2H").unwrap(), 120);
        assert_eq!(parse_duration_min("45M").unwrap(), 45);
    }

    #[test]
    fn duration_parser_rejects_garbage_and_silent_zero() {
        assert!(parse_duration_min("abc").is_err());
        assert!(parse_duration_min("1.5h").is_err());
        // silent-zero is the bug class these tests exist to prevent:
        assert!(parse_duration_min("h").is_err());
        assert!(parse_duration_min("0m").is_err());
        assert!(parse_duration_min("0h0m").is_err());
        assert!(parse_duration_min("").is_err());
    }

    // --- the strike penalty and its floor (v0.5) -----------------------------

    fn session(focused: u64, bailed: bool) -> Session {
        Session {
            task: "t".into(),
            started_at: 0,
            focused_min: focused,
            idle_min: 0,
            bailed,
        }
    }

    #[test]
    fn strikes_cut_the_multiplier_but_never_below_the_floor() {
        let mut st = YardState::default();
        st.day.strikes = 1; // ×(1 + 0 − 0.15) = ×0.85
        st.record_session(session(60, false), 0.5);
        assert_eq!(st.day.yard_earned_min, 26); // round(60 × 0.5 × 0.85) = round(25.5) = 26

        st = YardState::default();
        st.day.strikes = 100; // penalty clamps at 0.9 → ×0.1 floor
        st.record_session(session(60, false), 0.5);
        assert_eq!(st.day.yard_earned_min, 3); // 60×0.5×0.1 = 3
    }

    #[test]
    fn a_strike_and_a_streak_partially_cancel_but_streak_is_bounded() {
        let mut st = YardState::default();
        st.day.clean_streak = 5; // +0.5 (capped at +1.0)
        st.day.strikes = 2; // −0.3
        st.record_session(session(60, false), 0.5);
        assert_eq!(st.day.yard_earned_min, 36); // 60 × 0.5 × (1 + 0.5 − 0.3)
    }
}

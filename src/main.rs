//! Yard v0.3 — the core loop plus REAL idle detection (plan doc section 7)
//! and a live session view (countdown + bar + allowance gauge + status).
//! `yard add/start/spend/read/report`. The cell still doesn't lock (v0.4+)
//! and audio still doesn't play — small slices, shipping each one.

mod idle;
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
    Start {
        /// Task name (must exist; see `yard add`).
        task: String,
        /// Plan preset: quick (1 min, for testing), pomodoro 25/5, hour 60/15, deep 90/20.
        #[arg(long, default_value = "pomodoro")]
        plan: String,
    },
    /// Tell Yard you're reading for N minutes: extra idle allowance next session.
    Read { minutes: u64 },
    /// Spend earned yard time: `yard spend 30`.
    Spend { minutes: u64 },
    /// Today's report: focus vs goal, yard earned vs spent, streak.
    Report {},
}

/// "2h", "90m", "1h30" -> minutes. Rejects 0 and unparsable input.
fn parse_duration_min(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    let (hours, mins) = if let Some((h, m)) = s.split_once('h') {
        (
            h.trim(),
            m.trim().trim_end_matches('m').trim(),
        )
    } else {
        ("0", s.trim_end_matches('m').trim())
    };
    let hours: u64 = if hours.is_empty() { 0 } else { hours.parse().map_err(|_| format!("bad duration: {s}"))? };
    let mins: u64 = if mins.is_empty() { 0 } else { mins.parse().map_err(|_| format!("bad duration: {s}"))? };
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

/// Free idle seconds per session: 15% of its length (starter value, tune me).
const IDLE_ALLOWANCE_PCT: u64 = 15;

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
        Cmd::Start { task, plan } => run_session(&mut st, path, task, plan)?,
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

fn run_session(st: &mut YardState, path: &Path, task_name: &str, plan_name: &str) -> Result<()> {
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

    while focused_secs < target_secs {
        let action = gate.tick(idle::idle_seconds());
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

        let (status, accent) = match action {
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
                "PAUSED — no credit accruing. Move to resume.".to_string(),
                Some("31"),
            ),
            Action::Resumed => ("welcome back — counting again".to_string(), Some("32")),
        };

        if tty {
            let f = ui::Frame::focus(
                task_name,
                plan.name,
                focused_secs,
                target_secs,
                allowance_secs,
                gate.allowance_left(),
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
    println!("\nThe cell opens.");
    let sensor_known = idle::idle_seconds().is_some();
    if !sensor_known {
        // Honest fallback — no sensor platform; asked on a terminal OR piped:
        // v0.1's honesty check applies regardless of output mode.
        print!("Focused the whole time? [Y/n]: ");
        std::io::stdout().flush().ok();
        let mut ans = String::new();
        std::io::stdin().lock().read_line(&mut ans).ok();
        if matches!(ans.trim().to_uppercase().as_str(), "N") {
            focused_secs /= 2;
        }
    }

    let focused_min = focused_secs / 60;
    let bailed = false; // idle never counts as a bail; strikes are for real misbehaviour (v0.4)
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
    if let Some(t) = st.tasks.iter().find(|t| t.name == task_name) {
        println!("Task progress: {}/{} min today", t.done_min, t.daily_goal_min);
    }

    // The rest phase (plan doc section 4): the yard is open. This is your
    // reward break; the focus credit is already banked and safe.
    let rest_secs = plan.rest_min * 60;
    if rest_secs > 0 {
        if tty {
            println!("\n  Yard time — stretch, look far away, refill the cup.\n");
            std::io::stdout().flush().ok();
            print!("\x1b[?25l");
            std::io::stdout().flush().ok();
            first_frame = true;
            let mut rest_done = 0u64;
            while rest_done < rest_secs {
                // Rest view: countdown + bar + already-banked reassurance.
                let f = ui::Frame::rest(
                    task_name,
                    plan.name,
                    rest_done,
                    rest_secs,
                    "the yard is open — break freely".to_string(),
                    Some("36"),
                );
                draw_frame(&f, first_frame);
                first_frame = false;
                rest_done += 1;
                // One-second fine-grained countdown keeps the bar honest.
                std::thread::sleep(idle::sleep_secs());
            }
            println!("\nThe yard closes. Ready for the next cell? `yard start <task>`.\n");
            print!("\x1b[?25h");
            std::io::stdout().flush().ok();
        } else {
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
    println!("  Clean streak: {} sessions", st.day.clean_streak);
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
        println!("  {bar} {:>3}% {}/{} min", pct, t.done_min, t.daily_goal_min);
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
}

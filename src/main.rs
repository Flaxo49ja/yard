//! Yard v0.1 — the core loop only, per the plan doc's roadmap:
//! `yard add`, `yard start` (timer + plan ratio), `yard spend`, `yard report`.
//! No lockdown, no audio, no idle checks yet — those are v0.2+ by design.

mod state;

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use state::{Day, Session, Task, YardState};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Start a focus session. Answer honestly at the end; dishonesty costs.
    Start {
        /// Task name (must exist; see `yard add`).
        task: String,
        /// Plan preset: quick (1 min, for testing), pomodoro 25/5, hour 60/15, deep 90/20.
        #[arg(long, default_value = "pomodoro")]
        plan: String,
    },
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
    Plan { name: "quick", focus_min: 1, rest_min: 1 }, // for trying the loop out
    Plan { name: "pomodoro", focus_min: 25, rest_min: 5 },
    Plan { name: "hour", focus_min: 60, rest_min: 15 },
    Plan { name: "deep", focus_min: 90, rest_min: 20 },
];

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
        Cmd::Start { task, plan } => run_session(&mut st, task, plan)?,
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

fn run_session(st: &mut YardState, task_name: &str, plan_name: &str) -> Result<()> {
    ensure!(
        st.task_mut(task_name).is_some(),
        "unknown task {task_name:?} — `yard add` it first"
    );
    let plan = PLANS
        .iter()
        .find(|p| p.name == plan_name)
        .with_context(|| format!("unknown plan {plan_name:?} (try: pomodoro, hour, deep)"))?;
    let ratio = plan.rest_min as f64 / plan.focus_min as f64;

    println!(
        "Plan {} : {} min focus, {} min rest. Task: {}",
        plan.name, plan.focus_min, plan.rest_min, task_name
    );
    println!("v0.1 is an honest timer, not a lockdown (that's v0.2). Ctrl+C to bail.");

    let start = Instant::now();
    let total_secs = plan.focus_min * 60;
    while start.elapsed().as_secs() < total_secs {
        let left = total_secs.saturating_sub(start.elapsed().as_secs());
        print!("\r  {:>3}:{:02} left in the cell", left / 60, left % 60);
        std::io::stdout().flush().ok();
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("\nThe cell opens. Alive check:");

    // v0.1 alive check is an honesty prompt; real idle detection is v0.2.
    print!("Focused the whole time? [Y/n]: ");
    std::io::stdout().flush().ok();
    let mut ans = String::new();
    std::io::stdin().lock().read_line(&mut ans).ok();
    let honest = matches!(ans.trim(), "" | "y" | "Y" | "yes");
    let focused_min = if honest { plan.focus_min } else { plan.focus_min / 2 };

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    st.record_session(
        Session {
            task: task_name.to_string(),
            started_at: ts,
            focused_min,
            idle_min: 0,
            bailed: !honest,
        },
        ratio,
    );
    if let Some(t) = st.task_mut(task_name) {
        t.done_min += focused_min;
    }
    let earned = st.day.yard_earned_min;
    println!(
        "Focused {focused_min} min. Yard earned today: {earned} min (clean streak: {}).",
        st.day.clean_streak
    );
    if let Some(t) = st.tasks.iter().find(|t| t.name == task_name) {
        println!("Task progress: {}/{} min today", t.done_min, t.daily_goal_min);
    }
    Ok(())
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
        println!(
            "  {bar} {:>3}% {}/{} min",
            pct, t.done_min, t.daily_goal_min
        );
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

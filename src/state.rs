use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-task state: what you're working toward and today's progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub name: String,
    /// Daily goal in minutes (e.g. 120 for "2h today").
    pub daily_goal_min: u64,
    /// Focused minutes logged today.
    pub done_min: u64,
}

/// One completed (or bailed) session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub task: String,
    pub started_at: u64,   // epoch seconds
    pub focused_min: u64,  // only verified focus counts
    pub idle_min: u64,
    pub bailed: bool,
}

/// One day's earnings ledger.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Day {
    pub date: String,                 // "YYYY-MM-DD"
    pub yard_earned_min: u64,         // what focus earned
    pub yard_spent_min: u64,          // what was consumed by `yard spend`
    #[serde(default)]
    pub clean_streak: u64,            // consecutive clean sessions
    #[serde(default)]
    pub read_allowance_min: u64,      // queued by `yard read`, one-shot
    #[serde(default)]
    pub strikes: u64,                 // lockdown catches (v0.5)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct YardState {
    pub tasks: Vec<Task>,
    pub sessions: Vec<Session>,
    pub day: Day,
}

impl YardState {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn task_mut(&mut self, name: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.name == name)
    }

    pub fn record_session(&mut self, session: Session, ratio: f64) {
        // yard_time = focused_minutes × ratio × behavior multiplier.
        // Streak bonus compounds; each strike today costs 15% (hits the floor
        // at ×0.1 so the multiplier can never undo the work itself — the plan
        // doc's "there is a floor" promise, applied to both directions).
        let bonus = (self.day.clean_streak as f64 * 0.10).min(1.0);
        let penalty = (self.day.strikes as f64 * 0.15).min(0.9);
        let multiplier = 1.0 + bonus - penalty;
        let earned = (session.focused_min as f64 * ratio * multiplier).round() as u64;
        if !session.bailed {
            self.day.clean_streak += 1;
        } else {
            self.day.clean_streak = 0;
        }
        self.day.yard_earned_min += earned;
        self.sessions.push(session);
    }
}

pub fn today() -> String {
    // No chrono: epoch days -> YYYY-MM-DD by hand. UTC is fine for a personal tool.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's civil-from-days algorithm, public domain shape.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(focused: u64, bailed: bool) -> Session {
        Session {
            task: "t".into(),
            started_at: 0,
            focused_min: focused,
            idle_min: 0,
            bailed,
        }
    }

    // --- the ledger: yard_time = focused × ratio × multiplier --------------

    #[test]
    fn first_clean_session_earns_focus_times_ratio() {
        let mut st = YardState::default();
        st.record_session(session(60, false), 0.5);
        assert_eq!(st.day.yard_earned_min, 30); // 60 × 0.5 × 1.0
        assert_eq!(st.day.clean_streak, 1);
    }

    #[test]
    fn clean_streak_compounds_the_multiplier() {
        let mut st = YardState::default();
        st.record_session(session(60, false), 0.5); // ×1.0 (streak 0 at record time) -> 30
        st.record_session(session(60, false), 0.5); // ×1.1 (streak 1) -> 33
        assert_eq!(st.day.yard_earned_min, 63);
    }

    #[test]
    fn a_bail_resets_the_streak_but_still_earns_the_floor() {
        let mut st = YardState::default();
        st.record_session(session(60, false), 0.5); // 60×0.5×1.0 = 30
        st.record_session(session(60, false), 0.5); // 60×0.5×1.1 = 33
        // bail earns at the CURRENT multiplier, then resets it: 30×0.5×1.2 = 18
        st.record_session(session(30, true), 0.5);
        assert_eq!(st.day.clean_streak, 0);
        assert_eq!(st.day.yard_earned_min, 30 + 33 + 18);
        // the session AFTER the bail starts again at ×1.0, not ×1.3
        st.record_session(session(60, false), 0.5);
        assert_eq!(st.day.yard_earned_min, 30 + 33 + 18 + 30);
    }

    // --- persistence --------------------------------------------------------

    #[test]
    fn state_roundtrips_through_disk() -> anyhow::Result<()> {
        let mut st = YardState::default();
        st.tasks.push(Task {
            name: "Rust ownership".into(),
            daily_goal_min: 120,
            done_min: 25,
        });
        st.record_session(session(30, false), 0.5);
        let path = std::env::temp_dir().join(format!("yard_test_{}.json", std::process::id()));
        st.save(&path)?;
        let st2 = YardState::load(&path)?;
        std::fs::remove_file(&path).ok();
        assert_eq!(st2.tasks[0].name, "Rust ownership");
        assert_eq!(st2.tasks[0].done_min, 25);
        assert_eq!(st2.day.yard_earned_min, st.day.yard_earned_min);
        assert_eq!(st2.day.clean_streak, 1);
        Ok(())
    }

    #[test]
    fn load_missing_file_is_a_fresh_state_not_an_error() {
        let p = std::env::temp_dir().join("yard_test_does_not_exist.json");
        let st = YardState::load(&p).unwrap();
        assert!(st.tasks.is_empty());
        assert_eq!(st.day.yard_earned_min, 0);
    }

    // --- date math -----------------------------------------------------------

    #[test]
    fn civil_from_days_handles_epoch_and_leap_years() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1)); // Jan31 + Feb28
        assert_eq!(civil_from_days(790), (1972, 3, 1)); // +1972 leap Feb
    }

    #[test]
    fn today_is_a_wellformed_ymd_string() {
        let t = today();
        assert_eq!(t.len(), 10);
        let mut parts = t.split('-');
        let y = parts.next().unwrap().parse::<i64>().unwrap();
        let m = parts.next().unwrap().parse::<u32>().unwrap();
        let d = parts.next().unwrap().parse::<u32>().unwrap();
        assert!((2020..=2100).contains(&y));
        assert!((1..=12).contains(&m));
        assert!((1..=31).contains(&d));
    }
}

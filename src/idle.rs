//! v0.2 — real idle detection, per the plan doc (section 7):
//! - a free "reading allowance" (default 15% of session) absorbs small idles;
//! - once it's spent, "Still there?" pops; activity answers it and resets the
//!   allowance; 60s without activity → the timer PAUSES (no strikes; honest
//!   pause, credit stops until movement resumes).
//! - `yard read <minutes>` tops up the allowance (reading is studying).
//!
//! Platform idle sensors are checked at runtime; on an unknown platform the
//! gate treats every tick as active and the CLI keeps the v0.1 honesty
//! prompt as the fallback — it never *pretends* to know you were idle.

use std::time::Duration;

/// Idle below this many seconds counts as "actively at the keyboard".
const ACTIVE_THRESHOLD_SECS: u64 = 2;
/// Seconds the "Still there?" prompt waits before pausing the timer.
pub const STILL_THERE_DEADLINE: u64 = 60;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Gate {
    Watching,
    Asking { ticks_left: u64 },
    Paused,
}

/// One-second-granularity gate. Drive it with a tick per timer second.
pub struct IdleGate {
    allowance_total: u64,
    allowance_used: u64,
    gate: Gate,
}

/// What the session loop should do with this tick.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Action {
    /// Focus credit accrues for this second.
    Count,
    /// Idle absorbed by the reading allowance (no credit, no nag).
    Allowance,
    /// Allowance spent: show the "Still there?" prompt / keep waiting.
    Ask { ticks_left: u64 },
    /// Unanswered: the timer is paused — no credit accrues.
    Paused,
    /// Activity after a pause: allowance resets, counting resumes.
    Resumed,
}

impl IdleGate {
    pub fn new(allowance_secs: u64) -> Self {
        Self {
            allowance_total: allowance_secs,
            allowance_used: 0,
            gate: Gate::Watching,
        }
    }

    /// `idle_secs = None` means "platform cannot tell" → counts as active.
    pub fn tick(&mut self, idle_secs: Option<u64>) -> Action {
        let idle = idle_secs.unwrap_or(0);
        match self.gate {
            Gate::Watching => {
                if idle < ACTIVE_THRESHOLD_SECS {
                    Action::Count
                } else if self.allowance_used < self.allowance_total {
                    self.allowance_used += 1;
                    Action::Allowance
                } else {
                    let ticks_left = STILL_THERE_DEADLINE;
                    self.gate = Gate::Asking { ticks_left };
                    Action::Ask { ticks_left }
                }
            }
            Gate::Asking { ticks_left } => {
                if idle < ACTIVE_THRESHOLD_SECS {
                    // Movement itself answers the prompt: allowance resets.
                    self.gate = Gate::Watching;
                    self.allowance_used = 0;
                    Action::Resumed
                } else if ticks_left <= 1 {
                    self.gate = Gate::Paused;
                    Action::Paused
                } else {
                    self.gate = Gate::Asking { ticks_left: ticks_left - 1 };
                    Action::Ask { ticks_left: ticks_left - 1 }
                }
            }
            Gate::Paused => {
                if idle < ACTIVE_THRESHOLD_SECS {
                    self.gate = Gate::Watching;
                    self.allowance_used = 0;
                    Action::Resumed
                } else {
                    Action::Paused
                }
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        self.gate == Gate::Paused
    }
}

/// Alderwood-style platform idle sensor. Returns None when no sensor exists.
pub fn idle_seconds() -> Option<u64> {
    if cfg!(target_os = "linux") {
        // xprintidle (X11); under Wayland it reports XWayland input only —
        // good enough for a keep-studying nudge, bad for a keylogger.
        let out = std::process::Command::new("xprintidle").output().ok()?;
        let ms: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(ms / 1000)
    } else if cfg!(target_os = "macos") {
        let out = std::process::Command::new("ioreg")
            .args(["-c", "IOHIDSystem"])
            .output()
            .ok()?;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(rest) = line.strip_prefix("\"HIDIdleTime\" =") {
                let ns: u64 = rest.trim().parse().ok()?;
                return Some(ns / 1_000_000_000);
            }
        }
        None
    } else if cfg!(target_os = "windows") {
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command",
                "$t=Add-Type -M 'struct LII{public uint Size;public uint Time;} [DllImport(\"user32.dll\")] public static extern bool GetLastInputInfo(ref LII i);' -N Yard; $i=[Yard.LII]::new(); $i.Size=[System.Runtime.InteropServices.Marshal]::SizeOf($i); if([Yard.LII]::GetLastInputInfo([ref]$i)){[int]((Get-Date).MillisecondsSinceLastInput ?? 0) } else { 0 }"])
            .output()
            .ok()?;
        if out.status.success() {
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()
        } else {
            None
        }
    } else {
        None
    }
}

pub fn sleep_secs() -> Duration {
    Duration::from_secs(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;

    fn gate(allowance: u64) -> IdleGate {
        IdleGate::new(allowance)
    }

    #[test]
    fn active_ticks_always_count() {
        let mut g = gate(0);
        assert_eq!(g.tick(Some(0)), Count);
        assert_eq!(g.tick(Some(1)), Count);
        // unknown platform: never punished, always counts
        assert_eq!(g.tick(None), Count);
    }

    #[test]
    fn allowance_absorbs_small_idles_without_credit() {
        let mut g = gate(3);
        assert_eq!(g.tick(Some(5)), Allowance);
        assert_eq!(g.tick(Some(1)), Count); // brief activity (idle < 2s) costs nothing
        assert_eq!(g.tick(Some(9)), Allowance); // used 2/3
        assert_eq!(g.tick(Some(4)), Allowance); // used 3/3
        // budget exhausted: the next idle tick asks
        assert!(matches!(g.tick(Some(6)), Ask { ticks_left: 60 }));
    }

    #[test]
    fn prompt_deadline_expires_into_pause() {
        let mut g = gate(0);
        assert!(matches!(g.tick(Some(7)), Ask { .. })); // entering Asking (no tick burnt)
        for _ in 0..(STILL_THERE_DEADLINE - 1) {
            assert!(matches!(g.tick(Some(7)), Ask { .. }));
        }
        assert_eq!(g.tick(Some(7)), Paused); // the 60th waiting second
        assert!(g.is_paused());
        assert_eq!(g.tick(Some(9)), Paused); // silent idle stays paused
    }

    #[test]
    fn pausing_stops_credit_until_movement() {
        let mut g = gate(0);
        let mut last = g.tick(Some(9));
        for _ in 0..STILL_THERE_DEADLINE {
            last = g.tick(Some(9));
        }
        assert_eq!(last, Paused);
        assert_eq!(g.tick(Some(1)), Resumed); // movement = answer
        assert_eq!(g.tick(Some(0)), Count);   // counting resumes
    }

    #[test]
    fn answering_by_activity_resets_the_allowance() {
        let mut g = gate(1);
        assert_eq!(g.tick(Some(5)), Allowance);
        assert!(matches!(g.tick(Some(6)), Ask { .. }));
        assert_eq!(g.tick(Some(0)), Resumed);  // answered by coming back
        // full allowance available again after the reset
        assert_eq!(g.tick(Some(8)), Allowance);
    }

    #[test]
    fn prompt_during_asking_resumes_on_activity_without_pause() {
        let mut g = gate(0);
        assert!(matches!(g.tick(Some(9)), Ask { ticks_left: 60 }));
        let a = g.tick(Some(0));
        assert_eq!(a, Resumed);
    }
}

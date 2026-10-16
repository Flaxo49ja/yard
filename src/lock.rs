//! Lockdown (plan doc section 6): the cell watches what runs and what
//! windows say, and calls out offenders.
//!
//! v0.5 slice — deliberately small and safe:
//! - **Matching** is keyword-based (block words like youtube, mpv, anime…),
//!   checked against process names + /proc cmdline (token match) and Hyprland
//!   window class + title (text match). An allowlist-everything-else loop is
//!   a LATER slice: keyword-only settings can't strand you with a dead desktop.
//! - **Dry run is the default.** Offenders are flagged and strike-marked,
//!   nothing is closed. `yard start --lock enforce` arms the kill — the plan
//!   doc's own advice: dry-run first, so the lists get tuned on real data.
//! - **Enforce is ancestor-safe.** Yard, its parent shell(s), and everything
//!   up to init are unkillable no matter what they're named. Killing a window
//!   pid closes the WHOLE app (a YouTube tab kills the browser) — dry run
//!   exists precisely so you see that before arming it.
//! - **No daemon.** The watcher lives inside the session loop; bail/finish
//!   and it stops. You own the machine; friction, not a real prison.
//!
//! Zero new deps: /proc is read directly (the plan doc suggested `sysinfo`,
//! but a 30-line reader beats a crate here), windows come from `hyprctl
//! clients -j` parsed with the serde_json we already ship, and kills go
//! through `kill(1)`. On non-Hyprland sessions (or when `hyprctl` is absent)
//! only processes are watched — said honestly in the frame, never silently.

use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::process::Command;

/// Block words for process names, cmdline tokens, and window text (tune freely).
const BLOCK_KEYWORDS: &[&str] = &[
    "youtube",
    "youtu.be",
    "yt-dlp",
    "mpv",
    "ani-cli",
    "anime",
    "netflix",
    "twitch",
    "bilibili",
    "crunchyroll",
    "gogoanime",
    "hianime",
    "zoro",
    "discord",
    "steam",
    "spotify",
];

/// One thing the watcher caught.
#[derive(Debug, Clone, PartialEq)]
pub struct Offender {
    /// "proc" (a process) or "win" (a Hyprland toplevel).
    pub what: &'static str,
    pub pid: i32,
    /// What it is: process name, or window class + title.
    pub name: String,
    /// The keyword that did the accusing (goes in the frame line).
    pub matched: String,
}

impl Offender {
    /// Human string for the frame/report. Long titles get clipped by ui.rs.
    pub fn label(&self) -> String {
        match self.what {
            "win" => format!("{} (window)", self.name),
            _ => format!("{} (pid {})", self.name, self.pid),
        }
    }
}

/// A scan's outcome, ready for the frame and the ledger.
#[derive(Debug, Default)]
pub struct ScanOut {
    pub offenders: Vec<Offender>,
    /// New strikes this scan (cooldown applies per offender key).
    pub strikes_added: u64,
    /// Labels of processes actually closed (enforce mode only).
    pub closed: Vec<String>,
    /// Total processes/windows watched (for the honest "watching N" line).
    pub watched: usize,
}

/// Per-offender strike cooldown: one app open for five minutes is ONE
/// reprimand, not five minutes of them.
pub struct StrikeLedger {
    cooldown_secs: u64,
    last: HashMap<String, u64>,
}

impl StrikeLedger {
    pub fn new(cooldown_secs: u64) -> Self {
        Self { cooldown_secs, last: HashMap::new() }
    }

    /// true = this key earns a fresh strike; false = still cooling down.
    pub fn note(&mut self, key: &str, now: u64) -> bool {
        if let Some(&t) = self.last.get(key) {
            if now.saturating_sub(t) < self.cooldown_secs {
                return false;
            }
        }
        self.last.insert(key.to_string(), now);
        true
    }
}

/// Word segments: lowercase, split on every non-alphanumeric char. `mpv`,
/// `ani-cli` → [ani, cli], `youtu.be/xyz` → [youtu, be, xyz], so matching
/// is whole-WORD and hyphens/domains can't smuggle an offender through.
fn accused_token(haystack: &str, keyword: &str) -> bool {
    let hay = haystack.to_lowercase();
    let kw = keyword.to_lowercase();
    hay.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .any(|h| kw.split(|c: char| !c.is_ascii_alphanumeric()).filter(|s| !s.is_empty()).any(|k| h == k))
}

/// Text accusation for window class/title: prose is allowed substrings,
/// with a length floor so two-letter noise can't flag everything.
fn accused_text(haystack: &str, keyword: &str) -> bool {
    if keyword.len() < 3 {
        return false;
    }
    accused_token(haystack, keyword) || haystack.to_lowercase().contains(&keyword.to_lowercase())
}

/// Does this keyword have a name in the given haystack?
pub fn keyword_found(haystack: &str, keyword: &str, from_window: bool) -> bool {
    if from_window {
        accused_text(haystack, keyword)
    } else {
        accused_token(haystack, keyword)
    }
}

pub fn keyword_haystack_matches(haystack: &str, from_window: bool) -> Option<&'static str> {
    BLOCK_KEYWORDS
        .iter()
        .find(|k| keyword_found(haystack, k, from_window))
        .copied()
}

// --- /proc plumbing (std-only) -------------------------------------------

fn read_comm(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}

fn read_cmdline(pid: i32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None; // kernel threads have no cmdline
    }
    Some(String::from_utf8_lossy(&raw).replace('\0', " "))
}

/// The ppid from /proc/N/stat — comm may contain spaces and parens, so parse
/// AFTER the last ')' where the fixed fields start: `… ) S ppid …`.
fn ppid_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit(')').next()?;
    let mut fields = tail.split_whitespace();
    let _state = fields.next()?; // state sits right after ')'
    fields.next()?.parse::<i32>().ok()
}

/// Every ancestor up to (but never including) init. We never even CONSIDER
/// pid 1, so the worst case is a truncated chain, not a dead system.
pub fn protected_pids() -> BTreeSet<i32> {
    let me = std::process::id() as i32;
    let mut set = BTreeSet::from([me, 1]);
    let mut cur = me;
    // 32 hops is deeper than any sane shell nesting; the loop can't cycle
    // infinitely on /proc weirdness.
    for _ in 0..32 {
        match ppid_of(cur) {
            Some(p) if p > 1 => {
                set.insert(p);
                cur = p;
            }
            _ => break,
        }
    }
    set
}

// --- Hyprland windows ------------------------------------------------------

/// Offending windows via `hyprctl clients -j`. Only runs in a real Hyprland
/// session AND with the tool on PATH; otherwise None (honest fallback).
fn hyprland_windows() -> Option<Vec<Offender>> {
    std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let out = Command::new("hyprctl")
        .arg("clients")
        .arg("-j")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let arr = v.as_array()?;
    let mut found = Vec::new();
    for w in arr {
        let class = w.get("class").and_then(Value::as_str).unwrap_or("");
        let title = w.get("title").and_then(Value::as_str).unwrap_or("");
        let Some(pid) = w.get("pid").and_then(Value::as_i64) else { continue };
        let hay = format!("{class} {title}");
        let Some(k) = keyword_haystack_matches(&hay, true) else { continue };
        found.push(Offender {
            what: "win",
            pid: pid as i32,
            name: hay.trim().to_string(),
            matched: k.to_string(),
        });
    }
    Some(found)
}

// --- the watcher -----------------------------------------------------------

pub struct Watcher {
    pub enforce: bool,
    /// Yard + every ancestor up to init. Unkillable no matter what.
    protected: BTreeSet<i32>,
    ledger: StrikeLedger,
}

impl Watcher {
    pub fn new(enforce: bool) -> Self {
        Self {
            enforce,
            protected: protected_pids(),
            ledger: StrikeLedger::new(60),
        }
    }

    /// Scan processes (always) + Hyprland windows (when available). One pid
    /// is one flag even if multiple keywords or both sources match it.
    pub fn scan(&mut self, now: u64) -> ScanOut {
        let mut out = ScanOut::default();
        let mut flagged: BTreeSet<i32> = BTreeSet::new();

        // -- processes (token match) ---------------------------------------
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok())
                else {
                    continue;
                };
                if pid <= 1 || self.protected.contains(&pid) {
                    continue;
                }
                out.watched += 1;
                if flagged.contains(&pid) {
                    continue;
                }
                let name_from_comm = read_comm(pid).unwrap_or_default();
                if name_from_comm.is_empty() {
                    continue; // raced exit or kernel thread: silence is honest
                }
                let halted = read_cmdline(pid).unwrap_or_default();
                // What users (and windows) SEE is argv[0], not the kernel's
                // 15-char comm — show that, fall back to comm.
                let argv0 = halted.split(' ').next().unwrap_or("");
                let base = argv0.rsplit('/').next().unwrap_or(argv0);
                let name = if base.is_empty() { name_from_comm.clone() } else { base.to_string() };
                let Some(k) = keyword_haystack_matches(&name_from_comm, false)
                    .or_else(|| keyword_haystack_matches(&name, false))
                    .or_else(|| keyword_haystack_matches(&halted, false))
                else {
                    continue;
                };
                flagged.insert(pid);
                out.offenders
                    .push(Offender { what: "proc", pid, name, matched: k.to_string() });
            }
        }

        // -- windows (text match) ------------------------------------------
        if let Some(wins) = hyprland_windows() {
            out.watched += wins.len();
            for w in wins {
                if flagged.insert(w.pid) {
                    out.offenders.push(w);
                }
            }
        }

        // -- strikes with cooldown, then (only if armed) the close ----------
        let mut closed = Vec::new();
        for off in &out.offenders {
            let key = format!("{}:{}", off.what, off.pid);
            if self.ledger.note(&key, now) {
                out.strikes_added += 1;
            }
            if self.enforce && self.kill(off.pid) {
                closed.push(off.label());
            }
        }
        out.closed = closed;
        out
    }

    /// TERM, then let it go. Failures are honest, not fatal.
    fn kill(&self, pid: i32) -> bool {
        Command::new("kill").arg(pid.to_string()).status().map(|s| s.success()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- keyword matching ------------------------------------------------------

    #[test]
    fn process_tokens_match_whole_words_not_substrings() {
        assert!(accused_token("mpv", "mpv"));
        assert!(accused_token("mpv --loop playlist", "mpv"));
        assert!(accused_token("/usr/bin/mpv", "mpv")); // slashes split
        assert!(accused_token("ani-cli", "ani-cli")); // punctuation kept together
        // the near-miss that motivated the token rule:
        assert!(!accused_token("tmpviewer", "mpv"));
        // dense hyphenated and dotted tokens accuse via segments:
        assert!(accused_token("watch-mpv-hd", "mpv"));
        assert!(accused_token("sleep.mpv", "mpv"));
        assert!(accused_token("zoro.to", "zoro"));
        assert!(accused_token("youtu.be/xyz", "youtu.be"));
    }

    #[test]
    fn window_text_is_prose_so_substrings_fine() {
        let title = "Lo-fi beats to study to - YouTube";
        assert_eq!(keyword_haystack_matches(title, true), Some("youtube"));
        assert_eq!(keyword_haystack_matches("Focused: rust ownership", true), None);
        assert!(accused_text("watch mpv in hd", "mpv"));
        assert!(keyword_haystack_matches("gogoanime one piece", true).is_some());
    }

    #[test]
    fn the_same_probe_gives_different_verdicts_by_source() {
        // `tmpviewer` would be an innocent process but a flaggable typo'ed title
        assert!(!keyword_haystack_matches("tmpviewer", false).is_some());
        assert!(keyword_haystack_matches("tmpviewer anime night", true).is_some());
    }

    #[test]
    fn no_accusation_for_empty_or_tiny_keyword_edges() {
        assert!(!accused_token("", "mpv"));
        assert!(!accused_text("mpv", "mp")); // two-letter keyword: floor applies
    }

    // --- strike ledger -----------------------------------------------------------

    #[test]
    fn one_strike_per_cooldown_window_per_offender() {
        let mut l = StrikeLedger::new(60);
        assert!(l.note("proc:123", 0));
        assert!(!l.note("proc:123", 30)); // cooling down
        assert!(!l.note("proc:123", 59)); // JUST inside
        assert!(l.note("proc:123", 60)); // exactly the cooldown: fresh strike
        assert!(l.note("proc:456", 0)); // a different key strikes immediately
    }

    // --- /proc plumbing ------------------------------------------------------------

    #[test]
    fn proc_metadata_of_a_child_is_sane() {
        let mut child = Command::new("sleep").arg("3").spawn().unwrap();
        let pid = child.id() as i32;
        let comm = read_comm(pid).unwrap();
        assert_eq!(comm, "sleep");
        let cmd = read_cmdline(pid).unwrap();
        assert!(cmd.contains("sleep"), "cmdline was: {cmd}");
        assert_eq!(ppid_of(pid).unwrap(), std::process::id() as i32);
        let _ = child.kill();
        assert!(child.wait().is_ok()); // reap: no zombie
    }

    #[test]
    fn protection_covers_self_parents_and_always_pid_one() {
        let prot = protected_pids();
        assert!(prot.contains(&(std::process::id() as i32)));
        // Never-kill set: init is belt-and-braces IN it — the scan separately
        // never flags pid <= 1; both layers must agree the system survives.
        assert!(prot.contains(&1));
        assert!(prot.len() >= 3, "a test process has at least yard+shell ancestors");
        // a stray child pid is NOT protected:
        let mut child = Command::new("sleep").arg("2").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(!prot.contains(&pid));
        let _ = child.kill();
        assert!(child.wait().is_ok()); // reap: no zombie
    }

    #[test]
    fn scan_of_a_quiet_machine_finds_no_offenders_but_counts_watched() {
        let mut w = Watcher::new(false);
        let out = w.scan(0);
        // This test environment is clean-ish; if a keyword app is genuinely
        // running the scan must not lie ABOUT its count:
        assert!(out.watched > 0, "must count what it watched, even zero finds");
        for off in &out.offenders {
            assert!(!off.label().is_empty());
        }
    }

    #[test]
    fn enforce_mode_closes_a_spoofed_offender_and_dry_run_does_not() {
        // `exec -a` makes argv[0] = mpv: a real /proc reader sees an offender.
        let mut spoofed = Command::new("bash")
            .args(["-c", "exec -a mpv sleep 60"])
            .spawn()
            .unwrap();
        let pid = spoofed.id() as i32;

        // dry run: flagged as an offender by name, still alive afterwards
        let mut dry = Watcher::new(false);
        let out = dry.scan(0);
        assert!(
            out.offenders.iter().any(|o| o.pid == pid && o.name == "mpv"),
            "spoofed mpv must be flagged; got {:?}",
            out.offenders
        );
        assert!(out.closed.is_empty(), "dry run closes nothing");
        assert!(
            Command::new("kill").arg("-0").arg(pid.to_string()).status().unwrap().success(),
            "dry run must leave the process alive"
        );

        // enforce: the same scan closes it (fresh process so no pid reuse)
        let mut spoofed2 = Command::new("bash")
            .args(["-c", "exec -a mpv sleep 60"])
            .spawn()
            .unwrap();
        let pid2 = spoofed2.id() as i32;
        let mut armed = Watcher::new(true);
        let out2 = armed.scan(0);
        assert!(
            out2.closed.iter().any(|l| l.contains(&pid2.to_string()) && l.contains("mpv")),
            "enforce must close the spoofed offender; got {:?}",
            out2.closed
        );
        let reaped = spoofed2.wait(); // reaped after TERM
        assert!(reaped.is_ok(), "closed child should reap cleanly");
        let _ = spoofed.kill();
        assert!(spoofed.wait().is_ok()); // reap: no zombie
    }
}

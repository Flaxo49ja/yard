# yard

A focus timer where you earn the break. You sit in the cell and work; the yard time that
comes out the other end is what you actually banked.

Pomodoro timers count wall-clock minutes whether you were reading or scrolling, so the
break at the end means nothing. Yard watches for real input, pauses when you go quiet,
and pays out in proportion to the focus it could verify.

## Install

```bash
git clone https://github.com/Flaxo49ja/yard.git
cd yard
cargo build --release
cp target/release/yard ~/.cargo/bin/    # or: cargo install --path .
```

Rust 2021 edition. No configuration file, no daemon, no network.

## Use

```bash
yard add "Rust ownership" --daily 2h      # a task and its daily goal
yard start "Rust ownership" --plan pomodoro
yard read 15                             # reading tops up idle allowance
yard spend 30                            # draw down earned yard time
yard report                              # today
yard report --week                       # last seven days, plus per-task totals
```

Plans:

| `--plan` | Focus | Rest |
| --- | --- | --- |
| `quick` | 1 min | 1 min |
| `pomodoro` | 25 min | 5 min (default) |
| `hour` | 60 min | 15 min |
| `deep` | 90 min | 20 min |

`quick` exists so you can watch the whole loop in two minutes before committing an
evening to it.

While a session runs: `[P]` pauses everything (the clock stops, nothing accrues),
`[R]` grants one more minute of reading allowance, `[X]` is the emergency exit. `[X]`
banks the partial credit as a bail and resets your clean streak, which is the point.

## How the yard time is calculated

```
yard_time = focused_minutes × (rest / focus) × multiplier
multiplier = 1 + min(clean_streak × 0.10, 1.0) − min(strikes × 0.15, 0.9)
```

A clean streak pays a little more each session, capped at double. Each lockdown strike
costs 15%, floored so it can never take more than 90% — the work is always worth
something. A bail earns at the current multiplier and then resets the streak to zero.

## Idle detection, and what it cannot see

On Linux the sensor is `xprintidle`. Under X11 that is the real thing. Under Wayland it
reports XWayland input only, so native Wayland windows can look idle while you are
typing in them. The timer says which sensor it found before the session starts.

Where no sensor exists, the session ends by asking you plainly: *Focused the whole
time? [Y/n]*. Answer `n` and the credit is halved. It is not verification, but it is
honest about not being verification.

## Lockdown

```bash
yard start "Rust ownership" --lock dry      # flag offenders, close nothing
yard start "Rust ownership" --lock enforce  # also close them
```

The watcher reads `/proc` for blocked keywords (youtube, mpv, discord, steam, anime
sites and so on) and, when a Hyprland session is present, window classes and titles via
`hyprctl`. One app open for five minutes is one strike, not five minutes of them.

`dry` is the default for a reason: in enforce mode closing a browser window closes the
whole browser. Yard, its parent shells and everything up to init are never killed. The
watcher lives inside the session loop, so bailing or finishing stops it — there is no
daemon and nothing runs in the background.

## State

`~/.local/share/yard/state.json`, or wherever `--state <path>` points. Thirty finished
days are kept for the week report. A day with no sessions is dropped rather than
archived as a row of zeros.

## Not there yet

- Focus audio and the desktop widget are planned, not built. The binary says so.
- Lockdown matching is keyword-based, so it can be fooled by a renamed binary. It is a
  speed bump for a person who wants to study, not a security control.
- Only the Linux idle sensor is implemented.

## Tests

```bash
cargo test
cargo clippy -- -D warnings
```

The ledger math (streak bonus, strike floor, bail reset), the duration parser, date
handling and the `/proc` readers all have tests, including the ones that spawn a
deliberately named child process. CI runs the same two commands on every push.

MIT.

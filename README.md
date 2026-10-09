# reminder

A small, self-hosted reminder system. One SQLite file, one Rust binary, one VPS — a phone notification and a desktop notification, both driven by a server that stays the single source of truth even when your laptop is asleep, closed, or off.

[![Rust](https://img.shields.io/badge/rust-edition%202021-orange)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-63%20passing-brightgreen)](#testing)
[![Database](https://img.shields.io/badge/database-SQLite%20(WAL)-003b57)](https://www.sqlite.org)
[![Status](https://img.shields.io/badge/status-personal%20project-blueviolet)](#project-status--roadmap)
[![License](https://img.shields.io/badge/license-unlicensed-lightgrey)](#license)

## Table of contents

- [Why this project matters](#why-this-project-matters)
- [Features](#features)
- [System architecture](#system-architecture)
- [Testing](#testing)
- [Design decisions](#design-decisions)
- [Getting started](#getting-started)
- [CLI reference](#cli-reference)
- [Configuration](#configuration)
- [Project status / roadmap](#project-status--roadmap)
- [License](#license)

## Why this project matters

Most reminder apps are either a subscription, a cloud account you don't control, or a phone-only feature that doesn't reach your laptop. This project exists to answer a much smaller question: *what's the least amount of software that reliably reminds me of things, on my own infrastructure, that I can still fully understand a year from now?*

That last part is the actual constraint. It would be easy to reach for Kubernetes, Postgres, Redis, a message queue, and a microservice per concern — and just as easy to end up with a system nobody, including its author, wants to touch six months later. This project deliberately avoids all of that. SQLite is the entire database. One Rust binary is both the CLI and the server. One VPS is the entire deployment target. Every architectural choice in this README exists to keep the system small enough that reading the source is still a reasonable way to understand it.

The payoff is a reminder system that:
- keeps running correctly whether your laptop is open, asleep, or in a drawer somewhere,
- degrades gracefully instead of catastrophically when a piece of it (ntfy, your laptop) is unavailable,
- has no recurring cost beyond a free-tier VPS, and
- is small enough that "what does this actually do" always has a short answer.

## Features

- **One-shot and recurring reminders** — repeat on any combination of weekdays, optionally bounded by an end date or a total occurrence count.
- **DST-safe scheduling** — the next fire time is resolved fresh against a real IANA timezone on every tick, not precomputed, so clock changes never silently shift a reminder by an hour.
- **Crash-safe, idempotent firing** — "has this already fired today" is a database constraint, not application logic, so a restart or an overlapping scheduler tick can never double-fire a reminder.
- **Push notifications via [ntfy](https://ntfy.sh)** — works from a headless server with no GUI; delivery is retried independently of the reminder itself, so a dead ntfy topic never loses reminders, only their push notification.
- **Automatic desktop notifications** — a systemd user service polls the server and shows Omarchy notifications when reminders are due. It starts at desktop login and catches up on pending occurrences after reconnecting.
- **Full CLI** — `add` / `list` / `edit` / `delete` / `complete`, all backed by the same HTTP API a phone or another machine would use.
- **Interactive TUI** (`reminder tui`, built on [ratatui](https://ratatui.rs)) — browse, add, edit, delete, complete, and toggle reminders without leaving the terminal.
- **One SQLite file** — no external database service, no connection pool tuning, WAL mode for safe concurrent access from the API, scheduler, and notifier in the same process.
- **Bearer-token authentication** — one long-lived token, checked in constant time, no OAuth machinery for a single-user server.
- **Plain HTTP + reverse proxy** — the binary doesn't embed a TLS stack; put nginx or Caddy in front for HTTPS, and the Rust side stays free of certificate-lifecycle code.

## System architecture

![Architecture diagram: an Oracle Cloud VM running reminder serve (HTTP API, scheduler, notify loop) over a single SQLite file, behind nginx for TLS; a phone receives push notifications via ntfy; a laptop polls the API with the CLI, TUI, and watch, and shows local notifications via omarchy-reminder](docs/architecture.svg)

The server is one Rust process (`reminder serve`) running three concurrent loops against one SQLite connection:

| Component | What it does | Cadence |
|---|---|---|
| HTTP API (`axum`) | CRUD on reminders, `complete`, and the `/occurrences/*` endpoints the laptop poller uses — all behind bearer-token auth | request-driven |
| Scheduler | Asks "what's due right now?" and records an occurrence for anything that fires | every 30s |
| Notify loop | Pushes any occurrence not yet delivered via ntfy, independent of when it fired | every 30s |

Two things follow directly from that split:

1. **The laptop is a client, not a scheduler.** `reminder watch` polls `GET /occurrences/pending-laptop` and shows Omarchy notifications locally, then acks it. If the laptop is off for a week, nothing is lost — the occurrences just sit as "pending" until it asks again.
2. **The phone is a push target, not a dependency.** If ntfy is unreachable, the reminder still fired, is still recorded, and still shows up in `reminder list` / the TUI. Only the push notification is delayed, and it's retried automatically once ntfy comes back.

See [`docs/architecture.svg`](docs/architecture.svg) for the diagram source.

## Testing

The guiding rule: **push logic out of anything that touches a terminal or a socket, and test the logic directly.** Concretely:

| Module | Tests | What's covered |
|---|---:|---|
| `model.rs` | 10 | weekday bitmask math, `parse_weekday_list` / `mask_to_weekday_str` round-trips |
| `db.rs` | 14 | every query against a real in-memory SQLite database — including the occurrence idempotency race and cascade deletes |
| `scheduler.rs` | 8 | `is_due()` against specific dates (one-shot, repeat, end date, occurrence cap) and a real DST transition via `chrono-tz` |
| `server/mod.rs` | 3 | the hand-rolled constant-time token comparison |
| `notify/mod.rs` | 3 | the retry loop's success/failure/give-up-after-N-attempts behavior, against a fake backend |
| `notify/ntfy.rs` | 4 | the real `NtfyBackend` HTTP request shape, against a throwaway local `axum` server (title header, auth header, non-ASCII fallback, error handling) |
| `client/tui.rs` | 15 | form validation/parsing, schedule formatting, edit-form prefill — everything except the actual rendering |
| `client/watch.rs` | 6 | poll → notify → ack, against a fake local server, including the "local notify failed, stays pending" path |
| **Total** | **63** | `cargo test` |

Nothing here uses a mocking library. Where a real dependency (SQLite, an HTTP server) was needed for a faithful test, it's a real one — an in-memory SQLite database, or a throwaway `axum` server bound to `127.0.0.1:0` — rather than a simulation of one. The `client/tui.rs` event loop and `main.rs`'s `serve` wiring are the main things *not* covered by `cargo test`, because there's very little logic left in them once the parsing and query logic is factored out; those were instead verified by driving the real compiled binary (including through a pseudo-terminal for the TUI) during development.

```sh
cargo test
```

## Design decisions

Each of these was a real fork in the road, not a default. The rule applied throughout: explain the tradeoff, then take the simplest option that actually satisfies the requirement.

| Decision | Alternative considered | Why this one |
|---|---|---|
| `rusqlite` + `spawn_blocking` | `sqlx` (async) | `sqlx`'s async layer pulls in a TLS stack meant for *network* databases — pure overhead for a local file. `rusqlite` has a far smaller dependency tree and is the more idiomatic choice for embedded SQLite. |
| One `Mutex<Connection>` | A connection pool | SQLite allows one writer at a time regardless; a pool just moves that same serialization into `SQLITE_BUSY` contention instead of an in-process lock, for no throughput gain at this scale. |
| Poll-based scheduler (tick every 30s) | A `tokio::time::sleep` future per reminder | Per-reminder timers need a tracked `JoinHandle` per reminder, cancelled/respawned on every edit, and rehydrated on restart. A single "what's due right now?" query needs none of that — restart handling and edits-take-effect-immediately fall out for free. |
| Flat-interval notification retry, capped at 10 attempts | Exponential backoff with a `last_attempt_at` column | True backoff protects against a load problem this project doesn't have (a personal topic, a handful of reminders a day). Same practical outcome — delivery resumes automatically, a dead topic stops being retried forever — for a fraction of the bookkeeping. |
| Pull-based laptop delivery (`reminder watch` polls) | Push, same as ntfy | The server has no way to reach a laptop that's asleep or behind NAT. Polling means "laptop was off" requires no special handling at all — occurrences just wait as `pending` until the next poll. |
| One long-lived bearer token, constant-time compared | OAuth / per-session tokens | Single-user server; token rotation and session machinery would be complexity with no corresponding threat model. |
| Plain HTTP + reverse proxy for TLS | TLS embedded in the Rust binary | Certificate lifecycle (renewal, reload) is a solved problem in nginx/Caddy. Reimplementing it in application code buys nothing. |
| `native-tls` (OpenSSL) for the outbound HTTP client | `rustls` | The pure-Rust `rustls` dependency chain churns fast enough that pinning it for an older toolchain became its own project; OpenSSL via `native-tls` is one `apt install libssl-dev` away and far more stable to build against. |

## Getting started

```sh
git clone <this-repo>
cd reminder
cargo build --release
```

Run the server (this is what runs on your VPS):

```sh
export REMINDER_TOKEN=$(openssl rand -hex 32)   # generate once, reuse everywhere below
export REMINDER_TZ=America/New_York              # your IANA timezone
reminder serve
```

Then, as a client (your laptop, or the same machine while testing):

```sh
export REMINDER_SERVER_URL=http://127.0.0.1:8080
export REMINDER_TOKEN=<same token as above>

reminder add "Study Go" --time 18:00 --repeat mon,wed,fri
reminder list
reminder tui
```

## Automatic Omarchy notifications

After building, install the desktop service once:

```sh
mkdir -p ~/.local/bin ~/.config/reminder ~/.config/systemd/user
install -m 755 target/release/reminder ~/.local/bin/reminder-bin
# ~/.config/reminder/env must contain REMINDER_SERVER_URL and REMINDER_TOKEN
# as KEY=value lines (without `export`). Keep this file private:
chmod 600 ~/.config/reminder/env
install -m 644 deploy/reminder-watch.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now reminder-watch.service
```

The service starts at graphical login, restarts after failures, and needs no
terminal or manually running `reminder watch`. Inspect it with
`systemctl --user status reminder-watch` or
`journalctl --user -u reminder-watch -n 30`.

The server resolves the scheduled date, time, and repeat days in `REMINDER_TZ`;
use `Asia/Kolkata` for IST. A reminder created at 14:00 for 14:45 becomes due at
14:45. Desktop delivery follows within roughly 40 seconds while connected
(30-second scheduler + 10-second desktop poll). Pending occurrences are shown
after waking/reconnecting. This does not create a second Omarchy countdown or
populate `omarchy reminder show`; it uses the same desktop notification system.
The laptop needs network access to receive new occurrences.

Phone pushes use ntfy high priority, requesting sound, vibration, and a pop-over.
Android notification settings still control whether those alerts are allowed.
If a message is present in ntfy but silent, inspect the topic's notification
settings and Android's high-priority notification channel. If it only appears
when opening the app, check ntfy instant delivery and battery restrictions.
See [ntfy phone setup](https://docs.ntfy.sh/subscribe/phone/) and
[priority behavior](https://docs.ntfy.sh/publish/#message-priority).

## CLI reference

```
reminder <COMMAND>
```

| Command | Description |
|---|---|
| `serve` | Run the HTTP API and the scheduler together. This is what runs on the VPS. |
| `add` | Add a new reminder. |
| `list` | List all reminders. |
| `tui` | Interactive terminal UI for browsing and managing reminders. |
| `watch` | Poll for reminders that fired and show them locally via Omarchy desktop notifications. Run this on your laptop/desktop. |
| `edit` | Edit an existing reminder. Only the flags you pass are changed. |
| `delete` | Delete a reminder. |
| `complete` | Mark a reminder's most recently fired occurrence as done. |
| `tick` | Run one scheduler pass directly against the local database, bypassing the server. Local debugging only. |
| `tick-loop` | Like `tick`, but loops forever. Local debugging only. |

Run `reminder <command> --help` for a command's flags.

## Configuration

All configuration is via environment variables — there's no config file (yet; see [roadmap](#project-status--roadmap)).

| Variable | Used by | Default | Purpose |
|---|---|---|---|
| `REMINDER_DB` | `serve`, `tick`, `tick-loop` | `reminder.db` | Path to the SQLite file. |
| `REMINDER_TZ` | `serve`, `tick`, `tick-loop` | `UTC` | IANA timezone reminder times are resolved against. |
| `REMINDER_TOKEN` | `serve`, every client command | *(required)* | The shared bearer token. |
| `REMINDER_BIND` | `serve` | `127.0.0.1:8080` | Address the HTTP API listens on. |
| `REMINDER_SERVER_URL` | every client command | `http://127.0.0.1:8080` | Where the CLI/TUI/watch send requests. |
| `REMINDER_NTFY_TOPIC` | `serve` | *(unset = push disabled)* | Your ntfy topic name. |
| `REMINDER_NTFY_SERVER` | `serve` | `https://ntfy.sh` | ntfy server base URL (for self-hosting). |
| `REMINDER_NTFY_TOKEN` | `serve` | *(unset)* | Bearer token for a self-hosted, access-controlled ntfy instance. |
| `REMINDER_OMARCHY_CMD` | `watch` | *(unset)* | Optional custom command run as `<cmd> <title>`; defaults to immediate Omarchy notifications. |

## Project status / roadmap

This is being built in phases; the server, CLI, TUI, ntfy push, and laptop `watch` loop are done and tested. Still open:

- [ ] Deploy to an actual Oracle Cloud Always Free VM (systemd unit, automatic restart, backups)
- [ ] HTTPS via nginx/Caddy + Let's Encrypt in front of `reminder serve`
- [ ] A config file, as an alternative to environment variables
- [ ] Shell completion, import/export

## License

No license has been chosen yet — this started as a personal project. If you'd like to use or fork it, open an issue, or add an MIT/Apache-2.0 `LICENSE` file before treating it as available for reuse.

# reminder

A small, self-hosted reminder system. One SQLite file, one Rust binary, one VPS — a phone notification and a desktop notification, both driven by a server that stays the single source of truth even when your laptop is asleep, closed, or off.

[![Rust](https://img.shields.io/badge/rust-edition%202021-orange)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-63%20passing-brightgreen)](#testing)
[![Database](https://img.shields.io/badge/database-SQLite%20(WAL)-003b57)](https://www.sqlite.org)
[![Status](https://img.shields.io/badge/status-deployed-22c55e)](#project-status--roadmap)
[![License](https://img.shields.io/badge/license-MIT-blue)](#license)

## Table of contents

- [Why this project matters](#why-this-project-matters)
- [Features](#features)
- [Current deployment](#current-deployment)
- [System architecture](#system-architecture)
- [Testing](#testing)
- [Design decisions](#design-decisions)
- [Getting started](#getting-started)
- [Automatic Omarchy notifications](#automatic-omarchy-notifications)
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

## Current deployment

The system is deployed and phone and desktop delivery have been verified.

| Component | Current setup |
|---|---|
| Oracle Cloud VM | **VM.Standard.E2.1.Micro**, x86_64 |
| VM image | **Canonical Ubuntu 22.04** |
| Server | `/opt/reminder/reminder serve`, managed by `reminder.service` |
| API transport | HTTPS reverse proxy → `127.0.0.1:8080`; bearer-token authentication |
| Storage | `/opt/reminder/reminder.db`, SQLite in WAL mode |
| Schedule timezone | `Asia/Kolkata` (IST) |
| Phone | Redmi 12 5G, subscribed through the ntfy app to the configured ntfy.sh topic |
| Desktop | Omarchy; `reminder-watch.service` starts automatically at graphical login |

See [the deployment guide](deploy/README.md) for VM setup and the
[desktop setup](#automatic-omarchy-notifications) below for the user service.

## System architecture

![Dark architecture diagram showing the Omarchy CLI and automatic desktop daemon connecting over HTTPS to an Oracle VM running Canonical Ubuntu 22.04 on VM.Standard.E2.1.Micro. The Rust API, scheduler, and notification worker share SQLite. The notification worker sends high-priority pushes through ntfy.sh to a Redmi 12 5G.](docs/architecture.svg)

The server is one Rust process (`reminder serve`) with an HTTP API, scheduler, and notification worker sharing one SQLite connection. The desktop daemon runs separately on the laptop:

| Component | What it does | Cadence |
|---|---|---|
| HTTP API (`axum`) | CRUD on reminders, `complete`, and the `/occurrences/*` endpoints the laptop poller uses — all behind bearer-token auth | request-driven |
| Scheduler | Asks "what's due right now?" and records an occurrence for anything that fires | every 30s |
| Notify loop | Sends pending occurrences to ntfy with `Priority: high`; retries up to 10 attempts per occurrence | every 30s |
| Desktop daemon (on the laptop) | Fetches pending occurrences, displays them with `omarchy-notification-send`, then acknowledges delivery | every 10s |

Two things follow directly from that split:

1. **Automatic desktop delivery.** The systemd user service runs `reminder watch --interval 10` in the background. It polls `GET /occurrences/pending-laptop`, displays each due reminder, and acknowledges successful delivery. Occurrences recorded while the laptop is off remain pending until it reconnects. A failed local notification is retried.
2. **Independent phone delivery.** The server records an occurrence before trying ntfy. Push failures do not block scheduling or desktop delivery; retries run every 30 seconds, up to 10 attempts. A successful ntfy response means the message was accepted, not that Android displayed or sounded it.

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
| `notify/ntfy.rs` | 4 | the real `NtfyBackend` HTTP request shape, against a throwaway local `axum` server (title, high priority, auth, non-ASCII fallback, error handling) |
| `client/tui.rs` | 15 | form validation/parsing, schedule formatting, edit-form prefill — everything except the actual rendering |
| `client/watch.rs` | 6 | poll → notify → ack, against a fake local server, including failed notifications staying pending, safe command arguments, and rejecting a zero poll interval |
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
export PATH="$PWD/target/release:$PATH"
```

Run the server (this is what runs on your VPS):

```sh
export REMINDER_TOKEN=$(openssl rand -hex 32)   # generate once, reuse everywhere below
export REMINDER_TZ=Asia/Kolkata                # current deployment uses IST
reminder serve
```

Then, as a client (your laptop, or the same machine while testing):

```sh
export REMINDER_SERVER_URL=http://127.0.0.1:8080
export REMINDER_TOKEN="your-shared-server-token"

reminder add "Study Go" --time 18:00 --repeat mon,wed,fri
reminder list
reminder tui
```

## Automatic Omarchy notifications

The current laptop already has this enabled. For another Omarchy machine,
build the binary, create `~/.config/reminder/env` with these values, then install
the service once:

```ini
REMINDER_SERVER_URL=https://your-reminder-server.example
REMINDER_TOKEN=your-shared-server-token
```

```sh
mkdir -p ~/.local/bin ~/.config/reminder ~/.config/systemd/user
install -m 755 target/release/reminder ~/.local/bin/reminder-bin.new
mv ~/.local/bin/reminder-bin.new ~/.local/bin/reminder-bin
# ~/.config/reminder/env must contain REMINDER_SERVER_URL and REMINDER_TOKEN
# as KEY=value lines (without `export`). Keep this file private:
chmod 600 ~/.config/reminder/env
install -m 644 deploy/reminder-watch.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now reminder-watch.service
# When updating an already-running installation:
systemctl --user restart reminder-watch.service
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

The server must run at or after the scheduled time on the scheduled day. It
catches up later on that day, but does not backfill earlier dates after downtime.
Desktop catch-up applies to occurrences the server has already recorded.

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
| `watch` | Desktop delivery loop; runs automatically through `reminder-watch.service`. Manual invocation is useful for debugging. Default poll interval: 10 seconds. |
| `edit` | Edit an existing reminder. Only the flags you pass are changed. |
| `delete` | Delete a reminder. |
| `complete` | Mark a reminder's most recently fired occurrence as done. |
| `tick` | Run one scheduler pass directly against the local database, bypassing the server. Local debugging only. |
| `tick-loop` | Like `tick`, but loops forever. Local debugging only. |

Run `reminder <command> --help` for a command's flags.

## Configuration

The application reads environment variables. The server service loads
`/opt/reminder/reminder.env`; the desktop service loads `~/.config/reminder/env`.
Use `KEY=value` lines without `export`, keep token files private (`chmod 600`),
and restart the relevant service after editing. Standalone CLI commands need
the same variables exported in their shell or loaded by a wrapper.

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

Implemented and deployed:

- [x] Oracle VM running Canonical Ubuntu 22.04 on VM.Standard.E2.1.Micro
- [x] Server managed by systemd, with startup and restart handling
- [x] HTTPS access to the authenticated API
- [x] One-shot and recurring schedules, CLI, and TUI
- [x] High-priority ntfy phone notifications, verified on Redmi 12 5G
- [x] Automatic Omarchy desktop daemon, verified with a scheduled reminder
- [x] 63 automated tests passing in the last validation run

Remaining work:

- [ ] Verify automated backup scheduling and restore recovery (a [backup script](deploy/backup.sh) is provided)
- [ ] Application-managed configuration file, as an alternative to environment variables
- [ ] Shell completion, import/export

## License

Licensed under the [MIT License](LICENSE).

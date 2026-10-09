mod client;
mod db;
mod model;
mod notify;
mod scheduler;
mod server;

use client::ApiClient;
use clap::{Parser, Subcommand};
use chrono::Local;
use chrono_tz::Tz;
use model::{NewReminder, ReminderPatch};

/// A small personal reminder system. The server (`serve`) is the
/// source of truth; every other subcommand here is an HTTP client of
/// it, the same way the phone (via ntfy) and `reminder watch` on
/// another machine would be.
#[derive(Parser)]
#[command(name = "reminder", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP API and the scheduler together. This is what runs
    /// on the VPS.
    Serve,

    /// Add a new reminder.
    Add {
        /// What to remind you of.
        title: String,
        /// Time of day, 24h "HH:MM", local to REMINDER_TZ.
        #[arg(long)]
        time: String,
        /// First date it can fire, "YYYY-MM-DD". Defaults to today.
        #[arg(long)]
        start: Option<String>,
        /// Comma-separated weekdays to repeat on, e.g. "mon,wed,fri".
        /// Omit for a one-shot reminder that fires once on `--start`.
        #[arg(long)]
        repeat: Option<String>,
        /// Stop after this many total firings.
        #[arg(long)]
        occurrences: Option<i64>,
        /// Stop firing after this date, "YYYY-MM-DD".
        #[arg(long)]
        end: Option<String>,
    },

    /// List all reminders.
    List,

    /// Interactive terminal UI for browsing and managing reminders.
    Tui,

    /// Poll for reminders that fired and show them locally via
    /// Omarchy notifications. Run this on your laptop/desktop; it does
    /// nothing on the server itself.
    Watch {
        /// Seconds between polls.
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
    },

    /// Edit an existing reminder. Only the flags you pass are changed.
    Edit {
        id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        time: Option<String>,
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        repeat: Option<String>,
        #[arg(long)]
        occurrences: Option<i64>,
        #[arg(long)]
        end: Option<String>,
        /// Clear the occurrence limit (make it unbounded).
        #[arg(long)]
        clear_occurrences: bool,
        /// Clear the end date (make it unbounded).
        #[arg(long)]
        clear_end: bool,
        #[arg(long)]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },

    /// Delete a reminder.
    Delete { id: i64 },

    /// Mark a reminder's most recently fired occurrence as done.
    Complete { id: i64 },

    /// Run one scheduler pass directly against the local database,
    /// bypassing the server. For local debugging only -- in normal
    /// use the scheduler runs inside `serve`.
    Tick,
    /// Like `tick`, but loops forever. For local debugging only.
    TickLoop,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    // A thin `main` that only handles error *presentation*: any
    // failure surfaces as a clean "Error: ..." line (with the full
    // anyhow cause chain via `{:#}`) rather than the derive(Debug)
    // dump -- backtrace included -- that returning `Result` straight
    // from `main` would print.
    match run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Serve => run_serve().await,
        Command::Tick => run_tick_once().await,
        Command::TickLoop => run_tick_loop().await,
        Command::Add {
            title,
            time,
            start,
            repeat,
            occurrences,
            end,
        } => {
            let client = api_client()?;
            let start = start.unwrap_or_else(|| Local::now().date_naive().to_string());
            let repeat_days = match repeat {
                Some(r) => model::parse_weekday_list(&r).map_err(|e| anyhow::anyhow!(e))?,
                None => 0,
            };
            let new = NewReminder {
                title,
                time,
                start_date: start,
                repeat_days,
                occurrences_total: occurrences,
                end_date: end,
            };
            let created = client.create_reminder(&new).await?;
            print_reminder(&created);
            Ok(())
        }
        Command::List => {
            let client = api_client()?;
            let reminders = client.list_reminders().await?;
            if reminders.is_empty() {
                println!("no reminders yet -- add one with `reminder add`");
            }
            for r in &reminders {
                print_reminder(r);
            }
            Ok(())
        }
        Command::Tui => {
            let client = api_client()?;
            client::tui::run(client).await
        }
        Command::Watch { interval } => {
            let client = api_client()?;
            let omarchy_cmd = std::env::var("REMINDER_OMARCHY_CMD").ok();
            let notifier = client::watch::OmarchyNotifier::new(omarchy_cmd);
            client::watch::run(client, notifier, interval).await
        }
        Command::Edit {
            id,
            title,
            time,
            start,
            repeat,
            occurrences,
            end,
            clear_occurrences,
            clear_end,
            enable,
            disable,
        } => {
            if enable && disable {
                anyhow::bail!("pass at most one of --enable / --disable");
            }
            if occurrences.is_some() && clear_occurrences {
                anyhow::bail!("pass at most one of --occurrences / --clear-occurrences");
            }
            if end.is_some() && clear_end {
                anyhow::bail!("pass at most one of --end / --clear-end");
            }

            let repeat_days = match repeat {
                Some(r) => Some(model::parse_weekday_list(&r).map_err(|e| anyhow::anyhow!(e))?),
                None => None,
            };
            let patch = ReminderPatch {
                title,
                time,
                start_date: start,
                repeat_days,
                occurrences_total: if clear_occurrences {
                    Some(None)
                } else {
                    occurrences.map(Some)
                },
                end_date: if clear_end { Some(None) } else { end.map(Some) },
                enabled: if enable {
                    Some(true)
                } else if disable {
                    Some(false)
                } else {
                    None
                },
            };

            let client = api_client()?;
            match client.update_reminder(id, &patch).await? {
                Some(r) => {
                    print_reminder(&r);
                    Ok(())
                }
                None => anyhow::bail!("no reminder with id {id}"),
            }
        }
        Command::Delete { id } => {
            let client = api_client()?;
            if client.delete_reminder(id).await? {
                println!("deleted reminder {id}");
                Ok(())
            } else {
                anyhow::bail!("no reminder with id {id}")
            }
        }
        Command::Complete { id } => {
            let client = api_client()?;
            if client.complete_reminder(id).await? {
                println!("marked reminder {id} complete");
                Ok(())
            } else {
                anyhow::bail!("reminder {id} has no fired occurrence to complete")
            }
        }
    }
}

fn print_reminder(r: &model::Reminder) {
    let schedule = if r.repeat_days == 0 {
        format!("once on {}", r.start_date)
    } else {
        format!("repeats, mask {:#09b}, from {}", r.repeat_days, r.start_date)
    };
    let status = if r.enabled { "enabled" } else { "disabled" };
    println!("[{}] {} at {} -- {} ({status})", r.id, r.title, r.time, schedule);
}

fn timezone() -> anyhow::Result<Tz> {
    std::env::var("REMINDER_TZ")
        .unwrap_or_else(|_| "UTC".to_string())
        .parse()
        .map_err(|_| anyhow::anyhow!("REMINDER_TZ must be a valid IANA timezone, e.g. America/New_York"))
}

fn api_client() -> anyhow::Result<ApiClient> {
    let base_url =
        std::env::var("REMINDER_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let token = std::env::var("REMINDER_TOKEN")
        .map_err(|_| anyhow::anyhow!("REMINDER_TOKEN must be set to the same token the server uses"))?;
    Ok(ApiClient::new(base_url, token))
}

async fn run_serve() -> anyhow::Result<()> {
    let db_path = std::env::var("REMINDER_DB").unwrap_or_else(|_| "reminder.db".to_string());
    let tz = timezone()?;
    let token = std::env::var("REMINDER_TOKEN").map_err(|_| {
        anyhow::anyhow!("REMINDER_TOKEN must be set -- generate one with, e.g., `openssl rand -hex 32`")
    })?;
    let bind_addr = std::env::var("REMINDER_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let db = db::Db::connect(&db_path).await?;

    // Scheduler runs as a background task alongside the API. If it
    // errors, that's fatal to the whole process rather than silently
    // going dark -- a reminder server that stops scheduling but keeps
    // answering HTTP requests would look healthy while quietly doing
    // nothing.
    let scheduler_db = db.clone();
    let scheduler_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            match scheduler::tick(&scheduler_db, tz).await {
                Ok(fired) => {
                    for (reminder, occ) in fired {
                        println!(
                            "fired: [{}] {} (occurrence {} for {})",
                            reminder.id, reminder.title, occ.id, occ.occurrence_date
                        );
                    }
                }
                // A single tick failing (e.g. transient DB contention)
                // shouldn't take the whole server down -- log it and
                // let the next tick, 30s later, try again.
                Err(e) => eprintln!("scheduler tick failed: {e:#}"),
            }
        }
    });

    // ntfy push is optional: if REMINDER_NTFY_TOPIC isn't set, this
    // task just parks forever via `std::future::pending()` rather than
    // being conditionally spawned or omitted from the `select!` below
    // -- that keeps this branch structurally identical to the
    // scheduler's whether or not push is configured, instead of
    // needing special-cased plumbing for the "disabled" case.
    let notify_backend = match std::env::var("REMINDER_NTFY_TOPIC") {
        Ok(topic) => {
            let server = std::env::var("REMINDER_NTFY_SERVER")
                .unwrap_or_else(|_| "https://ntfy.sh".to_string());
            let token = std::env::var("REMINDER_NTFY_TOKEN").ok();
            println!("ntfy push enabled: {server}/{topic}");
            Some(notify::ntfy::NtfyBackend::new(server, topic, token))
        }
        Err(_) => {
            println!(
                "REMINDER_NTFY_TOPIC not set -- push notifications disabled (reminders still work locally)"
            );
            None
        }
    };

    let notify_db = db.clone();
    let notify_handle = tokio::spawn(async move {
        match notify_backend {
            Some(backend) => {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if let Err(e) = notify::retry_pending(&notify_db, &backend).await {
                        eprintln!("ntfy retry pass failed: {e:#}");
                    }
                }
            }
            None => std::future::pending::<()>().await,
        }
    });

    let app = server::build_router(db, token);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    println!("reminder server listening on http://{bind_addr} (timezone: {tz})");
    println!(
        "note: this serves plain HTTP -- put a TLS-terminating reverse proxy in front for anything beyond localhost"
    );

    // Neither background loop above returns under normal operation --
    // this select! just means "run the API server, but if either
    // background task panics, bring the whole process down rather than
    // silently keep serving HTTP with scheduling or notifications
    // dead."
    tokio::select! {
        result = axum::serve(listener, app) => { result?; }
        result = scheduler_handle => { result?; }
        result = notify_handle => { result?; }
    }
    Ok(())
}

async fn run_tick_once() -> anyhow::Result<()> {
    let db_path = std::env::var("REMINDER_DB").unwrap_or_else(|_| "reminder.db".to_string());
    let tz = timezone()?;
    let db = db::Db::connect(&db_path).await?;

    let fired = scheduler::tick(&db, tz).await?;
    if fired.is_empty() {
        println!("nothing due right now");
    }
    for (reminder, occ) in fired {
        println!(
            "fired: [{}] {} (occurrence {} for {})",
            reminder.id, reminder.title, occ.id, occ.occurrence_date
        );
    }
    Ok(())
}

async fn run_tick_loop() -> anyhow::Result<()> {
    let db_path = std::env::var("REMINDER_DB").unwrap_or_else(|_| "reminder.db".to_string());
    let tz = timezone()?;
    let db = db::Db::connect(&db_path).await?;

    println!("scheduler running, ticking every 30s (timezone: {tz}), Ctrl-C to stop");
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        let fired = scheduler::tick(&db, tz).await?;
        for (reminder, occ) in fired {
            println!(
                "fired: [{}] {} (occurrence {} for {})",
                reminder.id, reminder.title, occ.id, occ.occurrence_date
            );
        }
    }
}

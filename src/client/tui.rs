//! The interactive TUI (`reminder tui`). Talks to the server through
//! the same [`ApiClient`] the other CLI subcommands use -- there's no
//! separate "TUI mode" of the API, just this being another client of
//! it.
//!
//! # Structure
//!
//! The parsing/formatting logic that's actually worth getting right
//! ([`build_new_reminder`], [`build_patch`], [`format_schedule`],
//! [`FormState::new_edit`]'s prefill mapping) is written as plain
//! functions with no dependency on the terminal, and is unit tested
//! below the same way `db.rs`/`scheduler.rs` are. The event loop and
//! rendering are, necessarily, untested glue -- there's very little
//! logic left in them to get wrong once the parsing is factored out.
//!
//! Each iteration of the event loop does two things in sequence:
//! first a synchronous match on the key press that only touches local
//! UI state (`app.mode`, cursor position, typed characters) and decides
//! whether an API call is needed; then, once that match has ended and
//! released its borrow of `app.mode`, the resulting [`Pending`] action
//! (if any) is awaited and applied to `app`. Keeping those two steps
//! separate avoids holding a `&mut app.mode` borrow across an `.await`
//! point, which is both a borrow-checker headache and, more
//! importantly, harder to read than it needs to be.

use crate::client::ApiClient;
use crate::model::{self, NewReminder, Reminder, ReminderPatch};
use crate::scheduler;
use chrono::Local;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use std::io;
use std::time::Duration;

const FIELD_LABELS: [&str; 6] = [
    "Title",
    "Time (HH:MM)",
    "Repeat (mon,wed,fri; blank = once)",
    "Start (YYYY-MM-DD)",
    "End (YYYY-MM-DD, optional)",
    "Occurrences (optional)",
];
const TITLE: usize = 0;
const TIME: usize = 1;
const REPEAT: usize = 2;
const START: usize = 3;
const END: usize = 4;
const OCCURRENCES: usize = 5;

pub async fn run(client: ApiClient) -> anyhow::Result<()> {
    install_panic_hook();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, client).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Without this, a panic mid-render (a bug, ideally never hit) leaves
/// the user's terminal stuck in raw/alternate-screen mode after the
/// process dies -- annoying enough to fix properly for a program
/// that's meant to run in someone's terminal daily.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(panic_info);
    }));
}

struct App {
    reminders: Vec<Reminder>,
    selected: usize,
    mode: Mode,
    status: Option<String>,
}

enum Mode {
    List,
    Form(FormState),
    ConfirmDelete { id: i64, title: String },
}

struct FormState {
    editing_id: Option<i64>,
    fields: [String; 6],
    focus: usize,
    error: Option<String>,
}

impl FormState {
    fn new_add() -> Self {
        let mut fields: [String; 6] = Default::default();
        fields[START] = Local::now().date_naive().to_string();
        FormState { editing_id: None, fields, focus: TITLE, error: None }
    }

    fn new_edit(r: &Reminder) -> Self {
        let fields = [
            r.title.clone(),
            r.time.clone(),
            model::mask_to_weekday_str(r.repeat_days),
            r.start_date.clone(),
            r.end_date.clone().unwrap_or_default(),
            r.occurrences_total.map(|n| n.to_string()).unwrap_or_default(),
        ];
        FormState { editing_id: Some(r.id), fields, focus: TITLE, error: None }
    }

    fn focus_next(&mut self) {
        self.focus = (self.focus + 1) % self.fields.len();
    }

    fn focus_prev(&mut self) {
        self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
    }
}

/// What the synchronous key-handling step decided needs to happen
/// against the server. Computed while matching on `&mut app.mode`,
/// applied afterward once that borrow has ended -- see the module doc
/// comment.
enum Pending {
    None,
    Quit,
    Refresh,
    Create(NewReminder),
    Update(i64, ReminderPatch),
    Delete(i64),
    Complete(i64),
    Toggle(i64, bool),
}

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    client: ApiClient,
) -> anyhow::Result<()> {
    let mut app = App {
        reminders: client.list_reminders().await?,
        selected: 0,
        mode: Mode::List,
        status: None,
    };

    loop {
        terminal.draw(|f| ui(f, &app))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        let mut pending = Pending::None;

        match &mut app.mode {
            Mode::List => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => pending = Pending::Quit,
                KeyCode::Down | KeyCode::Char('j') => select_next(&mut app),
                KeyCode::Up | KeyCode::Char('k') => select_prev(&mut app),
                KeyCode::Char('r') => pending = Pending::Refresh,
                KeyCode::Char('a') => app.mode = Mode::Form(FormState::new_add()),
                KeyCode::Char('e') => {
                    if let Some(r) = app.reminders.get(app.selected) {
                        app.mode = Mode::Form(FormState::new_edit(r));
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(r) = app.reminders.get(app.selected) {
                        app.mode = Mode::ConfirmDelete { id: r.id, title: r.title.clone() };
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(r) = app.reminders.get(app.selected) {
                        pending = Pending::Complete(r.id);
                    }
                }
                KeyCode::Char(' ') | KeyCode::Char('n') => {
                    if let Some(r) = app.reminders.get(app.selected) {
                        pending = Pending::Toggle(r.id, !r.enabled);
                    }
                }
                _ => {}
            },
            Mode::Form(form) => match key.code {
                KeyCode::Esc => app.mode = Mode::List,
                KeyCode::Tab | KeyCode::Down => form.focus_next(),
                KeyCode::BackTab | KeyCode::Up => form.focus_prev(),
                KeyCode::Backspace => {
                    form.fields[form.focus].pop();
                }
                KeyCode::Char(c) => form.fields[form.focus].push(c),
                KeyCode::Enter => match form.editing_id {
                    None => match build_new_reminder(&form.fields) {
                        Ok(new) => pending = Pending::Create(new),
                        Err(e) => form.error = Some(e),
                    },
                    Some(id) => match build_patch(&form.fields) {
                        Ok(patch) => pending = Pending::Update(id, patch),
                        Err(e) => form.error = Some(e),
                    },
                },
                _ => {}
            },
            Mode::ConfirmDelete { id, .. } => match key.code {
                KeyCode::Char('y') => pending = Pending::Delete(*id),
                _ => app.mode = Mode::List, // anything else cancels
            },
        }

        // The match above has ended, so app.mode's borrow is released
        // and it's safe to `.await` and mutate `app` freely.
        match pending {
            Pending::None => {}
            Pending::Quit => break,
            Pending::Refresh => {
                app.reminders = client.list_reminders().await?;
                app.status = Some("refreshed".into());
            }
            Pending::Create(new) => {
                let created = client.create_reminder(&new).await?;
                app.reminders.push(created);
                app.status = Some("added".into());
                app.mode = Mode::List;
            }
            Pending::Update(id, patch) => {
                if let Some(updated) = client.update_reminder(id, &patch).await? {
                    replace_reminder(&mut app.reminders, updated);
                    app.status = Some("saved".into());
                }
                app.mode = Mode::List;
            }
            Pending::Delete(id) => {
                client.delete_reminder(id).await?;
                app.reminders.retain(|r| r.id != id);
                if app.selected >= app.reminders.len() {
                    app.selected = app.reminders.len().saturating_sub(1);
                }
                app.status = Some(format!("deleted #{id}"));
                app.mode = Mode::List;
            }
            Pending::Complete(id) => {
                app.status = Some(if client.complete_reminder(id).await? {
                    format!("marked #{id} complete")
                } else {
                    format!("#{id} has no fired occurrence to complete")
                });
            }
            Pending::Toggle(id, enabled) => {
                let patch = ReminderPatch { enabled: Some(enabled), ..Default::default() };
                if let Some(updated) = client.update_reminder(id, &patch).await? {
                    replace_reminder(&mut app.reminders, updated);
                }
            }
        }
    }

    Ok(())
}

fn select_next(app: &mut App) {
    if !app.reminders.is_empty() {
        app.selected = (app.selected + 1) % app.reminders.len();
    }
}

fn select_prev(app: &mut App) {
    if !app.reminders.is_empty() {
        app.selected = (app.selected + app.reminders.len() - 1) % app.reminders.len();
    }
}

fn replace_reminder(reminders: &mut [Reminder], updated: Reminder) {
    if let Some(slot) = reminders.iter_mut().find(|r| r.id == updated.id) {
        *slot = updated;
    }
}

/// Render a reminder's schedule as a short human-readable summary for
/// the list view, e.g. `"once, 2026-09-25"` or `"mon,wed,fri, from
/// 2026-09-21"`.
fn format_schedule(r: &Reminder) -> String {
    if r.repeat_days == 0 {
        format!("once, {}", r.start_date)
    } else {
        format!("{}, from {}", model::mask_to_weekday_str(r.repeat_days), r.start_date)
    }
}

/// Parse the add form's fields into a `NewReminder`, or a
/// human-readable error naming what's wrong. Reuses
/// `scheduler::parse_time`/`parse_date` rather than re-validating the
/// formats itself, so the form and the scheduler can never disagree
/// about what counts as a valid time or date.
fn build_new_reminder(fields: &[String; 6]) -> Result<NewReminder, String> {
    let title = fields[TITLE].trim();
    if title.is_empty() {
        return Err("title is required".to_string());
    }
    scheduler::parse_time(fields[TIME].trim()).map_err(|e| e.to_string())?;

    let start = fields[START].trim();
    let start_date = if start.is_empty() {
        Local::now().date_naive().to_string()
    } else {
        scheduler::parse_date(start).map_err(|e| e.to_string())?;
        start.to_string()
    };

    let repeat_days = model::parse_weekday_list(fields[REPEAT].trim())?;

    let end = fields[END].trim();
    let end_date = if end.is_empty() {
        None
    } else {
        scheduler::parse_date(end).map_err(|e| e.to_string())?;
        Some(end.to_string())
    };

    let occurrences_total = parse_optional_count(fields[OCCURRENCES].trim())?;

    Ok(NewReminder {
        title: title.to_string(),
        time: fields[TIME].trim().to_string(),
        start_date,
        repeat_days,
        occurrences_total,
        end_date,
    })
}

/// Parse the edit form's fields into a `ReminderPatch`. Unlike the add
/// form, every field is pre-filled from the reminder being edited (see
/// `FormState::new_edit`), so an empty End/Occurrences field is taken
/// to mean "the user cleared this" rather than "leave unchanged" --
/// there's no other way to represent "clear this" as plain text, and
/// re-typing the current value to keep it is a small enough ask.
fn build_patch(fields: &[String; 6]) -> Result<ReminderPatch, String> {
    let title = fields[TITLE].trim();
    if title.is_empty() {
        return Err("title is required".to_string());
    }
    scheduler::parse_time(fields[TIME].trim()).map_err(|e| e.to_string())?;
    scheduler::parse_date(fields[START].trim()).map_err(|e| e.to_string())?;

    let repeat_days = model::parse_weekday_list(fields[REPEAT].trim())?;

    let end = fields[END].trim();
    let end_date = if end.is_empty() {
        Some(None)
    } else {
        scheduler::parse_date(end).map_err(|e| e.to_string())?;
        Some(Some(end.to_string()))
    };

    let occurrences_total = Some(parse_optional_count(fields[OCCURRENCES].trim())?);

    Ok(ReminderPatch {
        title: Some(title.to_string()),
        time: Some(fields[TIME].trim().to_string()),
        start_date: Some(fields[START].trim().to_string()),
        repeat_days: Some(repeat_days),
        occurrences_total,
        end_date,
        enabled: None, // toggled from the list view (space/n), not through the form
    })
}

fn parse_optional_count(s: &str) -> Result<Option<i64>, String> {
    if s.is_empty() {
        return Ok(None);
    }
    s.parse::<i64>()
        .map(Some)
        .map_err(|_| format!("'{s}' is not a whole number"))
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

fn ui(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    match &app.mode {
        Mode::List => draw_list(f, chunks[0], app),
        Mode::ConfirmDelete { title, .. } => {
            draw_list(f, chunks[0], app);
            draw_confirm_overlay(f, area, title);
        }
        Mode::Form(form) => draw_form(f, chunks[0], form),
    }
    draw_footer(f, chunks[1], app);
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = if app.reminders.is_empty() {
        vec![ListItem::new("no reminders yet -- press 'a' to add one")]
    } else {
        app.reminders
            .iter()
            .map(|r| {
                let status = if r.enabled { "" } else { " (disabled)" };
                ListItem::new(format!(
                    "{:<28} {:<8} {}{}",
                    truncate(&r.title, 28),
                    r.time,
                    format_schedule(r),
                    status
                ))
            })
            .collect()
    };

    let mut state = ListState::default();
    if !app.reminders.is_empty() {
        state.select(Some(app.selected));
    }

    let list = List::new(items)
        .block(Block::default().title("Reminders").borders(Borders::ALL))
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));

    f.render_stateful_widget(list, area, &mut state);
}

fn draw_form(f: &mut Frame, area: Rect, form: &FormState) {
    let title = match form.editing_id {
        Some(id) => format!("Edit reminder #{id}"),
        None => "Add reminder".to_string(),
    };

    let mut lines: Vec<Line> = FIELD_LABELS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let marker = if i == form.focus { "> " } else { "  " };
            let style = if i == form.focus {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{label}: "), style),
                Span::raw(form.fields[i].clone()),
            ])
        })
        .collect();

    lines.push(Line::from(""));
    if let Some(err) = &form.error {
        lines.push(Line::from(Span::styled(format!("error: {err}"), Style::default())));
    }

    let block = Block::default().title(title).borders(Borders::ALL);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_confirm_overlay(f: &mut Frame, area: Rect, title: &str) {
    let width = (title.len() as u16 + 20).min(area.width.saturating_sub(4)).max(30);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + area.height / 2 - 2,
        width,
        height: 4,
    };
    let text = format!("Delete '{title}'? (y/n)");
    let block = Block::default().title("Confirm").borders(Borders::ALL);
    f.render_widget(ratatui::widgets::Clear, popup);
    f.render_widget(Paragraph::new(text).block(block), popup);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let hint = match &app.mode {
        Mode::List => "a Add  e Edit  d Delete  c Complete  Space Toggle  r Refresh  q Quit",
        Mode::Form(_) => "Tab/↑↓ move field  Enter save  Esc cancel",
        Mode::ConfirmDelete { .. } => "y confirm  any other key cancels",
    };
    let text = match &app.status {
        Some(s) => format!("{hint}   [{s}]"),
        None => hint.to_string(),
    };
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(title: &str, time: &str, repeat: &str, start: &str, end: &str, occ: &str) -> [String; 6] {
        [title.into(), time.into(), repeat.into(), start.into(), end.into(), occ.into()]
    }

    #[test]
    fn build_new_reminder_happy_path() {
        let f = fields("Study Go", "18:00", "mon,wed,fri", "2026-09-25", "", "");
        let new = build_new_reminder(&f).unwrap();
        assert_eq!(new.title, "Study Go");
        assert_eq!(new.time, "18:00");
        assert_eq!(new.start_date, "2026-09-25");
        assert_eq!(new.repeat_days, model::parse_weekday_list("mon,wed,fri").unwrap());
        assert_eq!(new.end_date, None);
        assert_eq!(new.occurrences_total, None);
    }

    #[test]
    fn build_new_reminder_defaults_start_to_today_when_blank() {
        let f = fields("Study Go", "18:00", "", "", "", "");
        let new = build_new_reminder(&f).unwrap();
        assert_eq!(new.start_date, Local::now().date_naive().to_string());
    }

    #[test]
    fn build_new_reminder_rejects_empty_title() {
        let f = fields("   ", "18:00", "", "2026-09-25", "", "");
        assert_eq!(build_new_reminder(&f).unwrap_err(), "title is required");
    }

    #[test]
    fn build_new_reminder_rejects_bad_time() {
        let f = fields("Study Go", "not-a-time", "", "2026-09-25", "", "");
        assert!(build_new_reminder(&f).is_err());
    }

    #[test]
    fn build_new_reminder_rejects_bad_weekday() {
        let f = fields("Study Go", "18:00", "funday", "2026-09-25", "", "");
        let err = build_new_reminder(&f).unwrap_err();
        assert!(err.contains("funday"));
    }

    #[test]
    fn build_new_reminder_rejects_non_numeric_occurrences() {
        let f = fields("Study Go", "18:00", "", "2026-09-25", "", "abc");
        let err = build_new_reminder(&f).unwrap_err();
        assert!(err.contains("abc"));
    }

    #[test]
    fn build_new_reminder_parses_optional_end_and_occurrences() {
        let f = fields("Study Go", "18:00", "mon", "2026-09-25", "2026-12-01", "10");
        let new = build_new_reminder(&f).unwrap();
        assert_eq!(new.end_date, Some("2026-12-01".to_string()));
        assert_eq!(new.occurrences_total, Some(10));
    }

    #[test]
    fn build_patch_blank_end_and_occurrences_clear_them() {
        let f = fields("Study Go", "18:00", "", "2026-09-25", "", "");
        let patch = build_patch(&f).unwrap();
        assert_eq!(patch.end_date, Some(None));
        assert_eq!(patch.occurrences_total, Some(None));
        assert_eq!(patch.enabled, None); // toggled elsewhere, never through the form
    }

    #[test]
    fn build_patch_filled_end_and_occurrences_set_them() {
        let f = fields("Study Go", "18:00", "", "2026-09-25", "2026-12-01", "5");
        let patch = build_patch(&f).unwrap();
        assert_eq!(patch.end_date, Some(Some("2026-12-01".to_string())));
        assert_eq!(patch.occurrences_total, Some(Some(5)));
    }

    #[test]
    fn build_patch_rejects_empty_title() {
        let f = fields("", "18:00", "", "2026-09-25", "", "");
        assert!(build_patch(&f).is_err());
    }

    #[test]
    fn format_schedule_one_shot() {
        let r = Reminder {
            id: 1,
            title: "x".into(),
            time: "18:00".into(),
            start_date: "2026-09-25".into(),
            repeat_days: 0,
            occurrences_total: None,
            end_date: None,
            enabled: true,
            created_at: "".into(),
            updated_at: "".into(),
        };
        assert_eq!(format_schedule(&r), "once, 2026-09-25");
    }

    #[test]
    fn format_schedule_repeating() {
        let r = Reminder {
            id: 1,
            title: "x".into(),
            time: "18:00".into(),
            start_date: "2026-09-21".into(),
            repeat_days: model::parse_weekday_list("mon,wed,fri").unwrap(),
            occurrences_total: None,
            end_date: None,
            enabled: true,
            created_at: "".into(),
            updated_at: "".into(),
        };
        assert_eq!(format_schedule(&r), "mon,wed,fri, from 2026-09-21");
    }

    #[test]
    fn form_state_new_edit_prefills_from_reminder() {
        let r = Reminder {
            id: 7,
            title: "Exercise".into(),
            time: "20:00".into(),
            start_date: "2026-09-21".into(),
            repeat_days: model::parse_weekday_list("mon,wed,fri").unwrap(),
            occurrences_total: Some(10),
            end_date: Some("2026-12-01".into()),
            enabled: true,
            created_at: "".into(),
            updated_at: "".into(),
        };
        let form = FormState::new_edit(&r);
        assert_eq!(form.editing_id, Some(7));
        assert_eq!(form.fields[TITLE], "Exercise");
        assert_eq!(form.fields[TIME], "20:00");
        assert_eq!(form.fields[REPEAT], "mon,wed,fri");
        assert_eq!(form.fields[START], "2026-09-21");
        assert_eq!(form.fields[END], "2026-12-01");
        assert_eq!(form.fields[OCCURRENCES], "10");
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("short", 28), "short");
    }

    #[test]
    fn truncate_shortens_long_strings_with_ellipsis() {
        let long = "a".repeat(40);
        let truncated = truncate(&long, 10);
        assert_eq!(truncated.chars().count(), 10);
        assert!(truncated.ends_with('…'));
    }
}

//! Drawing of the block/habit form (`editor.rs`): the fields on the left, the
//! TOML it saves on the right, the app/site picker over it.

use super::app::App;
use super::editor::{rule_from_text, scalar, site_of_pattern, toml_inline, Editor, Kind, Options, Row, Suggest};
use super::ui::pane_block;
use habit_core::duration::format_duration;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

const LABEL: usize = 16;

fn dim(text: impl Into<String>) -> Span<'static> {
    text.into().fg(Color::DarkGray)
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App, editor: &Editor) {
    let (form, preview) = if area.width >= 100 {
        let [a, b] = Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(area);
        (a, Some(b))
    } else {
        (area, None)
    };
    draw_form(frame, form, app, editor);
    if let Some(preview) = preview {
        draw_preview(frame, preview, editor);
    }
    if editor.picker.is_some() {
        draw_picker(frame, form, editor);
    }
    if editor.confirm_discard {
        let lines = vec![
            Line::from(" Discard your changes?"),
            Line::from(vec![" y".bold(), dim(" discard · any other key keeps editing")]),
        ];
        let [area] = Layout::horizontal([Constraint::Length(44)]).flex(Flex::Center).areas(form);
        let [area] = Layout::vertical([Constraint::Length(4)]).flex(Flex::Center).areas(area);
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(lines).block(Block::bordered().border_style(Color::Red)), area);
    }
}

/// An allow rule in a few words: its app, site or patterns, and the
/// terminal program and tmux session it needs.
fn describe_rule(editor: &Editor, rule: &serde_json::Value) -> String {
    let part = |key: &str| rule.get(key).and_then(|v| v.as_str());
    let mut parts: Vec<String> = part("app").map(|app| editor.app_with_name(app)).into_iter().collect();
    if let Some(pattern) = part("title") {
        parts.push(format!("title ~ {pattern}"));
    }
    match part("url").map(|url| (url, site_of_pattern(url))) {
        Some((_, Some(site))) => parts.push(format!("site {site}")),
        Some((url, None)) => parts.push(format!("url ~ {url}")),
        None => {}
    }
    if let Some(program) = part("program") {
        parts.push(format!("running {program}"));
    }
    if let Some(session) = part("tmux_session") {
        parts.push(format!("in tmux {session}"));
    }
    parts.join(" ")
}

/// The recent use of an app or site, as "1:25/day".
fn per_day(editor: &Editor, suggest: Suggest, value: &str) -> Option<String> {
    let keys = match suggest {
        Suggest::Apps => vec![value.to_string()],
        Suggest::Sites => vec![format!("site:{value}"), format!("site:www.{value}")],
    };
    let ms: u64 = editor.context.recent.iter().filter(|(k, _)| keys.contains(k)).map(|(_, ms)| ms).sum();
    (ms > 0).then(|| format!("{}/day", format_duration(ms)))
}

fn row_line(editor: &Editor, row: &Row, first_of_field: bool, selected: bool) -> Line<'static> {
    let field = editor.field_of(row);
    let label = match (row, field) {
        (Row::Id, _) => "Id",
        (_, Some(f)) if first_of_field => f.label,
        _ => "",
    };
    let mut spans = vec![dim(format!(" {label:<LABEL$}"))];
    let typing = editor.input.as_ref().filter(|_| selected);
    match (row, field) {
        (Row::Id, _) => match typing {
            Some(input) => spans.extend([input.clone().bold(), "█".fg(Color::Cyan)]),
            None if editor.id.is_empty() => spans.push(dim("from the name")),
            None => spans.push(editor.id.clone().into()),
        },
        (Row::Field(_), Some(f)) => match f.kind {
            _ if typing.is_some() => spans.extend([typing.cloned().unwrap_or_default().bold(), "█".fg(Color::Cyan)]),
            Kind::Choice(choices) => {
                let current = editor.value(f.key).and_then(|v| v.as_str()).unwrap_or(choices[0].0);
                let label = choices.iter().find(|(v, _)| *v == current).map_or(current, |(_, l)| l);
                spans.extend([dim("◀ "), label.to_string().bold(), dim(" ▶")]);
            }
            Kind::ReadOnly => {
                let text = match f.key {
                    "schedule" if editor.value("schedule").is_none() => "all day, every day".to_string(),
                    "schedule" if !editor.context.schedule.is_empty() => editor.context.schedule.join("; "),
                    key => editor.value(key).map(toml_inline).unwrap_or_default(),
                };
                spans.extend([text.into(), dim("  (edit in config.toml)")]);
            }
            _ => match scalar(editor.value(f.key)) {
                text if text.is_empty() => spans.push(dim("—")),
                text => spans.push(text.bold()),
            },
        },
        (Row::Item { index, .. }, Some(f)) if f.kind == Kind::Rules => {
            let rules = editor.value(f.key).and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let rule = rules.get(*index).cloned().unwrap_or_default();
            spans.extend(["✓ ".fg(Color::Green), describe_rule(editor, &rule).into()]);
        }
        (Row::Item { index, .. }, Some(f)) => {
            let items = editor.list(f.key);
            let value = items.get(*index).cloned().unwrap_or_default();
            let shown = if matches!(f.kind, Kind::List(Suggest::Apps)) { editor.app_with_name(&value) } else { value.clone() };
            spans.extend(["✓ ".fg(Color::Green), shown.into()]);
            if let Kind::List(suggest) = f.kind {
                if let Some(use_) = per_day(editor, suggest, &value) {
                    spans.push(dim(format!("  {use_}")));
                }
            }
        }
        (Row::Add { .. }, Some(f)) if f.kind == Kind::Rules => {
            spans.push(dim("+ add (enter or a)"));
            let empty = editor.value(f.key).is_none();
            match editor.habit_kind() {
                "passive" if empty => spans.push("  needs at least one: what counts".fg(Color::Yellow)),
                _ if empty => spans.push(dim("  none: any window counts")),
                _ => spans.push(dim("  any of them counts")),
            }
        }
        (Row::Add { .. }, _) => spans.push(dim("+ add (enter or a)")),
        (Row::Option { id, .. }, Some(f)) => {
            let chosen = editor.list(f.key).contains(id);
            let options = match f.kind {
                Kind::Multi(Options::Groups) => &editor.context.groups,
                _ => &editor.context.habits,
            };
            let name = options.iter().find(|(o, _)| o == id).map_or(id.as_str(), |(_, n)| n.as_str());
            spans.push(if chosen { "[x] ".fg(Color::Green) } else { dim("[ ] ") });
            spans.push(name.to_string().into());
        }
        _ => {}
    }
    let line = Line::from(spans);
    if selected && typing.is_none() { line.add_modifier(Modifier::REVERSED) } else { line }
}

fn draw_form(frame: &mut Frame, area: Rect, app: &App, editor: &Editor) {
    let name = scalar(editor.value("name"));
    let title = match (editor.creating, name.is_empty()) {
        (true, _) => format!(" New {} ", editor.section.noun()),
        (false, true) => format!(" Edit {} · {} ", editor.section.noun(), editor.id),
        (false, false) => format!(" Edit {} · {name} ", editor.section.noun()),
    };
    let block = Block::bordered().title(title.bold()).border_style(Color::Cyan);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut notes = Vec::new();
    if let Some(error) = &editor.error {
        notes.push(Line::from(format!(" {error}")).fg(Color::Red));
    } else if editor.saving {
        notes.push(Line::from(dim(" Saving…")));
    }
    if app.snapshot.as_ref().is_some_and(|s| s.lock.is_some()) {
        notes.push(Line::from(" Committed: changes that make things easier are refused until the lock ends.").fg(Color::Magenta));
    }
    // Room for the notes as they wrap, plus a blank line above them.
    let width = inner.width.max(1) as usize;
    let note_rows: usize = notes.iter().map(|n| n.width().div_ceil(width).max(1)).sum();
    let [list, _, bottom] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(note_rows as u16)]).areas(inner);

    let rows = editor.rows();
    let mut previous_field = None;
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let field = match row {
                Row::Field(f) | Row::Item { field: f, .. } | Row::Add { field: f } | Row::Option { field: f, .. } => Some(*f),
                Row::Id => None,
            };
            let first = field != previous_field;
            previous_field = field;
            row_line(editor, row, first, i == editor.cursor)
        })
        .collect();
    // Keep the cursor in view.
    let height = list.height as usize;
    let skip = editor.cursor.saturating_sub(height.saturating_sub(2));
    frame.render_widget(Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>()), list);
    frame.render_widget(Paragraph::new(notes).wrap(Wrap { trim: false }), bottom);
}

fn draw_preview(frame: &mut Frame, area: Rect, editor: &Editor) {
    let id = if editor.id.is_empty() { "…" } else { editor.id.as_str() };
    let mut lines = vec![Line::from(format!("[{}.{id}]", editor.section.key()).fg(Color::Cyan))];
    for (key, value) in &editor.table {
        lines.push(Line::from(vec![key.clone().fg(Color::Yellow), " = ".into(), toml_inline(value).into()]));
    }
    let hint = if editor.dirty { " unsaved · ctrl+s writes it to config.toml" } else { " as in config.toml" };
    lines.push(Line::from(""));
    lines.push(Line::from(dim(hint)));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(pane_block("config.toml")), area);
}

fn draw_picker(frame: &mut Frame, area: Rect, editor: &Editor) {
    let Some(picker) = &editor.picker else { return };
    let options = editor.suggestions(picker);
    let kind = editor.fields().get(picker.field).map(|f| f.kind);
    let rules = kind == Some(Kind::Rules);
    let what = match kind {
        Some(Kind::List(Suggest::Sites)) => "site",
        Some(Kind::Rules) => "where it counts",
        _ => "app",
    };
    let mut lines = vec![
        Line::from(vec![" Find or type: ".into(), picker.filter.clone().bold(), "█".fg(Color::Cyan)]),
        Line::from(""),
    ];
    if rules {
        lines.insert(1, Line::from(dim(" an app id, site:github.com, term:nvim, tmux:thesis; words combine")));
    }
    if options.is_empty() {
        lines.push(Line::from(dim(if picker.filter.is_empty() {
            " Nothing used recently; type an id".to_string()
        } else {
            format!(" enter adds {:?}", picker.filter)
        })));
    }
    for (i, (value, per_day)) in options.iter().take(10).enumerate() {
        let label = match rule_from_text(value) {
            Ok(rule) if rules => describe_rule(editor, &serde_json::Value::Object(rule)),
            _ => editor.app_with_name(value),
        };
        let use_ = match (value.starts_with("tmux:"), *per_day) {
            (true, _) => "tmux session".to_string(),
            (false, 0) => String::new(),
            (false, ms) => format!("{}/day", format_duration(ms)),
        };
        let line = Line::from(vec![format!(" {label:<40}").into(), dim(use_)]);
        lines.push(if i == picker.index { line.add_modifier(Modifier::REVERSED) } else { line });
    }
    lines.push(Line::from(""));
    lines.push(Line::from(dim(" ↑/↓ choose · enter add · esc cancel")));
    let height = (lines.len() as u16 + 2).min(area.height);
    let [popup] = Layout::horizontal([Constraint::Length(64.min(area.width))]).flex(Flex::Center).areas(area);
    let [popup] = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center).areas(popup);
    frame.render_widget(Clear, popup);
    let title = format!(" Add {what} ");
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(title).border_style(Color::Cyan)), popup);
}

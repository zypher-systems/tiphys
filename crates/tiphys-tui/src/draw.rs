//! Drawing a [`View`].
//!
//! Drawing reads the view and writes cells; it changes nothing. Colours are
//! the terminal's own sixteen, so the app looks right in any theme, and
//! nothing depends on colour alone: a failure has a cross as well as red.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tiphys_core::llm::Model;
use tiphys_core::spend::dollars;
use unicode_width::UnicodeWidthStr;

use crate::input::Input;
use crate::view::{
    Activity, Card, Chat, Field, Item, Picker, Screen, Setup, Status, ToolState, View,
};
use crate::wrap::{fit, wrap};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// The widest a form is drawn, so it reads as a column on a wide terminal.
const FORM_WIDTH: u16 = 76;

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn bad() -> Style {
    Style::new().fg(Color::Red)
}

pub fn draw(view: &View, frame: &mut Frame) {
    let area = frame.area();
    match &view.screen {
        Screen::Loading => {
            frame.render_widget(Paragraph::new(Line::styled(" Starting…", dim())), area);
        }
        Screen::Setup(form) => draw_setup(form, view.tick, frame, column(area)),
        Screen::Models(picker) => draw_models(picker, view.tick, frame, column(area)),
        Screen::Chat => draw_chat(view, frame, area),
    }
}

/// A column for a form: the left of the screen, with a margin.
fn column(area: Rect) -> Rect {
    Rect {
        x: area.x + 1.min(area.width),
        y: area.y,
        width: area.width.saturating_sub(2).min(FORM_WIDTH),
        height: area.height,
    }
}

fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick as usize) % SPINNER.len()]
}

/// The line under a form that says how the last request is going.
fn status_lines(status: &Status, tick: u64, width: usize) -> Vec<Line<'static>> {
    match status {
        Status::Idle => vec![Line::default()],
        Status::Working(what) => {
            vec![Line::from(vec![
                Span::styled(format!("{} ", spinner(tick)), accent()),
                Span::raw(what.clone()),
            ])]
        }
        Status::Failed(why) => wrap(&format!("✗ {why}"), width)
            .into_iter()
            .map(|line| Line::styled(line, bad()))
            .collect(),
    }
}

fn draw_setup(form: &Setup, tick: u64, frame: &mut Frame, area: Rect) {
    let width = area.width as usize;
    let mut lines = vec![
        Line::default(),
        Line::styled(
            format!("Tiphys {}", tiphys_core::VERSION),
            accent().add_modifier(Modifier::BOLD),
        ),
        Line::default(),
        Line::styled("Set up a connection", bold()),
        Line::default(),
    ];
    let intro = "A connection is a model endpoint that speaks Chat Completions: OpenRouter, \
                 OpenAI, or a server of your own.";
    lines.extend(wrap(intro, width).into_iter().map(Line::raw));
    lines.push(Line::default());

    let label_width = 10;
    let field_width = width.saturating_sub(label_width + 4);
    let mut cursor = None;
    let fields: [(Field, &str, Option<&Input>); 4] = [
        (Field::Name, "Name", Some(&form.name)),
        (Field::Address, "Address", Some(&form.address)),
        (Field::Key, "Key", Some(&form.key)),
        (Field::Local, "Local", None),
    ];
    for (field, label, input) in fields {
        let focused = form.focus == field;
        let marker = if focused { "› " } else { "  " };
        let mut spans = vec![
            Span::styled(marker, accent()),
            Span::styled(
                format!("{label:<label_width$}"),
                if focused { bold() } else { Style::new() },
            ),
        ];
        match input {
            Some(input) => {
                let (shown, column) = input.visible(field_width);
                if focused {
                    cursor = Some((label_width + 2 + column, lines.len()));
                }
                let hint = match field {
                    Field::Key if input.is_empty() && form.has_key => {
                        "leave empty to keep the stored key"
                    }
                    Field::Key if input.is_empty() && form.local => "not needed for a local server",
                    _ => "",
                };
                if shown.is_empty() && !hint.is_empty() {
                    spans.push(Span::styled(hint, dim()));
                } else {
                    spans.push(Span::raw(shown));
                }
            }
            None => {
                let tick_box = if form.local { "[x]" } else { "[ ]" };
                spans.push(Span::raw(format!(
                    "{tick_box} runs on my own hardware: no cost, and longer to answer"
                )));
                if focused {
                    cursor = Some((label_width + 3, lines.len()));
                }
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    let promise = "The key is stored on this machine, in a file only Tiphys can read. It is not \
                   shown again and is never passed on a command line.";
    lines.extend(
        wrap(promise, width)
            .into_iter()
            .map(|line| Line::styled(line, dim())),
    );
    lines.push(Line::default());
    lines.extend(status_lines(&form.status, tick, width));
    lines.push(Line::default());
    let keys = if form.can_leave {
        "tab next field · space tick · enter continue · esc back · ctrl+c quit"
    } else {
        "tab next field · space tick · enter continue · ctrl+c quit"
    };
    lines.push(Line::styled(fit(keys, width), dim()));

    frame.render_widget(Paragraph::new(lines), area);
    if let Some((x, y)) = cursor
        && !matches!(form.status, Status::Working(_))
    {
        place_cursor(frame, area, x, y);
    }
}

fn place_cursor(frame: &mut Frame, area: Rect, x: usize, y: usize) {
    let (x, y) = (area.x + x as u16, area.y + y as u16);
    if x < area.right() && y < area.bottom() {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// A model's price for a list: dollars per million tokens in and out.
fn price(model: &Model) -> String {
    match model.rates {
        Some(rates) if rates.input == 0.0 && rates.output == 0.0 => "free".into(),
        Some(rates) => format!(
            "{} in · {} out",
            dollars(Some(rates.input)),
            dollars(Some(rates.output))
        ),
        None => dollars(None),
    }
}

fn context(model: &Model) -> String {
    match model.context {
        Some(tokens) if tokens >= 1_000_000 => format!("{:.1}M", tokens as f64 / 1e6),
        Some(tokens) if tokens >= 1000 => format!("{}k", tokens / 1000),
        Some(tokens) => tokens.to_string(),
        None => String::new(),
    }
}

fn draw_models(picker: &Picker, tick: u64, frame: &mut Frame, area: Rect) {
    let width = area.width as usize;
    let matching = picker.matching();
    let count = match (matching.len(), picker.models.len()) {
        (_, 0) => "this connection lists no models".to_string(),
        (shown, all) if shown == all => format!("{all} models, priced per million tokens"),
        (shown, all) => format!("{shown} of {all} models, priced per million tokens"),
    };
    let title = format!("Choose a model for {}", picker.draft.name);
    let gap = width.saturating_sub(title.width() + count.width());
    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(title, bold()),
            Span::raw(" ".repeat(gap)),
            Span::styled(count, dim()),
        ]),
        Line::default(),
    ];
    let (shown, column) = picker.filter.visible(width.saturating_sub(10));
    let filter_row = lines.len();
    lines.push(Line::from(vec![
        Span::styled("Filter  ", dim()),
        if shown.is_empty() && picker.models.is_empty() {
            Span::styled("type the model's id", dim())
        } else {
            Span::raw(shown)
        },
    ]));
    lines.push(Line::default());

    // The status and the key hints take the last lines; the list gets the rest.
    let status = status_lines(&picker.status, tick, width);
    let rows = (area.height as usize).saturating_sub(lines.len() + status.len() + 3);
    let first = picker.selected.saturating_sub(rows.saturating_sub(1));
    let id_width = width.saturating_sub(36).max(12);
    for (index, model) in matching.iter().enumerate().skip(first).take(rows) {
        let selected = index == picker.selected;
        let row = format!(
            "{} {:<id_width$}  {:>6}  {}",
            if selected { "›" } else { " " },
            fit(&model.id, id_width),
            context(model),
            price(model),
        );
        let style = if selected {
            accent().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        lines.push(Line::styled(fit(&row, width), style));
    }
    if matching.is_empty() && !picker.filter.is_empty() {
        let typed = picker.filter.text().trim();
        lines.push(Line::styled(
            fit(
                &format!("No model matches. Enter uses \"{typed}\" as the model's id."),
                width,
            ),
            dim(),
        ));
    }
    while lines.len() + status.len() + 2 < area.height as usize {
        lines.push(Line::default());
    }
    lines.extend(status);
    lines.push(Line::default());
    lines.push(Line::styled(
        fit(
            "↑↓ move · type to filter · enter choose and check · esc back",
            width,
        ),
        dim(),
    ));

    frame.render_widget(Paragraph::new(lines), area);
    if !matches!(picker.status, Status::Working(_)) {
        place_cursor(frame, area, 8 + column, filter_row);
    }
}

/// The conversation as styled lines that fit `width`.
fn transcript(chat: &Chat, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let push =
        |lines: &mut Vec<Line<'static>>, text: &str, first: &str, rest: &str, style: Style| {
            let room = width.saturating_sub(first.width()).max(1);
            for (index, line) in wrap(text, room).into_iter().enumerate() {
                let prefix = if index == 0 { first } else { rest };
                lines.push(Line::styled(format!("{prefix}{line}"), style));
            }
        };
    for item in &chat.items {
        match item {
            Item::User(text) => {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                push(&mut lines, text, "› ", "  ", bold());
                lines.push(Line::default());
            }
            Item::Assistant(text) => push(&mut lines, text, "", "", Style::new()),
            Item::Tool {
                summary,
                reason,
                state,
                ..
            } => {
                let mark = match state {
                    ToolState::Running => "…",
                    ToolState::Done => "✓",
                    ToolState::Failed(_) => "✗",
                };
                let what = if reason.is_empty() {
                    format!("{summary} {mark}")
                } else {
                    format!("{summary}  ({reason}) {mark}")
                };
                push(&mut lines, &what, "  · ", "    ", dim());
                if let ToolState::Failed(why) = state {
                    push(&mut lines, why, "    ", "    ", bad());
                }
            }
            Item::Notice(text) => push(&mut lines, text, "— ", "  ", dim()),
            Item::Error(text) => push(&mut lines, text, "✗ ", "  ", bad()),
        }
    }
    if !chat.streaming.is_empty() {
        push(&mut lines, &chat.streaming, "", "", Style::new());
    }
    lines
}

fn asking() -> Style {
    Style::new().fg(Color::Yellow)
}

/// The lines of an approval card, at most `rows` of them. What is being
/// asked always fits; the preview is what gives way.
fn card_lines(card: &Card, width: usize, rows: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(2).max(1);
    let bar = || Span::styled("▌ ", asking());
    let mut lines = vec![Line::from(vec![
        bar(),
        Span::styled(
            "Tiphys asks before it does this",
            asking().add_modifier(Modifier::BOLD),
        ),
    ])];
    for line in wrap(&card.summary, room) {
        lines.push(Line::from(vec![bar(), Span::styled(line, bold())]));
    }
    if !card.reason.is_empty() {
        for line in wrap(&format!("Its reason: {}", card.reason), room) {
            lines.push(Line::from(vec![bar(), Span::raw(line)]));
        }
    }
    if !card.why.is_empty() {
        for line in wrap(&format!("It asks because {}.", card.why), room) {
            lines.push(Line::from(vec![bar(), Span::styled(line, dim())]));
        }
    }
    let keys = Line::from(vec![bar(), Span::styled("y approve · n deny", asking())]);
    if let Some(preview) = &card.preview {
        // The two header lines of a diff say nothing the summary has not.
        let preview: Vec<&str> = preview
            .lines()
            .filter(|line| !line.starts_with("--- ") && !line.starts_with("+++ "))
            .collect();
        // One line is kept for the keys, and one to say what was left out.
        let space = rows.saturating_sub(lines.len() + 2);
        let shown = if preview.len() > space {
            space.saturating_sub(1)
        } else {
            preview.len()
        };
        if shown > 0 {
            lines.push(Line::from(vec![bar()]));
        }
        for line in &preview[..shown] {
            let style = match line.chars().next() {
                Some('+') => Style::new().fg(Color::Green),
                Some('-') => bad(),
                _ => dim(),
            };
            lines.push(Line::from(vec![
                bar(),
                Span::styled(fit(line, room), style),
            ]));
        }
        if shown < preview.len() {
            let more = format!("… {} more lines", preview.len() - shown);
            lines.push(Line::from(vec![bar(), Span::styled(more, dim())]));
        }
    }
    lines.truncate(rows.saturating_sub(1));
    lines.push(keys);
    lines
}

fn draw_chat(view: &View, frame: &mut Frame, area: Rect) {
    let chat = &view.chat;
    let [header, body, status, input] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let width = area.width as usize;

    // Header: where prompts go, and what this run has cost.
    let session = view.state.session.as_ref();
    let default = view
        .state
        .default
        .as_ref()
        .and_then(|name| view.state.connections.iter().find(|c| &c.name == name));
    let place = match (session, default) {
        (Some(session), _) => format!("{} · {}", session.connection, session.model),
        (None, Some(default)) => format!(
            "{} · {}",
            default.name,
            default.model.as_deref().unwrap_or("no model chosen")
        ),
        (None, None) => "no connection".into(),
    };
    let cost = if chat.unpriced == 0 {
        dollars(Some(chat.cost))
    } else if chat.cost == 0.0 {
        dollars(None)
    } else {
        format!("{} + {} unknown", dollars(Some(chat.cost)), chat.unpriced)
    };
    let left = format!(" Tiphys · {place}");
    let left = fit(&left, width.saturating_sub(cost.width() + 2));
    let gap = width.saturating_sub(left.width() + cost.width() + 1);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, accent()),
            Span::raw(" ".repeat(gap)),
            Span::styled(cost, dim()),
        ])),
        header,
    );

    // An approval card takes as much of the body as it needs, leaving a
    // couple of lines of the conversation: the owner has to be able to read
    // what they are agreeing to.
    let card = chat
        .card
        .as_ref()
        .map(|card| {
            card_lines(
                card,
                width.saturating_sub(2),
                (body.height as usize).saturating_sub(2).max(2),
            )
        })
        .unwrap_or_default();
    let [body, asked] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(card.len() as u16)]).areas(body);
    let asked = Rect {
        x: asked.x + 1,
        width: asked.width.saturating_sub(2),
        ..asked
    };
    frame.render_widget(Paragraph::new(card), asked);

    // Body: the newest lines that fit, moved up by the scroll.
    let lines = transcript(chat, width.saturating_sub(2));
    let rows = body.height as usize;
    let scroll = chat.scroll.min(lines.len().saturating_sub(rows));
    let end = lines.len() - scroll;
    let start = end.saturating_sub(rows);
    let mut visible: Vec<Line> = lines[start..end].to_vec();
    if lines.is_empty() {
        visible = vec![
            Line::default(),
            Line::styled(
                "Ask for something. Tiphys can read files and look around this machine.",
                dim(),
            ),
            Line::styled("/help lists the commands.", dim()),
        ];
    }
    let inner = Rect {
        x: body.x + 1,
        width: body.width.saturating_sub(2),
        ..body
    };
    frame.render_widget(Paragraph::new(visible), inner);

    // Status: what is happening, or what the keys do.
    let status_line = if chat.card.is_some() {
        Line::from(vec![
            Span::styled(format!(" {} ", spinner(view.tick)), asking()),
            Span::raw("waiting for your answer"),
            Span::styled("  ctrl+c stops the turn", dim()),
        ])
    } else if chat.busy {
        let what = match &chat.activity {
            Activity::Idle | Activity::Waiting => "waiting for the model".to_string(),
            Activity::Thinking => "thinking".to_string(),
            Activity::Answering => "answering".to_string(),
            Activity::Tool(summary) => summary.clone(),
        };
        Line::from(vec![
            Span::styled(format!(" {} ", spinner(view.tick)), accent()),
            Span::raw(fit(&what, width.saturating_sub(20))),
            Span::styled("  esc to stop", dim()),
        ])
    } else if scroll > 0 {
        Line::styled(format!(" {scroll} lines below · pgdn to return"), dim())
    } else {
        Line::styled(
            " enter send · pgup/pgdn scroll · /help commands · ctrl+c quit",
            dim(),
        )
    };
    frame.render_widget(Paragraph::new(status_line), status);

    let (shown, column) = chat.input.visible(width.saturating_sub(4));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" › ", accent()),
            Span::raw(shown),
        ])),
        input,
    );
    place_cursor(frame, input, 3 + column, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{apply, paste};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tiphys_core::proto::{ConnectionInfo, Event, SessionInfo, State, StopReason};
    use tiphys_core::spend::Rates;

    /// Draws a view and returns the screen as text, with the cursor's place.
    fn render(view: &View, width: u16, height: u16) -> (String, (u16, u16)) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw(view, frame);
            })
            .unwrap();
        let position = terminal.get_cursor_position().unwrap();
        let cursor = (position.x, position.y);
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                row.push_str(buffer[(x, y)].symbol());
            }
            text.push_str(row.trim_end());
            text.push('\n');
        }
        (text, cursor)
    }

    /// Compares a screen with the one kept in `snapshots/`. Run the tests with
    /// `UPDATE_SNAPSHOTS=1` to keep what is drawn now.
    fn check(name: &str, view: &View) {
        let (screen, _) = render(view, 80, 20);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("snapshots")
            .join(format!("{name}.txt"));
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::write(&path, &screen).unwrap();
        }
        let kept = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            kept == screen,
            "{name} is drawn differently now:\n{screen}\nkept:\n{kept}"
        );
    }

    fn state(session: bool) -> State {
        State {
            connections: vec![ConnectionInfo {
                name: "openrouter".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                model: Some("vendor/model".into()),
                local: false,
                has_key: true,
            }],
            default: Some("openrouter".into()),
            session: session.then(|| SessionInfo {
                id: "0199".into(),
                connection: "openrouter".into(),
                model: "vendor/model".into(),
            }),
        }
    }

    fn models() -> Vec<Model> {
        let model = |id: &str, context, rates| Model {
            id: id.into(),
            context,
            rates,
            tools: None,
        };
        let rates = |input, output| {
            Some(Rates {
                input,
                output,
                cache_read: None,
                cache_write: None,
            })
        };
        vec![
            model("vendor/large", Some(1_000_000), rates(3.0, 15.0)),
            model("vendor/small", Some(200_000), rates(0.25, 1.25)),
            model("vendor/free", Some(32_768), rates(0.0, 0.0)),
            model("local/unpriced", None, None),
        ]
    }

    fn first_run() -> View {
        let mut view = View::default();
        apply(&mut view, &Event::State(State::default()));
        view
    }

    fn chatting() -> View {
        let mut view = View::default();
        apply(&mut view, &Event::State(state(true)));
        for event in [
            Event::UserMessage {
                text: "how full is the root disk?".into(),
            },
            Event::AssistantMessage {
                text: "Let me look.".into(),
            },
            Event::ToolStarted {
                id: "a".into(),
                tool: "read_file".into(),
                summary: "read /proc/mounts".into(),
                reason: "to find the root disk".into(),
            },
            Event::ToolFinished {
                id: "a".into(),
                ok: true,
                output: String::new(),
            },
            Event::ToolStarted {
                id: "b".into(),
                tool: "read_file".into(),
                summary: "read /nope".into(),
                reason: String::new(),
            },
            Event::ToolFinished {
                id: "b".into(),
                ok: false,
                output: "/nope: No such file or directory".into(),
            },
            Event::Spend {
                cost: Some(0.0421),
                usage: None,
            },
        ] {
            apply(&mut view, &event);
        }
        view
    }

    #[test]
    fn the_first_run_screen() {
        check("setup_first_run", &first_run());
    }

    #[test]
    fn a_typed_key_is_drawn_as_dots_and_never_as_itself() {
        let mut view = first_run();
        paste(&mut view, "sk-live-secret-123");
        let (screen, cursor) = render(&view, 80, 20);
        assert!(!screen.contains("sk-live"), "{screen}");
        assert!(screen.contains("Key       ••••••••••••••••••"), "{screen}");
        // The cursor sits after the last dot, on the key's row.
        let row = screen
            .lines()
            .position(|line| line.contains("Key   "))
            .unwrap() as u16;
        assert_eq!(cursor, (1 + 12 + 18, row));
        check("setup_key_typed", &view);
    }

    #[test]
    fn a_setup_failure_is_drawn_under_the_form() {
        let mut view = first_run();
        apply(
            &mut view,
            &Event::Failed {
                message: "the provider refused the key (401): No auth credentials found".into(),
            },
        );
        check("setup_failed", &view);
    }

    #[test]
    fn the_model_list() {
        let mut view = first_run();
        paste(&mut view, "sk-live-secret-123");
        apply(&mut view, &Event::Models { models: models() });
        check("models", &view);
        let (screen, _) = render(&view, 80, 20);
        assert!(!screen.contains("sk-live"));
    }

    #[test]
    fn a_conversation_with_tools_and_a_failure() {
        let mut view = chatting();
        check("chat_busy", &view);
        apply(
            &mut view,
            &Event::AssistantMessage {
                text: "The root disk is 42% full.".into(),
            },
        );
        apply(
            &mut view,
            &Event::TurnFinished {
                reason: StopReason::Completed,
                error: None,
            },
        );
        check("chat_idle", &view);
    }

    #[test]
    fn an_action_that_asks_is_drawn_as_a_card_over_the_conversation() {
        let mut view = chatting();
        let diff = "--- before\n+++ after\n@@ -1,2 +1,3 @@\n 127.0.0.1 localhost\n-10.0.0.4 db\n+10.0.0.5 db\n+10.0.0.6 cache";
        apply(
            &mut view,
            &Event::ApprovalRequested {
                id: "w1".into(),
                tool: "edit_file".into(),
                summary: "edit /etc/hosts (3 lines)".into(),
                reason: "to point db at the new address".into(),
                why: "/etc/hosts is outside Tiphys's own home".into(),
                class: tiphys_core::policy::Class::System,
                preview: Some(diff.into()),
            },
        );
        check("chat_asking", &view);

        // A long preview gives way; the question and the keys never do.
        let long: String = (0..200).map(|n| format!("+line {n}\n")).collect();
        view.chat.card.as_mut().unwrap().preview = Some(long);
        let (screen, _) = render(&view, 80, 20);
        assert!(screen.contains("edit /etc/hosts (3 lines)"), "{screen}");
        assert!(screen.contains("y approve · n deny"), "{screen}");
        assert!(screen.contains("more lines"), "{screen}");
    }

    #[test]
    fn an_empty_conversation_says_what_to_do() {
        let mut view = View::default();
        apply(&mut view, &Event::State(state(false)));
        check("chat_empty", &view);
    }

    #[test]
    fn every_screen_survives_a_terminal_too_small_for_it() {
        let mut picking = first_run();
        apply(&mut picking, &Event::Models { models: models() });
        for view in [View::default(), first_run(), picking, chatting()] {
            for (width, height) in [(1, 1), (10, 3), (20, 5), (40, 8), (200, 60)] {
                render(&view, width, height);
            }
        }
    }

    #[test]
    fn a_long_conversation_shows_its_end_and_scrolls_back() {
        let mut view = View::default();
        apply(&mut view, &Event::State(state(true)));
        for n in 1..=40 {
            apply(
                &mut view,
                &Event::AssistantMessage {
                    text: format!("line {n}"),
                },
            );
        }
        let (screen, _) = render(&view, 40, 10);
        assert!(
            screen.contains("line 40") && !screen.contains("line 30\n"),
            "{screen}"
        );

        view.chat.scroll = 10;
        let (screen, _) = render(&view, 40, 10);
        assert!(
            screen.contains("line 30") && !screen.contains("line 40"),
            "{screen}"
        );
        assert!(screen.contains("10 lines below"), "{screen}");

        // Scrolling past the start stops at the start.
        view.chat.scroll = 10_000;
        let (screen, _) = render(&view, 40, 10);
        assert!(screen.contains("line 1\n"), "{screen}");
    }
}

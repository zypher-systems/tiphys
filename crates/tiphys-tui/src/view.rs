//! What the app is showing, and how it changes.
//!
//! [`View`] is plain data. Two functions change it: [`apply`] for an event
//! from the host and [`key`] for a key from the owner. Each returns the
//! requests to send as a result. Neither touches the terminal, the network
//! or the disk, so every screen can be put in any state and checked in a
//! test.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tiphys_core::config::{Connection, valid_name};
use tiphys_core::keys::Secret;
use tiphys_core::llm::Model;
use tiphys_core::proto::{Draft, Event, Request, State, StopReason};

use crate::input::Input;

const FIRST_NAME: &str = "openrouter";
const FIRST_ADDRESS: &str = "https://openrouter.ai/api/v1";

const HELP: &str = "/new starts a fresh session · /model picks another model · \
/connections sets up or changes a connection · /quit leaves";

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub screen: Screen,
    /// Where things stand, as the host last said.
    pub state: State,
    pub chat: Chat,
    pub quit: bool,
    /// Counts up while the app runs; drives the spinner.
    pub tick: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    /// Waiting for the host to say where things stand.
    Loading,
    Setup(Setup),
    Models(Box<Picker>),
    Chat,
}

/// What a form is doing about the last thing asked of it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Idle,
    Working(String),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Address,
    Key,
    Local,
}

const FIELDS: [Field; 4] = [Field::Name, Field::Address, Field::Key, Field::Local];

/// The form a connection is set up in.
#[derive(Debug, Clone, PartialEq)]
pub struct Setup {
    pub name: Input,
    pub address: Input,
    pub key: Input,
    pub local: bool,
    pub focus: Field,
    pub status: Status,
    /// Whether there is a conversation to go back to.
    pub can_leave: bool,
    /// Whether a key is already stored under the name the form opened with.
    pub has_key: bool,
}

impl Setup {
    fn first_run() -> Self {
        Self {
            name: Input::new(FIRST_NAME),
            address: Input::new(FIRST_ADDRESS),
            key: Input::masked(),
            local: false,
            focus: Field::Key,
            status: Status::Idle,
            can_leave: false,
            has_key: false,
        }
    }

    /// The form for changing the connection in use, or adding another.
    fn from_state(state: &State) -> Self {
        let current = state
            .default
            .as_ref()
            .and_then(|name| state.connections.iter().find(|c| &c.name == name));
        match current {
            Some(current) => Self {
                name: Input::new(&current.name),
                address: Input::new(&current.base_url),
                local: current.local,
                has_key: current.has_key,
                can_leave: true,
                ..Self::first_run()
            },
            None => Self {
                can_leave: !state.connections.is_empty(),
                ..Self::first_run()
            },
        }
    }

    fn field(&mut self) -> Option<&mut Input> {
        match self.focus {
            Field::Name => Some(&mut self.name),
            Field::Address => Some(&mut self.address),
            Field::Key => Some(&mut self.key),
            Field::Local => None,
        }
    }

    fn step(&mut self, by: isize) {
        let at = FIELDS.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        let next = (at + by).rem_euclid(FIELDS.len() as isize) as usize;
        self.focus = FIELDS[next];
    }

    /// The connection as typed, or what is wrong with it.
    fn draft(&self) -> Result<Draft, String> {
        let name = self.name.text().trim().to_string();
        valid_name(&name).map_err(|_| {
            "The name can use lowercase letters, digits, - and _, and starts with a letter or digit."
                .to_string()
        })?;
        let connection = Connection {
            base_url: self.address.text().trim().trim_end_matches('/').to_string(),
            model: None,
            env_key: None,
            local: self.local,
        };
        connection
            .validate()
            .map_err(|_| "The address must start with http:// or https://.".to_string())?;
        Ok(Draft {
            name,
            connection,
            key: Secret::new(self.key.text()),
        })
    }
}

/// The list a model is chosen from.
#[derive(Debug, Clone, PartialEq)]
pub struct Picker {
    /// The connection the model is for. For one being set up it carries the
    /// key that was typed.
    pub draft: Draft,
    /// The form to go back to, when the connection is still being set up.
    pub form: Option<Setup>,
    pub models: Vec<Model>,
    pub filter: Input,
    /// The chosen row among the models that match the filter.
    pub selected: usize,
    pub status: Status,
}

impl Picker {
    /// The models whose id contains every word of the filter.
    pub fn matching(&self) -> Vec<&Model> {
        let filter = self.filter.text().to_lowercase();
        let words: Vec<&str> = filter.split_whitespace().collect();
        self.models
            .iter()
            .filter(|model| {
                let id = model.id.to_lowercase();
                words.iter().all(|word| id.contains(word))
            })
            .collect()
    }

    /// The model Enter would choose: the selected row, or what was typed
    /// when no model matches. A server that lists no models can still be
    /// given one by name.
    fn choice(&self) -> Option<String> {
        let typed = self.filter.text().trim();
        match self.matching().get(self.selected) {
            Some(model) => Some(model.id.clone()),
            None if !typed.is_empty() => Some(typed.to_string()),
            None => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Chat {
    pub items: Vec<Item>,
    /// The reply that is arriving now.
    pub streaming: String,
    pub input: Input,
    /// A prompt has been sent and its turn is not over.
    pub busy: bool,
    pub activity: Activity,
    /// Lines scrolled up from the newest.
    pub scroll: usize,
    /// What this run of the app has spent, over calls with a known cost.
    pub cost: f64,
    /// Calls whose cost is not known.
    pub unpriced: u32,
    /// An action waiting for the owner's yes or no. While there is one, the
    /// keys answer it and nothing else.
    pub card: Option<Card>,
}

/// What the owner is shown when an action asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub id: String,
    pub summary: String,
    /// Why the model wants to do it.
    pub reason: String,
    /// Why it has to ask.
    pub why: String,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    User(String),
    Assistant(String),
    Tool {
        id: String,
        summary: String,
        reason: String,
        state: ToolState,
    },
    Notice(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Done,
    /// The first line of why it did not work.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Activity {
    #[default]
    Idle,
    Waiting,
    Thinking,
    Answering,
    Tool(String),
}

impl Default for View {
    fn default() -> Self {
        Self {
            screen: Screen::Loading,
            state: State::default(),
            chat: Chat::default(),
            quit: false,
            tick: 0,
        }
    }
}

/// Takes in an event from the host. Returns what to ask next.
pub fn apply(view: &mut View, event: &Event) -> Vec<Request> {
    match event {
        Event::State(state) => {
            view.state = state.clone();
            let saved =
                matches!(&view.screen, Screen::Models(p) if matches!(p.status, Status::Working(_)));
            if matches!(view.screen, Screen::Loading) || saved {
                view.screen = if state.connections.is_empty() {
                    Screen::Setup(Setup::first_run())
                } else {
                    Screen::Chat
                };
            }
        }
        Event::Models { models } => match &mut view.screen {
            Screen::Setup(form) => {
                if let Ok(draft) = form.draft() {
                    form.status = Status::Idle;
                    view.screen = Screen::Models(Box::new(Picker {
                        draft,
                        form: Some(form.clone()),
                        models: models.clone(),
                        filter: Input::default(),
                        selected: 0,
                        status: Status::Idle,
                    }));
                }
            }
            Screen::Chat => {
                if let Some(draft) = current_draft(&view.state) {
                    view.screen = Screen::Models(Box::new(Picker {
                        draft,
                        form: None,
                        models: models.clone(),
                        filter: Input::default(),
                        selected: 0,
                        status: Status::Idle,
                    }));
                }
            }
            _ => {}
        },
        Event::ModelChecked { model, ok, message } => {
            if let Screen::Models(picker) = &mut view.screen {
                if !ok {
                    picker.status = Status::Failed(message.clone());
                    return Vec::new();
                }
                picker.status = Status::Working("Saving…".into());
                let mut draft = picker.draft.clone();
                draft.connection.model = Some(model.clone());
                return vec![if picker.form.is_some() {
                    Request::SaveConnection(draft)
                } else {
                    Request::ChooseModel {
                        connection: draft.name,
                        model: model.clone(),
                    }
                }];
            }
        }
        Event::Failed { message } => match &mut view.screen {
            Screen::Setup(form) => form.status = Status::Failed(message.clone()),
            Screen::Models(picker) => picker.status = Status::Failed(message.clone()),
            _ => {
                view.chat.items.push(Item::Error(message.clone()));
                view.chat.busy = false;
                view.chat.activity = Activity::Idle;
            }
        },
        turn_event => view.chat.apply(turn_event),
    }
    Vec::new()
}

/// The connection in use, as a draft with no key: the stored one is used.
fn current_draft(state: &State) -> Option<Draft> {
    let name = state.default.as_ref()?;
    let current = state.connections.iter().find(|c| &c.name == name)?;
    Some(Draft {
        name: current.name.clone(),
        connection: Connection {
            base_url: current.base_url.clone(),
            model: current.model.clone(),
            env_key: None,
            local: current.local,
        },
        key: None,
    })
}

impl Chat {
    fn apply(&mut self, event: &Event) {
        match event {
            Event::UserMessage { text } => {
                self.items.push(Item::User(text.clone()));
                self.busy = true;
                self.activity = Activity::Waiting;
            }
            Event::Text { text } => {
                self.streaming.push_str(text);
                self.activity = Activity::Answering;
            }
            Event::Reasoning { .. } => self.activity = Activity::Thinking,
            Event::AssistantMessage { text } => {
                self.streaming.clear();
                self.items.push(Item::Assistant(text.clone()));
                self.activity = Activity::Waiting;
            }
            Event::ToolStarted {
                id,
                summary,
                reason,
                ..
            } => {
                self.activity = Activity::Tool(summary.clone());
                self.items.push(Item::Tool {
                    id: id.clone(),
                    summary: summary.clone(),
                    reason: reason.clone(),
                    state: ToolState::Running,
                });
            }
            Event::ToolFinished { id, ok, output } => {
                let finished = if *ok {
                    ToolState::Done
                } else {
                    ToolState::Failed(output.lines().next().unwrap_or_default().to_string())
                };
                let running = self.items.iter_mut().rev().find_map(|item| match item {
                    Item::Tool {
                        id: this, state, ..
                    } if this == id => Some(state),
                    _ => None,
                });
                if let Some(state) = running {
                    *state = finished;
                }
                self.activity = Activity::Waiting;
            }
            Event::ApprovalRequested {
                id,
                summary,
                reason,
                why,
                preview,
                ..
            } => {
                self.card = Some(Card {
                    id: id.clone(),
                    summary: summary.clone(),
                    reason: reason.clone(),
                    why: why.clone(),
                    preview: preview.clone(),
                });
            }
            Event::ApprovalResolved { id, .. } => {
                if self.card.as_ref().is_some_and(|card| &card.id == id) {
                    self.card = None;
                }
            }
            Event::Spend { cost, .. } => match cost {
                Some(cost) => self.cost += cost,
                None => self.unpriced += 1,
            },
            Event::Notice { text } => self.items.push(Item::Notice(text.clone())),
            Event::TurnFinished { reason, error } => {
                // A reply that was still arriving is not part of the session;
                // show what there was of it, marked as unfinished.
                let partial = std::mem::take(&mut self.streaming);
                if !partial.trim().is_empty() {
                    self.items
                        .push(Item::Assistant(format!("{} …", partial.trim_end())));
                }
                match reason {
                    StopReason::Completed | StopReason::Rounds => {}
                    StopReason::Cancelled => self.items.push(Item::Notice("Stopped.".into())),
                    StopReason::Stuck => self.items.push(Item::Notice(
                        "Stopped: the model kept making the same call and getting the same result."
                            .into(),
                    )),
                    StopReason::CutOff => self.items.push(Item::Notice(
                        "Stopped: the reply kept hitting the output limit.".into(),
                    )),
                    StopReason::Failed => self.items.push(Item::Error(
                        error.clone().unwrap_or_else(|| "The turn failed.".into()),
                    )),
                }
                self.busy = false;
                self.activity = Activity::Idle;
                self.card = None;
            }
            _ => {}
        }
    }
}

/// Takes in a key. Returns what to ask of the host.
pub fn key(view: &mut View, key: KeyEvent) -> Vec<Request> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('c') {
        // The first press stops a running turn; with nothing running it quits.
        if view.chat.busy {
            return vec![Request::Cancel];
        }
        view.quit = true;
        return Vec::new();
    }
    match &mut view.screen {
        Screen::Loading => Vec::new(),
        Screen::Setup(form) => {
            let leave = setup_key(form, key, ctrl);
            match leave {
                Leave::Stay(requests) => requests,
                Leave::Back => {
                    view.screen = Screen::Chat;
                    Vec::new()
                }
            }
        }
        Screen::Models(picker) => match picker_key(picker, key, ctrl) {
            Leave::Stay(requests) => requests,
            Leave::Back => {
                view.screen = match picker.form.take() {
                    Some(form) => Screen::Setup(form),
                    None => Screen::Chat,
                };
                Vec::new()
            }
        },
        Screen::Chat => chat_key(view, key, ctrl),
    }
}

/// Pastes text into whichever field has the focus.
pub fn paste(view: &mut View, text: &str) {
    match &mut view.screen {
        Screen::Setup(form) if !matches!(form.status, Status::Working(_)) => {
            if let Some(field) = form.field() {
                field.insert(text);
            }
        }
        Screen::Models(picker) if !matches!(picker.status, Status::Working(_)) => {
            picker.filter.insert(text);
            picker.selected = 0;
        }
        // A pasted block becomes one line: line breaks turn into spaces. A
        // paste never answers a question, even one that contains a `y`.
        Screen::Chat if view.chat.card.is_none() => {
            view.chat.input.insert(&text.replace(['\r', '\n'], " "));
        }
        _ => {}
    }
}

enum Leave {
    Stay(Vec<Request>),
    Back,
}

/// Editing keys every field shares. Returns whether the key was one.
fn edit(input: &mut Input, key: KeyEvent, ctrl: bool) -> bool {
    match key.code {
        KeyCode::Char('w') if ctrl => input.delete_word(),
        KeyCode::Char('a') if ctrl => input.home(),
        KeyCode::Char('e') if ctrl => input.end(),
        KeyCode::Char('u') if ctrl => {
            input.take();
        }
        KeyCode::Char(c) if !ctrl => input.insert(c.encode_utf8(&mut [0; 4])),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        _ => return false,
    }
    true
}

fn setup_key(form: &mut Setup, key: KeyEvent, ctrl: bool) -> Leave {
    if matches!(form.status, Status::Working(_)) {
        return Leave::Stay(Vec::new());
    }
    match key.code {
        KeyCode::Esc if form.can_leave => return Leave::Back,
        KeyCode::Tab | KeyCode::Down => form.step(1),
        KeyCode::BackTab | KeyCode::Up => form.step(-1),
        KeyCode::Char(' ') if form.focus == Field::Local => form.local = !form.local,
        KeyCode::Enter => {
            // Enter walks down the form and sends it from the last field.
            if form.focus != Field::Local && form.focus != Field::Key {
                form.step(1);
                return Leave::Stay(Vec::new());
            }
            match form.draft() {
                Ok(draft) => {
                    form.status = Status::Working("Reaching the connection…".into());
                    return Leave::Stay(vec![Request::TryConnection(draft)]);
                }
                Err(problem) => form.status = Status::Failed(problem),
            }
        }
        _ => {
            if let Some(field) = form.field()
                && edit(field, key, ctrl)
            {
                form.status = Status::Idle;
            }
        }
    }
    Leave::Stay(Vec::new())
}

fn picker_key(picker: &mut Picker, key: KeyEvent, ctrl: bool) -> Leave {
    if matches!(picker.status, Status::Working(_)) {
        return Leave::Stay(Vec::new());
    }
    let count = picker.matching().len();
    let last = count.saturating_sub(1);
    match key.code {
        KeyCode::Esc => return Leave::Back,
        KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
        KeyCode::Down => picker.selected = (picker.selected + 1).min(last),
        KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(10),
        KeyCode::PageDown => picker.selected = (picker.selected + 10).min(last),
        KeyCode::Enter => {
            if let Some(model) = picker.choice() {
                picker.status = Status::Working(format!("Checking {model} with a real tool call…"));
                let mut draft = picker.draft.clone();
                draft.connection.model = Some(model);
                return Leave::Stay(vec![Request::CheckModel(draft)]);
            }
        }
        _ => {
            if edit(&mut picker.filter, key, ctrl) {
                picker.selected = 0;
                picker.status = Status::Idle;
            }
        }
    }
    Leave::Stay(Vec::new())
}

fn chat_key(view: &mut View, key: KeyEvent, ctrl: bool) -> Vec<Request> {
    let chat = &mut view.chat;
    // A question is on the screen: the keys answer it. Only an explicit `y`
    // is a yes, so a stray Enter cannot approve anything.
    if let Some(card) = &chat.card {
        let approve = match key.code {
            KeyCode::Char('y' | 'Y') => true,
            KeyCode::Char('n' | 'N') | KeyCode::Esc => false,
            _ => return Vec::new(),
        };
        return vec![Request::Approval {
            id: card.id.clone(),
            approve,
            note: None,
        }];
    }
    match key.code {
        KeyCode::Esc if chat.busy => return vec![Request::Cancel],
        KeyCode::PageUp => chat.scroll += 10,
        KeyCode::PageDown => chat.scroll = chat.scroll.saturating_sub(10),
        KeyCode::Enter => {
            let text = chat.input.take();
            let text = text.trim();
            if text.is_empty() {
                return Vec::new();
            }
            chat.scroll = 0;
            if let Some(command) = text.strip_prefix('/') {
                return run_command(view, command);
            }
            if chat.busy {
                chat.items
                    .push(Item::Notice(format!("Queued for after this turn: {text}")));
            } else {
                chat.busy = true;
                chat.activity = Activity::Waiting;
            }
            return vec![Request::Prompt {
                text: text.to_string(),
            }];
        }
        _ => {
            edit(&mut chat.input, key, ctrl);
        }
    }
    Vec::new()
}

fn run_command(view: &mut View, command: &str) -> Vec<Request> {
    let chat = &mut view.chat;
    match command.trim() {
        "quit" | "exit" => view.quit = true,
        "help" => chat.items.push(Item::Notice(HELP.into())),
        _ if chat.busy => chat.items.push(Item::Notice(
            "A turn is running. Press esc to stop it first.".into(),
        )),
        "new" => {
            chat.items.clear();
            chat.items.push(Item::Notice(
                "A new session starts with your next message.".into(),
            ));
            return vec![Request::NewSession];
        }
        "model" => match &view.state.default {
            Some(connection) => {
                chat.items.push(Item::Notice(format!(
                    "Fetching the models of {connection}…"
                )));
                return vec![Request::Models {
                    connection: connection.clone(),
                }];
            }
            None => chat.items.push(Item::Notice(
                "No connection is in use. Try /connections.".into(),
            )),
        },
        "connections" | "connection" => view.screen = Screen::Setup(Setup::from_state(&view.state)),
        other => chat
            .items
            .push(Item::Notice(format!("There is no /{other}. {HELP}"))),
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiphys_core::proto::{ConnectionInfo, SessionInfo};

    fn press(view: &mut View, code: KeyCode) -> Vec<Request> {
        key(view, KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(view: &mut View, c: char) -> Vec<Request> {
        key(view, KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn type_text(view: &mut View, text: &str) {
        for c in text.chars() {
            press(view, KeyCode::Char(c));
        }
    }

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            context: None,
            rates: None,
            tools: None,
        }
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

    fn chatting() -> View {
        let mut view = View::default();
        apply(&mut view, &Event::State(state(false)));
        assert_eq!(view.screen, Screen::Chat);
        view
    }

    #[test]
    fn a_first_run_walks_from_the_key_to_a_checked_model_to_the_chat() {
        let mut view = View::default();
        assert!(apply(&mut view, &Event::State(State::default())).is_empty());
        let Screen::Setup(form) = &view.screen else {
            panic!("expected the setup form");
        };
        // The cursor starts on the one field that has to be filled in.
        assert_eq!((form.focus, form.can_leave), (Field::Key, false));

        paste(&mut view, "sk-live-123\n");
        let sent = press(&mut view, KeyCode::Enter);
        let [Request::TryConnection(draft)] = sent.as_slice() else {
            panic!("expected the connection to be tried: {sent:?}");
        };
        assert_eq!(draft.name, "openrouter");
        assert_eq!(draft.connection.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(draft.key.as_ref().unwrap().expose(), "sk-live-123");
        // While that is in flight, keys do nothing.
        assert!(press(&mut view, KeyCode::Enter).is_empty());

        apply(
            &mut view,
            &Event::Models {
                models: vec![model("vendor/a"), model("vendor/b"), model("other/c")],
            },
        );
        type_text(&mut view, "vendor");
        press(&mut view, KeyCode::Down);
        let sent = press(&mut view, KeyCode::Enter);
        let [Request::CheckModel(draft)] = sent.as_slice() else {
            panic!("expected the model to be checked: {sent:?}");
        };
        assert_eq!(draft.connection.model.as_deref(), Some("vendor/b"));
        assert_eq!(draft.key.as_ref().unwrap().expose(), "sk-live-123");

        let sent = apply(
            &mut view,
            &Event::ModelChecked {
                model: "vendor/b".into(),
                ok: true,
                message: String::new(),
            },
        );
        let [Request::SaveConnection(draft)] = sent.as_slice() else {
            panic!("expected the connection to be saved: {sent:?}");
        };
        assert_eq!(draft.connection.model.as_deref(), Some("vendor/b"));

        apply(&mut view, &Event::State(state(false)));
        assert_eq!(view.screen, Screen::Chat);
    }

    #[test]
    fn a_form_that_cannot_work_says_so_without_asking_the_host() {
        let mut view = View::default();
        apply(&mut view, &Event::State(State::default()));
        press(&mut view, KeyCode::BackTab);
        ctrl(&mut view, 'u');
        type_text(&mut view, "openrouter.ai/api");
        press(&mut view, KeyCode::Enter);
        assert!(press(&mut view, KeyCode::Enter).is_empty());
        let Screen::Setup(form) = &view.screen else {
            panic!("expected the setup form");
        };
        assert_eq!(
            form.status,
            Status::Failed("The address must start with http:// or https://.".into())
        );
        // Typing clears the complaint.
        press(&mut view, KeyCode::BackTab);
        type_text(&mut view, "x");
        let Screen::Setup(form) = &view.screen else {
            panic!("expected the setup form");
        };
        assert_eq!(form.status, Status::Idle);
    }

    #[test]
    fn a_failure_shows_on_the_screen_that_asked_and_can_be_retried() {
        let mut view = View::default();
        apply(&mut view, &Event::State(State::default()));
        paste(&mut view, "sk-bad");
        press(&mut view, KeyCode::Enter);
        apply(
            &mut view,
            &Event::Failed {
                message: "the provider refused the key (401)".into(),
            },
        );
        let Screen::Setup(form) = &view.screen else {
            panic!("expected the setup form");
        };
        assert_eq!(
            form.status,
            Status::Failed("the provider refused the key (401)".into())
        );
        // The form is live again.
        assert_eq!(press(&mut view, KeyCode::Enter).len(), 1);

        apply(
            &mut view,
            &Event::Models {
                models: vec![model("a")],
            },
        );
        press(&mut view, KeyCode::Enter);
        apply(
            &mut view,
            &Event::ModelChecked {
                model: "a".into(),
                ok: false,
                message: "it may not support tool calls".into(),
            },
        );
        let Screen::Models(picker) = &view.screen else {
            panic!("expected the model list");
        };
        assert_eq!(
            picker.status,
            Status::Failed("it may not support tool calls".into())
        );
        // Esc goes back to the form, with what was typed still in it.
        press(&mut view, KeyCode::Esc);
        let Screen::Setup(form) = &view.screen else {
            panic!("expected the setup form");
        };
        assert_eq!(form.key.text(), "sk-bad");
    }

    #[test]
    fn the_model_list_filters_as_you_type_and_takes_a_typed_name_when_nothing_matches() {
        let mut view = chatting();
        type_text(&mut view, "/model");
        let sent = press(&mut view, KeyCode::Enter);
        assert_eq!(
            sent,
            [Request::Models {
                connection: "openrouter".into()
            }]
        );
        apply(
            &mut view,
            &Event::Models {
                models: vec![
                    model("Vendor/Alpha"),
                    model("vendor/beta"),
                    model("other/alpha"),
                ],
            },
        );

        type_text(&mut view, "alpha vend");
        let Screen::Models(picker) = &view.screen else {
            panic!("expected the model list");
        };
        assert_eq!(
            picker
                .matching()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["Vendor/Alpha"]
        );
        // The stored key is used: none travels with the request.
        let sent = press(&mut view, KeyCode::Enter);
        let [Request::CheckModel(draft)] = sent.as_slice() else {
            panic!("expected a check: {sent:?}");
        };
        assert_eq!(
            (draft.connection.model.as_deref(), &draft.key),
            (Some("Vendor/Alpha"), &None)
        );
        let sent = apply(
            &mut view,
            &Event::ModelChecked {
                model: "Vendor/Alpha".into(),
                ok: true,
                message: String::new(),
            },
        );
        assert_eq!(
            sent,
            [Request::ChooseModel {
                connection: "openrouter".into(),
                model: "Vendor/Alpha".into()
            }]
        );

        // Nothing matches: what was typed is the model.
        let mut view = chatting();
        view.chat.input.insert("/model");
        press(&mut view, KeyCode::Enter);
        apply(&mut view, &Event::Models { models: vec![] });
        assert!(press(&mut view, KeyCode::Enter).is_empty());
        type_text(&mut view, "my-local-model");
        let sent = press(&mut view, KeyCode::Enter);
        let [Request::CheckModel(draft)] = sent.as_slice() else {
            panic!("expected a check: {sent:?}");
        };
        assert_eq!(draft.connection.model.as_deref(), Some("my-local-model"));
    }

    #[test]
    fn a_turn_is_drawn_from_its_events() {
        let mut view = chatting();
        type_text(&mut view, "how full is the disk?");
        let sent = press(&mut view, KeyCode::Enter);
        assert_eq!(
            sent,
            [Request::Prompt {
                text: "how full is the disk?".into()
            }]
        );
        assert!(view.chat.busy && view.chat.input.is_empty());

        let events = [
            Event::UserMessage {
                text: "how full is the disk?".into(),
            },
            Event::Reasoning { text: "hm".into() },
            Event::Text {
                text: "Let me ".into(),
            },
            Event::Text {
                text: "look.".into(),
            },
            Event::Spend {
                cost: Some(0.002),
                usage: None,
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
                output: "…".into(),
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
                output: "/nope: No such file\nmore".into(),
            },
            Event::Spend {
                cost: None,
                usage: None,
            },
            Event::AssistantMessage {
                text: "42% full.".into(),
            },
            Event::TurnFinished {
                reason: StopReason::Completed,
                error: None,
            },
        ];
        for event in &events {
            assert!(apply(&mut view, event).is_empty());
        }
        let tool = |id: &str, summary: &str, reason: &str, state| Item::Tool {
            id: id.into(),
            summary: summary.into(),
            reason: reason.into(),
            state,
        };
        assert_eq!(
            view.chat.items,
            [
                Item::User("how full is the disk?".into()),
                Item::Assistant("Let me look.".into()),
                tool(
                    "a",
                    "read /proc/mounts",
                    "to find the root disk",
                    ToolState::Done
                ),
                tool(
                    "b",
                    "read /nope",
                    "",
                    ToolState::Failed("/nope: No such file".into())
                ),
                Item::Assistant("42% full.".into()),
            ]
        );
        assert!(!view.chat.busy && view.chat.streaming.is_empty());
        assert_eq!((view.chat.cost, view.chat.unpriced), (0.002, 1));
        assert_eq!(view.chat.activity, Activity::Idle);
    }

    #[test]
    fn stopping_a_turn_keeps_what_had_arrived_marked_as_unfinished() {
        let mut view = chatting();
        type_text(&mut view, "write a lot");
        press(&mut view, KeyCode::Enter);
        apply(
            &mut view,
            &Event::UserMessage {
                text: "write a lot".into(),
            },
        );
        apply(
            &mut view,
            &Event::Text {
                text: "Once upon a".into(),
            },
        );

        assert_eq!(press(&mut view, KeyCode::Esc), [Request::Cancel]);
        // Ctrl-C stops a running turn before it quits.
        assert_eq!(ctrl(&mut view, 'c'), [Request::Cancel]);
        assert!(!view.quit);

        apply(
            &mut view,
            &Event::TurnFinished {
                reason: StopReason::Cancelled,
                error: None,
            },
        );
        assert_eq!(
            view.chat.items[1..],
            [
                Item::Assistant("Once upon a …".into()),
                Item::Notice("Stopped.".into())
            ]
        );
        assert!(ctrl(&mut view, 'c').is_empty());
        assert!(view.quit);
    }

    #[test]
    fn a_failed_turn_shows_its_error_and_frees_the_input() {
        let mut view = chatting();
        type_text(&mut view, "hi");
        press(&mut view, KeyCode::Enter);
        apply(
            &mut view,
            &Event::TurnFinished {
                reason: StopReason::Failed,
                error: Some("the provider refused the key (401)".into()),
            },
        );
        assert_eq!(
            view.chat.items,
            [Item::Error("the provider refused the key (401)".into())]
        );
        assert!(!view.chat.busy);

        // A prompt that never became a turn fails the same way.
        type_text(&mut view, "again");
        press(&mut view, KeyCode::Enter);
        apply(
            &mut view,
            &Event::Failed {
                message: "no model chosen".into(),
            },
        );
        assert!(!view.chat.busy);
        assert_eq!(
            view.chat.items.last(),
            Some(&Item::Error("no model chosen".into()))
        );
    }

    #[test]
    fn commands_do_their_jobs_and_wait_for_a_running_turn() {
        let mut view = chatting();
        view.chat.items.push(Item::Assistant("old".into()));
        type_text(&mut view, "/new");
        assert_eq!(press(&mut view, KeyCode::Enter), [Request::NewSession]);
        assert_eq!(view.chat.items.len(), 1);

        type_text(&mut view, "/connections");
        press(&mut view, KeyCode::Enter);
        let Screen::Setup(form) = &view.screen else {
            panic!("expected the setup form");
        };
        // The connection in use is filled in; its key is not, and stays stored.
        assert_eq!(
            (form.name.text(), form.address.text()),
            ("openrouter", "https://openrouter.ai/api/v1")
        );
        assert!(form.key.is_empty() && form.has_key && form.can_leave);
        press(&mut view, KeyCode::Esc);
        assert_eq!(view.screen, Screen::Chat);

        type_text(&mut view, "/nonsense");
        assert!(press(&mut view, KeyCode::Enter).is_empty());
        assert!(
            matches!(view.chat.items.last(), Some(Item::Notice(text)) if text.starts_with("There is no /nonsense."))
        );

        // During a turn: a message queues, a command waits, /quit still quits.
        type_text(&mut view, "first");
        press(&mut view, KeyCode::Enter);
        type_text(&mut view, "second");
        assert_eq!(
            press(&mut view, KeyCode::Enter),
            [Request::Prompt {
                text: "second".into()
            }]
        );
        assert_eq!(
            view.chat.items.last(),
            Some(&Item::Notice("Queued for after this turn: second".into()))
        );
        type_text(&mut view, "/new");
        assert!(press(&mut view, KeyCode::Enter).is_empty());
        type_text(&mut view, "/quit");
        press(&mut view, KeyCode::Enter);
        assert!(view.quit);
    }

    #[test]
    fn a_question_takes_the_keys_until_it_is_answered_and_only_y_is_a_yes() {
        let asked = Event::ApprovalRequested {
            id: "w1".into(),
            tool: "write_file".into(),
            summary: "write /etc/hosts (3 lines)".into(),
            reason: "to add a host".into(),
            why: "/etc/hosts is outside Tiphys's own home".into(),
            class: tiphys_core::policy::Class::System,
            preview: Some("+10.0.0.5 db".into()),
        };
        let answer = |approve| Request::Approval {
            id: "w1".into(),
            approve,
            note: None,
        };

        let mut view = chatting();
        type_text(&mut view, "edit the hosts file");
        press(&mut view, KeyCode::Enter);
        apply(&mut view, &asked);
        assert_eq!(
            view.chat.card.as_ref().unwrap().summary,
            "write /etc/hosts (3 lines)"
        );

        // Enter, other letters and a paste answer nothing and type nothing.
        assert!(press(&mut view, KeyCode::Enter).is_empty());
        assert!(press(&mut view, KeyCode::Char('x')).is_empty());
        paste(&mut view, "yes");
        assert!(view.chat.input.is_empty());

        assert_eq!(press(&mut view, KeyCode::Char('y')), [answer(true)]);
        assert_eq!(press(&mut view, KeyCode::Char('n')), [answer(false)]);
        assert_eq!(press(&mut view, KeyCode::Esc), [answer(false)]);

        // The card stays until the host says the question is settled.
        assert!(view.chat.card.is_some());
        apply(
            &mut view,
            &Event::ApprovalResolved {
                id: "other".into(),
                approved: true,
                note: String::new(),
            },
        );
        assert!(view.chat.card.is_some());
        apply(
            &mut view,
            &Event::ApprovalResolved {
                id: "w1".into(),
                approved: true,
                note: String::new(),
            },
        );
        assert!(view.chat.card.is_none());

        // A turn that ends takes its question with it.
        apply(&mut view, &asked);
        apply(
            &mut view,
            &Event::TurnFinished {
                reason: StopReason::Cancelled,
                error: None,
            },
        );
        assert!(view.chat.card.is_none());
    }

    #[test]
    fn the_conversation_scrolls_and_a_new_message_returns_to_the_end() {
        let mut view = chatting();
        press(&mut view, KeyCode::PageUp);
        press(&mut view, KeyCode::PageUp);
        press(&mut view, KeyCode::PageDown);
        assert_eq!(view.chat.scroll, 10);
        type_text(&mut view, "hi");
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.chat.scroll, 0);
        // An empty line sends nothing.
        apply(
            &mut view,
            &Event::TurnFinished {
                reason: StopReason::Completed,
                error: None,
            },
        );
        type_text(&mut view, "   ");
        assert!(press(&mut view, KeyCode::Enter).is_empty());
    }

    #[test]
    fn a_paste_goes_into_the_focused_field_as_one_line() {
        let mut view = chatting();
        paste(&mut view, "line one\nline two\r\n");
        assert_eq!(view.chat.input.text(), "line one line two  ");
    }
}

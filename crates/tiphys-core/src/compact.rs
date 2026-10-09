//! Keeping a conversation within what a model can hold.
//!
//! A chat with the agent never resets, so sooner or later it is longer than
//! the model's context window. Before that happens, the oldest part is left
//! out of what the model is sent. Nothing is deleted: the transcript keeps
//! every message, and a marker in it says where the model's view now starts.
//!
//! What goes first is what matters least, in this order:
//!
//! 1. The results of tool calls from all but the last few turns. A result is
//!    replaced by a line saying it was left out; the call stays, so the model
//!    still sees what it did.
//! 2. Whole turns, oldest first, down to the turn in progress.
//! 3. The older tool results of the turn in progress, when one long turn is
//!    too much by itself.
//!
//! No model is asked to summarise anything. What the agent must not forget
//! belongs in memory, not in a summary nobody chose.

use crate::llm::{Message, Role, ToolSpec};

/// The window assumed for a model whose provider does not say.
pub const DEFAULT_WINDOW: u64 = 100_000;
/// The share of the window at which the conversation is cut down.
const COMPACT_AT: f64 = 0.75;
/// The share of the window it is cut down to, so that it is not cut again on
/// the next message.
const COMPACT_TO: f64 = 0.5;
/// Turns whose tool results are kept whole while older ones go.
const KEEP_TURNS: usize = 4;
/// Messages at the end of a single over-long turn that are kept whole.
const KEEP_MESSAGES: usize = 6;

/// What stands in for a tool result that was left out.
pub const TRIMMED: &str =
    "[an earlier tool result, left out to save room; run the tool again if it is needed]";

/// Where the model's view of a transcript starts and what is thinned in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct View {
    /// The first message the model is sent.
    pub start: usize,
    /// Tool results before this message are replaced by [`TRIMMED`].
    pub trim_before: usize,
}

/// The messages the model is sent: the transcript as `view` shows it.
pub fn apply(messages: &[Message], view: View) -> Vec<Message> {
    let mut shown = Vec::with_capacity(messages.len().saturating_sub(view.start) + 1);
    if view.start > 0 {
        shown.push(Message::user(format!(
            "[Tiphys: the first {} messages of this conversation were left out to fit your \
             context. If something from them is needed, ask the owner.]",
            view.start
        )));
    }
    for (index, message) in messages.iter().enumerate().skip(view.start) {
        if message.role == Role::Tool && index < view.trim_before {
            shown.push(Message {
                content: TRIMMED.into(),
                ..message.clone()
            });
        } else {
            shown.push(message.clone());
        }
    }
    shown
}

/// A rough count of the tokens a request would be: four bytes to a token,
/// which is close enough to decide when to cut and errs on the early side for
/// code and logs.
pub fn estimate(system: &str, messages: &[Message], tools: &[ToolSpec]) -> u64 {
    let message = |m: &Message| {
        m.content.len()
            + m.tool_calls
                .iter()
                .map(|c| c.name.len() + c.arguments.len() + 16)
                .sum::<usize>()
            + 8
    };
    let tool = |t: &ToolSpec| t.name.len() + t.description.len() + t.parameters.to_string().len();
    let bytes = system.len()
        + messages.iter().map(message).sum::<usize>()
        + tools.iter().map(tool).sum::<usize>();
    (bytes / 4) as u64
}

/// Whether a conversation of `tokens` should be cut down for a model with
/// this context window.
pub fn is_due(tokens: u64, window: u64) -> bool {
    tokens as f64 > window as f64 * COMPACT_AT
}

/// Works out a smaller view of `messages` for a model with this window, or
/// `None` if the current one cannot be made smaller. `fixed` is what every
/// request carries besides the messages: the system prompt and the tools.
pub fn plan(messages: &[Message], current: View, window: u64, fixed: u64) -> Option<View> {
    let target = (window as f64 * COMPACT_TO) as u64;
    let size = |view: View| fixed + estimate("", &apply(messages, view), &[]);
    let turns: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(index, m)| m.role == Role::User && *index >= current.start)
        .map(|(index, _)| index)
        .collect();
    let last_turn = *turns.last()?;
    let mut view = current;

    // 1. Tool results from before the last few turns.
    let recent = turns[turns.len().saturating_sub(KEEP_TURNS)];
    view.trim_before = view.trim_before.max(recent);
    // 2. Whole turns, oldest first.
    for turn in &turns {
        if size(view) <= target {
            break;
        }
        view.start = view.start.max(*turn);
    }
    view.start = view.start.min(last_turn);
    // 3. The older tool results of the one turn that is left.
    if size(view) > target {
        view.trim_before = view
            .trim_before
            .max(messages.len().saturating_sub(KEEP_MESSAGES));
    }
    view.trim_before = view.trim_before.max(view.start);
    (view != current).then_some(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }
    }

    /// A turn: a question, a tool call with a result of `result` bytes, and
    /// an answer.
    fn turn(n: usize, result: usize) -> Vec<Message> {
        vec![
            Message::user(format!("question {n}")),
            Message::assistant("", vec![call(&format!("c{n}"))]),
            Message::tool(format!("c{n}"), "x".repeat(result)),
            Message::assistant(format!("answer {n}"), vec![]),
        ]
    }

    fn conversation(turns: usize, result: usize) -> Vec<Message> {
        (0..turns).flat_map(|n| turn(n, result)).collect()
    }

    /// Every tool result in what the model is sent answers a call it is also
    /// sent, and the first message is from the user.
    fn is_well_formed(shown: &[Message]) -> bool {
        let mut asked: Vec<&str> = Vec::new();
        for message in shown {
            asked.extend(message.tool_calls.iter().map(|c| c.id.as_str()));
            if let Some(id) = &message.tool_call_id
                && !asked.contains(&id.as_str())
            {
                return false;
            }
        }
        shown.first().is_some_and(|m| m.role == Role::User)
    }

    #[test]
    fn a_view_that_starts_at_the_beginning_is_the_transcript() {
        let messages = conversation(3, 100);
        assert_eq!(apply(&messages, View::default()), messages);
    }

    #[test]
    fn old_tool_results_go_first_and_the_calls_stay() {
        // Eight turns of 4,000 tokens each: 32,000, in a window of 40,000.
        let messages = conversation(8, 16_000);
        let before = estimate("", &messages, &[]);
        assert!(is_due(before, 40_000));

        let view = plan(&messages, View::default(), 40_000, 0).unwrap();
        // The results of the last four turns are whole; no turn was dropped.
        assert_eq!(
            view,
            View {
                start: 0,
                trim_before: 16
            }
        );
        let shown = apply(&messages, view);
        assert_eq!(shown.len(), messages.len());
        assert_eq!(shown[2].content, TRIMMED);
        assert_eq!(shown[2].tool_call_id.as_deref(), Some("c0"));
        assert_eq!(shown[1].tool_calls, messages[1].tool_calls);
        assert_eq!(shown[18].content.len(), 16_000);
        assert!(estimate("", &shown, &[]) <= 20_000);
        assert!(is_well_formed(&shown));
    }

    #[test]
    fn whole_turns_go_next_oldest_first_and_the_cut_is_at_a_turn() {
        // Twenty turns of 4,000 tokens: far more than a window of 20,000.
        let messages = conversation(20, 16_000);
        let view = plan(&messages, View::default(), 20_000, 0).unwrap();
        assert!(view.start > 0 && view.start.is_multiple_of(4), "{view:?}");
        let shown = apply(&messages, view);
        assert!(
            shown[0]
                .content
                .contains(&format!("first {} messages", view.start))
        );
        assert_eq!(shown[1].content, format!("question {}", view.start / 4));
        assert!(estimate("", &shown, &[]) <= 10_000);
        assert!(is_well_formed(&shown));
        // The last turn is always there, whole.
        assert_eq!(shown.last().unwrap().content, "answer 19");
    }

    #[test]
    fn one_long_turn_keeps_its_latest_results_and_thins_the_rest() {
        // One question, then thirty tool calls with large results.
        let mut messages = vec![Message::user("a long job")];
        for n in 0..30 {
            messages.push(Message::assistant("", vec![call(&format!("c{n}"))]));
            messages.push(Message::tool(format!("c{n}"), "x".repeat(16_000)));
        }
        let view = plan(&messages, View::default(), 40_000, 0).unwrap();
        assert_eq!(view.start, 0);
        let shown = apply(&messages, view);
        let whole = shown
            .iter()
            .filter(|m| m.role == Role::Tool && m.content != TRIMMED)
            .count();
        assert_eq!(whole, 3);
        assert_eq!(shown.last().unwrap().content.len(), 16_000);
        assert!(is_well_formed(&shown));
    }

    #[test]
    fn a_view_already_as_small_as_it_gets_is_left_alone() {
        // Short enough that nothing has to go.
        assert_eq!(
            plan(&conversation(2, 100), View::default(), 40_000, 0),
            None
        );
        assert_eq!(plan(&[], View::default(), 40_000, 0), None);
        // Cut down once, and asked again with nothing new: nothing more to do.
        let messages = conversation(20, 16_000);
        let view = plan(&messages, View::default(), 20_000, 0).unwrap();
        assert_eq!(plan(&messages, view, 20_000, 0), None);
    }

    #[test]
    fn a_later_cut_never_brings_back_what_an_earlier_one_left_out() {
        let mut messages = conversation(20, 16_000);
        let first = plan(&messages, View::default(), 20_000, 0).unwrap();
        messages.extend(conversation(20, 16_000));
        let second = plan(&messages, first, 20_000, 0).unwrap();
        assert!(second.start >= first.start && second.trim_before >= first.trim_before);
        assert!(is_well_formed(&apply(&messages, second)));
    }

    #[test]
    fn what_every_request_carries_counts_toward_the_window() {
        let messages = conversation(6, 16_000);
        let lean = plan(&messages, View::default(), 60_000, 0).unwrap();
        let heavy = plan(&messages, View::default(), 60_000, 25_000).unwrap();
        assert!(heavy.start > lean.start, "{lean:?} {heavy:?}");
    }
}

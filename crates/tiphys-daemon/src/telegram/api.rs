//! The part of Telegram's Bot API that Tiphys uses.
//!
//! Every call is `POST {base}/bot{token}/{method}` with a JSON body. The
//! token is part of the address, so an error from the HTTP client is never
//! passed on with its address in it.

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tiphys_core::keys::Secret;

/// Where the Bot API is, unless the configuration says otherwise.
pub const TELEGRAM: &str = "https://api.telegram.org";
/// The most text one message may carry. Telegram's limit is 4096 UTF-16
/// units; this leaves room under it.
pub const MESSAGE_CHARS: usize = 3800;
/// How long an ordinary call may take.
const CALL_WITHIN: Duration = Duration::from_secs(30);
/// How often a call is made again after Telegram said to slow down.
const SLOWED_TRIES: u32 = 3;
/// The longest Telegram may ask for before the call is given up.
const SLOWED_AT_MOST: u64 = 30;

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Telegram does not know this token.
    BadToken,
    /// Something else is polling with this token, so updates would be split
    /// between the two.
    Conflict,
    /// Telegram said to slow down, and to ask again in this many seconds.
    Slow(u64),
    /// Anything else: the network, a refused message.
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadToken => f.write_str("Telegram does not accept this bot token"),
            Self::Conflict => f.write_str(
                "something else is already reading this bot's messages; a bot can serve one \
                 Tiphys at a time",
            ),
            Self::Slow(seconds) => write!(f, "Telegram said to slow down for {seconds} seconds"),
            Self::Other(what) => f.write_str(what),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Chat {
    pub id: i64,
    /// `private`, `group`, `supergroup` or `channel`.
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Message {
    pub message_id: i64,
    #[serde(default)]
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
}

/// A button under a message was pressed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Callback {
    pub id: String,
    pub from: User,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<Callback>,
}

/// A button: what it says, and what pressing it sends back.
pub type Button<'a> = (&'a str, &'a str);

pub struct Api {
    client: reqwest::Client,
    base: String,
    token: Secret,
}

impl Api {
    pub fn new(base: &str, token: Secret) -> Result<Self, ApiError> {
        let client = reqwest::Client::builder()
            .user_agent(format!("tiphys/{}", tiphys_core::VERSION))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| ApiError::Other(format!("could not set up the connection: {e}")))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Makes a call, and makes it again after a wait when Telegram says to
    /// slow down, which it does to a bot that sends a lot at once.
    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        body: Value,
        within: Duration,
    ) -> Result<T, ApiError> {
        let mut tries = 0;
        loop {
            match self.call_once(method, &body, within).await {
                Err(ApiError::Slow(seconds))
                    if tries < SLOWED_TRIES && seconds <= SLOWED_AT_MOST =>
                {
                    tries += 1;
                    tokio::time::sleep(Duration::from_secs(seconds.max(1))).await;
                }
                answered => return answered,
            }
        }
    }

    async fn call_once<T: DeserializeOwned>(
        &self,
        method: &str,
        body: &Value,
        within: Duration,
    ) -> Result<T, ApiError> {
        #[derive(Deserialize, Default)]
        struct Parameters {
            #[serde(default)]
            retry_after: Option<u64>,
        }
        #[derive(Deserialize)]
        struct Answer {
            ok: bool,
            #[serde(default)]
            result: Option<Value>,
            #[serde(default)]
            error_code: Option<i64>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            parameters: Option<Parameters>,
        }
        let url = format!("{}/bot{}/{method}", self.base, self.token.expose());
        let sent = self
            .client
            .post(url)
            .json(body)
            .timeout(within)
            .send()
            .await;
        // The address holds the token, so it is taken off the error.
        let response = sent.map_err(|e| {
            ApiError::Other(format!(
                "Telegram could not be reached: {}",
                e.without_url()
            ))
        })?;
        let status = response.status().as_u16();
        let answer: Answer = match response.json().await {
            Ok(answer) => answer,
            Err(_) if status == 401 || status == 404 => return Err(ApiError::BadToken),
            Err(e) => {
                return Err(ApiError::Other(format!(
                    "Telegram's answer to {method} could not be read: {}",
                    e.without_url()
                )));
            }
        };
        if !answer.ok {
            return Err(match answer.error_code.unwrap_or(i64::from(status)) {
                401 | 404 => ApiError::BadToken,
                409 => ApiError::Conflict,
                429 => ApiError::Slow(
                    answer
                        .parameters
                        .unwrap_or_default()
                        .retry_after
                        .unwrap_or(1),
                ),
                code => ApiError::Other(format!(
                    "Telegram refused {method} ({code}): {}",
                    answer.description.unwrap_or_default()
                )),
            });
        }
        serde_json::from_value(answer.result.unwrap_or(Value::Null)).map_err(|e| {
            ApiError::Other(format!(
                "Telegram's answer to {method} was not what was expected: {e}"
            ))
        })
    }

    /// The bot this token is for.
    pub async fn get_me(&self) -> Result<User, ApiError> {
        self.call("getMe", json!({}), CALL_WITHIN).await
    }

    /// Waits up to `wait` seconds for updates numbered `offset` and above.
    /// Asking with an offset tells Telegram everything below it was handled.
    pub async fn get_updates(&self, offset: i64, wait: u64) -> Result<Vec<Update>, ApiError> {
        let body = json!({
            "offset": offset,
            "timeout": wait,
            "allowed_updates": ["message", "callback_query"],
        });
        self.call("getUpdates", body, Duration::from_secs(wait + 20))
            .await
    }

    /// Sends a message, with buttons under it if any are given. Returns the
    /// message's id.
    pub async fn send_message(
        &self,
        chat: i64,
        text: &str,
        buttons: &[Button<'_>],
    ) -> Result<i64, ApiError> {
        let mut body = json!({"chat_id": chat, "text": text, "disable_web_page_preview": true});
        if !buttons.is_empty() {
            body["reply_markup"] = keyboard(buttons);
        }
        let sent: Message = self.call("sendMessage", body, CALL_WITHIN).await?;
        Ok(sent.message_id)
    }

    /// Replaces a message's text, and its buttons with the ones given.
    pub async fn edit_message(
        &self,
        chat: i64,
        message: i64,
        text: &str,
        buttons: &[Button<'_>],
    ) -> Result<(), ApiError> {
        let body = json!({
            "chat_id": chat,
            "message_id": message,
            "text": text,
            "disable_web_page_preview": true,
            "reply_markup": keyboard(buttons),
        });
        self.call::<Value>("editMessageText", body, CALL_WITHIN)
            .await
            .map(drop)
    }

    /// Tells Telegram a button press was dealt with, so the button stops
    /// showing as busy. `text` is shown briefly to whoever pressed it.
    pub async fn answer_callback(&self, id: &str, text: &str) -> Result<(), ApiError> {
        let body = json!({"callback_query_id": id, "text": text});
        self.call::<Value>("answerCallbackQuery", body, CALL_WITHIN)
            .await
            .map(drop)
    }

    /// Shows "typing…" in the chat for a few seconds.
    pub async fn typing(&self, chat: i64) -> Result<(), ApiError> {
        let body = json!({"chat_id": chat, "action": "typing"});
        self.call::<Value>("sendChatAction", body, CALL_WITHIN)
            .await
            .map(drop)
    }
}

fn keyboard(buttons: &[Button<'_>]) -> Value {
    let row: Vec<Value> = buttons
        .iter()
        .map(|(label, data)| json!({"text": label, "callback_data": data}))
        .collect();
    json!({"inline_keyboard": if row.is_empty() { vec![] } else { vec![row] }})
}

/// Splits text into pieces a message can carry, at line breaks where it can.
pub fn pieces(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut length = 0;
    for line in text.split_inclusive('\n') {
        let chars = line.chars().count();
        if length + chars > MESSAGE_CHARS && !piece.is_empty() {
            pieces.push(std::mem::take(&mut piece));
            length = 0;
        }
        if chars <= MESSAGE_CHARS {
            piece.push_str(line);
            length += chars;
            continue;
        }
        // One line longer than a whole message is cut where it has to be.
        for c in line.chars() {
            if length == MESSAGE_CHARS {
                pieces.push(std::mem::take(&mut piece));
                length = 0;
            }
            piece.push(c);
            length += 1;
        }
    }
    if !piece.trim().is_empty() {
        pieces.push(piece);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_are_read_with_whatever_telegram_leaves_out() {
        let update: Update = serde_json::from_value(json!({
            "update_id": 7,
            "message": {
                "message_id": 3,
                "from": {"id": 42, "is_bot": false, "first_name": "Ada", "username": "ada", "language_code": "en"},
                "chat": {"id": 42, "type": "private", "first_name": "Ada"},
                "date": 1_700_000_000,
                "text": "hi"
            }
        }))
        .unwrap();
        let message = update.message.unwrap();
        assert_eq!(
            (message.chat.id, message.chat.kind.as_str()),
            (42, "private")
        );
        assert_eq!(message.from.unwrap().username.as_deref(), Some("ada"));
        assert_eq!(message.text.as_deref(), Some("hi"));

        // A photo has no text; a button press has no message of its own.
        let photo: Update = serde_json::from_value(json!({"update_id": 8, "message": {"message_id": 4, "chat": {"id": 42, "type": "private"}}})).unwrap();
        assert_eq!(photo.message.unwrap().text, None);
        let press: Update = serde_json::from_value(json!({
            "update_id": 9,
            "callback_query": {"id": "q1", "from": {"id": 42, "first_name": "Ada"}, "data": "approve",
                "message": {"message_id": 5, "chat": {"id": 42, "type": "private"}}}
        }))
        .unwrap();
        let callback = press.callback_query.unwrap();
        assert_eq!(
            (
                callback.data.as_deref(),
                callback.message.unwrap().message_id
            ),
            (Some("approve"), 5)
        );
        // Something this version has no use for is still an update.
        let other: Update =
            serde_json::from_value(json!({"update_id": 10, "edited_message": {}})).unwrap();
        assert!(other.message.is_none() && other.callback_query.is_none());
    }

    #[test]
    fn long_text_is_split_at_line_breaks_and_nothing_is_lost() {
        assert_eq!(pieces("short"), ["short"]);
        assert!(pieces("  \n").is_empty());

        let lines: String = (0..400)
            .map(|n| format!("line number {n} of a long answer\n"))
            .collect();
        let split = pieces(&lines);
        assert!(split.len() > 1);
        assert!(
            split
                .iter()
                .all(|piece| piece.chars().count() <= MESSAGE_CHARS)
        );
        assert!(split.iter().all(|piece| piece.ends_with('\n')));
        assert_eq!(split.concat(), lines);

        let one_line = "é".repeat(MESSAGE_CHARS * 2 + 10);
        let split = pieces(&one_line);
        assert_eq!(split.len(), 3);
        assert_eq!(split.concat(), one_line);
    }

    #[test]
    fn buttons_are_one_row_and_no_buttons_clears_them() {
        assert_eq!(
            keyboard(&[("Approve", "approve"), ("Deny", "deny")]),
            json!({"inline_keyboard": [[{"text": "Approve", "callback_data": "approve"}, {"text": "Deny", "callback_data": "deny"}]]})
        );
        assert_eq!(keyboard(&[]), json!({"inline_keyboard": []}));
    }
}

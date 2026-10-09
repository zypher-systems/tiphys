//! A stand-in for Telegram's Bot API, for tests: a server on a loopback
//! port that knows one token, hands out the updates it is given, and keeps
//! what it was sent.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The one token the fake accepts.
pub const TOKEN: &str = "123456:test-token";
/// How long a test waits for something before it fails.
const PATIENCE: Duration = Duration::from_secs(10);

#[derive(Default)]
struct State {
    updates: Vec<Value>,
    next_update: i64,
    next_message: i64,
    /// The offset of the last request for updates.
    asked_from: i64,
    /// How many requests for updates were willing to wait.
    long_polls: u64,
    /// Every call but the requests for updates: the method and its body.
    calls: Vec<(String, Value)>,
}

#[derive(Clone)]
pub struct Fake {
    pub base: String,
    state: Arc<Mutex<State>>,
}

impl Fake {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Self {
            base: format!("http://{}", listener.local_addr().unwrap()),
            state: Arc::new(Mutex::new(State {
                next_update: 100,
                next_message: 1000,
                ..State::default()
            })),
        };
        let state = fake.state.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(answer(socket, state.clone()));
            }
        });
        fake
    }

    fn push(&self, update: impl FnOnce(i64) -> Value) -> i64 {
        let mut state = self.state.lock().unwrap();
        let id = state.next_update;
        state.next_update += 1;
        state.updates.push(update(id));
        id
    }

    /// A user writes to the bot in their private chat.
    pub fn say(&self, user: i64, text: &str) -> i64 {
        self.say_in(user, user, "private", text)
    }

    pub fn say_in(&self, user: i64, chat: i64, kind: &str, text: &str) -> i64 {
        self.push(|id| {
            json!({"update_id": id, "message": {
                "message_id": id, "date": 0, "text": text,
                "from": {"id": user, "is_bot": false, "first_name": format!("User{user}"), "username": format!("user{user}")},
                "chat": {"id": chat, "type": kind},
            }})
        })
    }

    /// A user presses a button under a message in their private chat.
    pub fn press(&self, user: i64, message: i64, data: &str) -> i64 {
        self.push(|id| {
            json!({"update_id": id, "callback_query": {
                "id": format!("q{id}"), "data": data,
                "from": {"id": user, "is_bot": false, "first_name": format!("User{user}")},
                "message": {"message_id": message, "date": 0, "chat": {"id": user, "type": "private"}},
            }})
        })
    }

    /// The bodies of the calls of one method so far.
    pub fn calls(&self, method: &str) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        state
            .calls
            .iter()
            .filter(|(called, _)| called == method)
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// The texts sent to a chat so far.
    pub fn said(&self, chat: i64) -> Vec<String> {
        self.calls("sendMessage")
            .iter()
            .filter(|body| body["chat_id"] == chat)
            .map(|body| body["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Waits for a call of `method` whose body passes `wanted`.
    pub async fn wait(&self, method: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        let found = async {
            loop {
                if let Some(body) = self.calls(method).into_iter().find(|body| wanted(body)) {
                    return body;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        match tokio::time::timeout(PATIENCE, found).await {
            Ok(body) => body,
            Err(_) => panic!(
                "no such {method}; the calls were {:#?}",
                self.state.lock().unwrap().calls
            ),
        }
    }

    /// Waits for a message to a chat that contains `text`, and returns its id.
    pub async fn wait_said(&self, chat: i64, text: &str) -> i64 {
        let body = self
            .wait("sendMessage", |body| {
                body["chat_id"] == chat
                    && body["text"]
                        .as_str()
                        .is_some_and(|said| said.contains(text))
            })
            .await;
        body["sent_as"].as_i64().unwrap()
    }

    /// Waits until every update given so far has been taken and dealt with:
    /// the bot has asked for the ones after them.
    pub async fn settled(&self) {
        let caught_up = async {
            loop {
                {
                    let state = self.state.lock().unwrap();
                    if state.asked_from >= state.next_update {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(PATIENCE, caught_up)
            .await
            .expect("the bot did not read its updates");
    }

    /// How many requests for updates were willing to wait so far.
    pub fn long_polls(&self) -> u64 {
        self.state.lock().unwrap().long_polls
    }

    /// Waits until the bot has asked for updates and is willing to wait for
    /// them more than `before` times: it is past what was waiting for it.
    pub async fn listening(&self, before: u64) {
        let asked = async {
            while self.long_polls() <= before {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(PATIENCE, asked)
            .await
            .expect("the bot did not start listening");
    }

    /// The offset of the last request for updates.
    pub fn asked_from(&self) -> i64 {
        self.state.lock().unwrap().asked_from
    }
}

async fn answer(mut socket: TcpStream, state: Arc<Mutex<State>>) {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    let (head, body) = loop {
        let Ok(n) = socket.read(&mut buf).await else {
            return;
        };
        request.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&request);
        if let Some(head_end) = text.find("\r\n\r\n") {
            let length = text[..head_end]
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= head_end + 4 + length {
                break (
                    text[..head_end].to_string(),
                    request[head_end + 4..].to_vec(),
                );
            }
        }
        if n == 0 {
            return;
        }
    };
    // `POST /bot<token>/<method> HTTP/1.1`
    let path = head.split_whitespace().nth(1).unwrap_or_default();
    let (token, method) = path
        .trim_start_matches("/bot")
        .split_once('/')
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (status, answer) = if token == TOKEN {
        (
            "200 OK",
            json!({"ok": true, "result": result(method, body, &state).await}),
        )
    } else {
        (
            "401 Unauthorized",
            json!({"ok": false, "error_code": 401, "description": "Unauthorized"}),
        )
    };
    let answer = answer.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
        answer.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn result(method: &str, mut body: Value, state: &Mutex<State>) -> Value {
    match method {
        "getMe" => {
            json!({"id": 1, "is_bot": true, "first_name": "Tiphys", "username": "tiphys_bot"})
        }
        "getUpdates" => {
            let offset = body["offset"].as_i64().unwrap_or(0);
            let patient = body["timeout"].as_u64().unwrap_or(0) > 0;
            if patient {
                state.lock().unwrap().long_polls += 1;
            }
            // A long poll: hold the request until there is something to say.
            loop {
                {
                    let mut state = state.lock().unwrap();
                    state.asked_from = offset;
                    let fresh: Vec<Value> = state
                        .updates
                        .iter()
                        .filter(|update| update["update_id"].as_i64() >= Some(offset))
                        .cloned()
                        .collect();
                    if !fresh.is_empty() || !patient {
                        return Value::Array(fresh);
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        "sendMessage" => {
            let mut state = state.lock().unwrap();
            let id = state.next_message;
            state.next_message += 1;
            let chat = body["chat_id"].clone();
            body["sent_as"] = json!(id);
            state.calls.push((method.to_string(), body));
            json!({"message_id": id, "date": 0, "chat": {"id": chat, "type": "private"}})
        }
        _ => {
            state.lock().unwrap().calls.push((method.to_string(), body));
            json!(true)
        }
    }
}

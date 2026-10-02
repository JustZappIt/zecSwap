use std::time::Duration;

use anyhow::{Result, ensure};
use reqwest::Client;
use serde::Deserialize;
use zeroize::Zeroizing;

pub(crate) struct Telegram {
    credentials: Option<Credentials>,
    client: Client,
    base: String,
}

struct Credentials {
    token: Zeroizing<String>,
    chat: Zeroizing<String>,
}

pub(crate) struct SendError {
    pub message: String,
    pub retry_after: Option<u64>,
}

impl SendError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_after: None,
        }
    }
}

#[derive(Deserialize)]
struct Reply {
    ok: bool,
    result: Option<Message>,
    parameters: Option<Parameters>,
}

#[derive(Deserialize)]
struct Message {
    message_id: i64,
}

#[derive(Deserialize)]
struct Parameters {
    retry_after: Option<u64>,
}

impl Telegram {
    pub(crate) fn from_env() -> Result<Self> {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").ok().map(Zeroizing::new);
        let chat = std::env::var("TELEGRAM_CHAT_ID").ok().map(Zeroizing::new);
        Self::new(
            token.as_deref().map(String::as_str),
            chat.as_deref().map(String::as_str),
        )
    }

    pub(crate) fn new(token: Option<&str>, chat: Option<&str>) -> Result<Self> {
        let credentials = match (token, chat) {
            (None, None) => None,
            (Some(token), Some(chat)) => {
                ensure!(
                    !token.is_empty()
                        && token
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b":_-".contains(&b)),
                    "invalid TELEGRAM_BOT_TOKEN"
                );
                ensure!(
                    chat.parse::<i64>().is_ok(),
                    "TELEGRAM_CHAT_ID must be a numeric chat ID"
                );
                Some(Credentials {
                    token: Zeroizing::new(token.into()),
                    chat: Zeroizing::new(chat.into()),
                })
            }
            _ => anyhow::bail!("set both TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID to enable alerts"),
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| anyhow::anyhow!("could not initialize Telegram client"))?;
        Ok(Self {
            credentials,
            client,
            base: "https://api.telegram.org".into(),
        })
    }

    pub(crate) fn enabled(&self) -> bool {
        self.credentials.is_some()
    }

    pub(crate) async fn send(&self, text: &str) -> std::result::Result<i64, SendError> {
        let credentials = self
            .credentials
            .as_ref()
            .ok_or_else(|| SendError::new("Telegram alerts disabled"))?;
        if text.is_empty() || text.chars().count() > 4096 {
            return Err(SendError::new("Telegram message length invalid"));
        }
        // Never propagate reqwest errors: their request URL contains the bot token.
        let mut response = self
            .client
            .post(format!(
                "{}/bot{}/sendMessage",
                self.base,
                credentials.token.as_str()
            ))
            .header("content-type", "application/json")
            .body(
                serde_json::json!({ "chat_id": credentials.chat.as_str(), "text": text,
                "link_preview_options": { "is_disabled": true } })
                .to_string(),
            )
            .send()
            .await
            .map_err(|_| SendError::new("Telegram request failed or timed out"))?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| SendError::new("Telegram response unreadable"))?
        {
            if body.len() + chunk.len() > 32_768 {
                return Err(SendError::new("Telegram response too large"));
            }
            body.extend_from_slice(&chunk);
        }
        let reply: Option<Reply> = serde_json::from_slice(&body).ok();
        if status.is_success()
            && let Some(reply) = &reply
            && reply.ok
            && let Some(message) = &reply.result
            && message.message_id > 0
        {
            return Ok(message.message_id);
        }
        // Provider descriptions can echo request data; expose only a status code.
        Err(SendError {
            message: if status.is_success() {
                "Telegram did not acknowledge delivery".into()
            } else {
                format!("Telegram rejected request (HTTP {})", status.as_u16())
            },
            retry_after: reply.and_then(|r| r.parameters).and_then(|p| p.retry_after),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, routing::post};

    #[tokio::test]
    async fn checks_acknowledgements_respects_rate_limits_and_redacts_errors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/bot123:secret/sendMessage", post(|Json(body): Json<serde_json::Value>| async move {
            assert_eq!(body["chat_id"], "1234");
            assert_eq!(body["link_preview_options"]["is_disabled"], true);
            match body["text"].as_str().unwrap() {
                "ok" => (StatusCode::OK, Json(serde_json::json!({"ok": true, "result": {"message_id": 7}}))),
                "rate" => (StatusCode::TOO_MANY_REQUESTS, Json(serde_json::json!({"ok": false, "description": "123:secret", "parameters": {"retry_after": 30}}))),
                _ => (StatusCode::OK, Json(serde_json::json!({"ok": false, "description": "123:secret"}))),
            }
        }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut telegram = Telegram::new(Some("123:secret"), Some("1234")).unwrap();
        telegram.base = format!("http://{address}");
        assert_eq!(telegram.send("ok").await.ok(), Some(7));
        let error = telegram.send("rate").await.err().unwrap();
        assert_eq!(error.retry_after, Some(30));
        assert!(!error.message.contains("secret"));
        let error = telegram.send("fail").await.err().unwrap();
        assert_eq!(error.message, "Telegram did not acknowledge delivery");
        task.abort();
        let error = telegram.send("ok").await.err().unwrap();
        assert!(!error.message.contains("secret"));
        assert!(!error.message.contains("http"));
    }

    #[tokio::test]
    async fn disabled_and_partial_credentials_do_not_send() {
        let telegram = Telegram::new(None, None).unwrap();
        assert!(!telegram.enabled());
        assert!(telegram.send("test").await.is_err());
        assert!(Telegram::new(Some("secret"), None).is_err());
        assert!(Telegram::new(Some("secret"), Some("not-a-chat")).is_err());
    }
}

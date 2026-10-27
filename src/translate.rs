//! Chat Completions による字幕翻訳（原文 → 日本語）

use reqwest::Client;
use serde::{Deserialize, Serialize};

pub const TRANSLATE_MODEL_DEFAULT: &str = "muse-spark-1.3";

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

pub fn chat_request_json(model: &str, text: &str) -> String {
    serde_json::to_string(&ChatRequest {
        model,
        messages: vec![
            ChatMessage {
                role: "system",
                content: "You translate video subtitles into Japanese. Output only the Japanese translation, no explanations, no quotation marks.",
            },
            ChatMessage {
                role: "user",
                content: text,
            },
        ],
        temperature: 0.0,
        max_tokens: 500,
    })
    .expect("chat request serialization")
}

/// Chat Completions レスポンスから本文を取り出す
pub fn parse_chat_content(body: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Resp {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: Msg,
    }
    #[derive(Deserialize)]
    struct Msg {
        content: String,
    }

    let resp: Resp = serde_json::from_str(body).map_err(|e| format!("bad chat response: {e}"))?;
    let content = resp
        .choices
        .first()
        .map(|c| c.message.content.trim().to_string())
        .unwrap_or_default();
    if content.is_empty() {
        return Err("empty chat response".to_string());
    }
    Ok(content)
}

pub async fn translate_to_japanese(
    client: &Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    text: &str,
) -> Result<String, String> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .header("Content-Type", "application/json")
        .body(chat_request_json(model, text))
        .send()
        .await
        .map_err(|e| format!("translate request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("translate read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("translate HTTP {status}: {body}"));
    }
    parse_chat_content(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_shape() {
        let v: serde_json::Value =
            serde_json::from_str(&chat_request_json("m1", "Hello")).unwrap();
        assert_eq!(v["model"], "m1");
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][1]["content"], "Hello");
        assert_eq!(v["temperature"], 0.0);
    }

    #[test]
    fn parses_chat_response() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"  こんにちは  "}}]}"#;
        assert_eq!(parse_chat_content(body).unwrap(), "こんにちは");
    }

    #[test]
    fn rejects_empty_response() {
        assert!(parse_chat_content(r#"{"choices":[]}"#).is_err());
        assert!(parse_chat_content("not json").is_err());
    }
}

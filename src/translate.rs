//! Chat Completions による字幕翻訳
//!
//! [`TranslationSession`] が直近の翻訳ペアを履歴として保持し、
//! 用語・代名詞の一貫性を保つ。

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::language::{Language, SourceLanguage};

pub const TRANSLATE_MODEL_DEFAULT: &str = "muse-spark-1.3";
/// プロンプトに含める履歴ペア数の上限
const MAX_HISTORY: usize = 6;

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
    /// muse-spark は推論モデル：既定だと短文にも数百tokenの推論を使い、
    /// max_tokens を食い尽くして content=null になる。字幕翻訳は minimal で十分
    reasoning_effort: String,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

/// 文脈保持つき翻訳セッション。出力言語が変わったら履歴を捨てる
#[derive(Default)]
pub struct TranslationSession {
    history: VecDeque<(String, String)>,
    last_target: Option<Language>,
}

impl TranslationSession {
    pub fn build_request_json(
        &mut self,
        model: &str,
        source: &SourceLanguage,
        target: &Language,
        text: &str,
    ) -> String {
        if let Some(last) = self.last_target {
            if last != *target {
                self.history.clear();
            }
        }
        self.last_target = Some(*target);
        let mut messages = vec![ChatMessage {
            role: "system".to_string(),
            content: system_prompt(source, target),
        }];
        for (original, translated) in &self.history {
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: original.clone(),
            });
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: translated.clone(),
            });
        }
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: text.to_string(),
        });
        serde_json::to_string(&ChatRequest {
            model: model.to_string(),
            messages,
            temperature: 0.0,
            max_tokens: 500,
            reasoning_effort: "minimal".to_string(),
        })
        .expect("chat request serialization")
    }

    pub fn push_history(&mut self, original: String, translated: String) {
        self.history.push_back((original, translated));
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    async fn post_chat(
        client: &Client,
        base_url: &str,
        api_key: &str,
        body: String,
    ) -> Result<String, String> {
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let resp = client
            .post(&url)
            .timeout(std::time::Duration::from_secs(30))
            .bearer_auth(api_key)
            .header("Content-Type", "application/json")
            .body(body)
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

    #[allow(clippy::too_many_arguments)]
    pub async fn translate_next(
        &mut self,
        client: &Client,
        base_url: &str,
        api_key: &str,
        model: &str,
        source: &SourceLanguage,
        target: &Language,
        text: &str,
    ) -> Result<String, String> {
        let translated = Self::post_chat(
            client,
            base_url,
            api_key,
            self.build_request_json(model, source, target, text),
        )
        .await?;
        self.push_history(text.to_string(), translated.clone());
        Ok(translated)
    }

    /// 途中表示用の翻訳。履歴には積まず、確定時に上書きされる
    #[allow(clippy::too_many_arguments)]
    pub async fn translate_preview(
        &mut self,
        client: &Client,
        base_url: &str,
        api_key: &str,
        model: &str,
        source: &SourceLanguage,
        target: &Language,
        text: &str,
    ) -> Result<String, String> {
        Self::post_chat(
            client,
            base_url,
            api_key,
            self.build_request_json(model, source, target, text),
        )
        .await
    }
}

fn system_prompt(source: &SourceLanguage, target: &Language) -> String {
    let name = target.api_name();
    let base = format!(
        "You translate video subtitles into {name}. Output only the {name} translation, no explanations, no quotation marks."
    );
    match source {
        SourceLanguage::Auto => base,
        SourceLanguage::Specific(l) => format!("The source language is {}. {base}", l.api_name()),
    }
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
        finish_reason: Option<String>,
    }
    #[derive(Deserialize)]
    struct Msg {
        content: Option<String>,
    }

    let resp: Resp = serde_json::from_str(body).map_err(|e| format!("bad chat response: {e}"))?;
    let choice = resp.choices.first().ok_or_else(|| "empty chat response".to_string())?;
    let content = choice.message.content.clone().unwrap_or_default();
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err(format!(
            "empty content (finish_reason={})",
            choice.finish_reason.as_deref().unwrap_or("?")
        ));
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_shape() {
        let mut s = TranslationSession::default();
        let v: serde_json::Value = serde_json::from_str(&s.build_request_json(
            "m1",
            &SourceLanguage::Auto,
            &Language::Japanese,
            "Hello",
        ))
        .unwrap();
        assert_eq!(v["model"], "m1");
        assert_eq!(v["messages"][0]["role"], "system");
        assert!(v["messages"][0]["content"].as_str().unwrap().contains("Japanese"));
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"], "Hello");
        assert_eq!(v["temperature"], 0.0);
        assert_eq!(v["reasoning_effort"], "minimal");
    }

    #[test]
    fn history_is_sent_as_prior_turns() {
        let mut s = TranslationSession::default();
        s.push_history("Hi".into(), "やあ".into());
        let v: serde_json::Value = serde_json::from_str(&s.build_request_json(
            "m1",
            &SourceLanguage::Auto,
            &Language::Japanese,
            "Bye",
        ))
        .unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1]["content"], "Hi");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "やあ");
        assert_eq!(msgs[3]["content"], "Bye");
    }

    #[test]
    fn history_capped_and_cleared_on_target_change() {
        let mut s = TranslationSession::default();
        // まず日本語で使い始める
        let _ = s.build_request_json("m1", &SourceLanguage::Auto, &Language::Japanese, "seed");
        for i in 0..10 {
            s.push_history(format!("o{i}"), format!("t{i}"));
        }
        // 上限6ペア＝1 + 6*2 + 1メッセージ
        let v: serde_json::Value = serde_json::from_str(&s.build_request_json(
            "m1",
            &SourceLanguage::Auto,
            &Language::Japanese,
            "next",
        ))
        .unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 1 + MAX_HISTORY * 2 + 1);
        // 別言語で組み立てると履歴が捨てられる
        let v: serde_json::Value = serde_json::from_str(&s.build_request_json(
            "m1",
            &SourceLanguage::Auto,
            &Language::English,
            "Bonjour",
        ))
        .unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn source_hint_in_system_prompt() {
        let mut s = TranslationSession::default();
        let v: serde_json::Value = serde_json::from_str(&s.build_request_json(
            "m1",
            &SourceLanguage::Specific(Language::English),
            &Language::Japanese,
            "Hello",
        ))
        .unwrap();
        let sys = v["messages"][0]["content"].as_str().unwrap();
        assert!(sys.contains("English"), "{sys}");
        assert!(sys.contains("Japanese"), "{sys}");
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

    #[test]
    fn null_content_reports_finish_reason() {
        let body = r#"{"choices":[{"finish_reason":"length","message":{"content":null,"role":"assistant"}}]}"#;
        let err = parse_chat_content(body).unwrap_err();
        assert!(err.contains("length"), "{err}");
    }
}

//! Chat Completions による字幕翻訳
//!
//! [`TranslationSession`] が直近の翻訳ペアを履歴として保持し、
//! 用語・代名詞の一貫性を保つ。応答はストリーミングで受け、
//! 届き次第表示する（初回トークンで反応を見せる）。

use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::language::{Language, SourceLanguage};

pub const TRANSLATE_MODEL_DEFAULT: &str = "muse-spark-1.3";
/// プロンプトに含める履歴ペア数の上限
const MAX_HISTORY: usize = 6;
/// ストリーミング途中表示の間引き間隔。初回と最終は必ず出す
const STREAM_PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
    /// muse-spark は推論モデル：既定だと短文にも数百tokenの推論を使い、
    /// max_tokens を食い尽くして content=null になる。字幕翻訳は minimal で十分
    reasoning_effort: String,
    stream: bool,
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
    /// 履歴と出力言語の記憶を捨てる。言語切替・セッション再開用
    pub fn reset(&mut self) {
        self.history.clear();
        self.last_target = None;
    }

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
            stream: true,
        })
        .expect("chat request serialization")
    }

    pub fn push_history(&mut self, original: String, translated: String) {
        self.history.push_back((original, translated));
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    /// ストリーミング翻訳。増分が届くたび `on_progress` に全文を渡す
    /// （初回即時＋間引き＋最終）。戻り値は確定全文。空のまま終わったらエラー
    pub(crate) async fn post_chat_stream<F>(
        client: &Client,
        base_url: &str,
        api_key: &str,
        body: String,
        mut on_progress: F,
    ) -> Result<String, String>
    where
        F: FnMut(&str, bool),
    {
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
        if !status.is_success() {
            let body = resp.text().await.map_err(|e| format!("translate read failed: {e}"))?;
            return Err(format!("translate HTTP {status}: {body}"));
        }
        let mut buf = SseBuffer::default();
        let mut acc = String::new();
        let mut last_progress: Option<Instant> = None;
        let mut bytes = resp.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| format!("translate stream failed: {e}"))?;
            for line in buf.feed(&chunk) {
                match parse_stream_delta(&line) {
                    StreamDelta::Content(d) => {
                        acc.push_str(&d);
                        let first = last_progress.is_none();
                        if first
                            || last_progress
                                .is_some_and(|at| at.elapsed() >= STREAM_PROGRESS_INTERVAL)
                        {
                            last_progress = Some(Instant::now());
                            on_progress(acc.trim(), false);
                        }
                    }
                    StreamDelta::Done => {
                        let acc = acc.trim().to_string();
                        if acc.is_empty() {
                            return Err("empty content (stream)".to_string());
                        }
                        on_progress(&acc, true);
                        return Ok(acc);
                    }
                    StreamDelta::Ignored => {}
                }
            }
        }
        Err("truncated stream (no [DONE])".to_string())
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
        let translated = Self::post_chat_stream(
            client,
            base_url,
            api_key,
            self.build_request_json(model, source, target, text),
            |_, _| {},
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
        Self::post_chat_stream(
            client,
            base_url,
            api_key,
            self.build_request_json(model, source, target, text),
            |_, _| {},
        )
        .await
    }
}

/// SSEの1行の分類結果
#[derive(Debug, PartialEq)]
enum StreamDelta {
    Content(String),
    Done,
    Ignored,
}

/// SSEの1行から増分テキストを取り出す。終端・役割行・空行・ゴミは捨てる
fn parse_stream_delta(line: &str) -> StreamDelta {
    let Some(payload) = line.strip_prefix("data:") else {
        return StreamDelta::Ignored;
    };
    let payload = payload.trim();
    if payload == "[DONE]" {
        return StreamDelta::Done;
    }
    if payload.is_empty() {
        return StreamDelta::Ignored;
    }
    let content = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            v.get("choices")?
                .as_array()?
                .first()?
                .get("delta")?
                .get("content")?
                .as_str()
                .map(|s| s.to_string())
        });
    match content {
        Some(c) if !c.is_empty() => StreamDelta::Content(c),
        _ => StreamDelta::Ignored,
    }
}

/// SSEの行バッファ。受信チャンクの境界をまたぐ行・UTF-8を復元する
#[derive(Default)]
struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    /// 受信分を追加し、完成した行を返す。末尾の未完分は保持する
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let mut line = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
            if let Some(stripped) = line.strip_suffix('\r') {
                line = stripped.to_string();
            }
            lines.push(line);
        }
        lines
    }
}

fn system_prompt(source: &SourceLanguage, target: &Language) -> String {
    let name = target.api_name();
    let base = format!(
        "You translate video subtitles into {name}. Translate every sentence completely. Do not omit, merge, or summarize any part. Output only the {name} translation, no explanations, no quotation marks."
    );
    match source {
        SourceLanguage::Auto => base,
        SourceLanguage::Specific(l) => format!("The source language is {}. {base}", l.api_name()),
    }
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
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn parses_stream_deltas() {
        // 増分テキスト
        assert_eq!(
            parse_stream_delta(r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#),
            StreamDelta::Content("Hello".to_string())
        );
        // 終端
        assert_eq!(parse_stream_delta("data: [DONE]"), StreamDelta::Done);
        assert_eq!(parse_stream_delta("data:[DONE]"), StreamDelta::Done);
        // 役割行・終了理由だけの行は無視する
        assert_eq!(
            parse_stream_delta(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#),
            StreamDelta::Ignored
        );
        assert_eq!(
            parse_stream_delta(r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            StreamDelta::Ignored
        );
        // コメント・空行・ゴミは無視する
        assert_eq!(parse_stream_delta(": keep-alive"), StreamDelta::Ignored);
        assert_eq!(parse_stream_delta(""), StreamDelta::Ignored);
        assert_eq!(parse_stream_delta("data: not json"), StreamDelta::Ignored);
    }

    #[test]
    fn sse_buffer_splits_lines_across_chunks() {
        let mut b = SseBuffer::default();
        // チャンク境界で行が割れても復元する
        assert!(b.feed(b"data: {\"a\"").is_empty());
        assert_eq!(
            b.feed(b":1}\n\ndata: [DONE]\r\n"),
            vec!["data: {\"a\":1}", "", "data: [DONE]"]
        );
        // 日本語のUTF-8が割れても復元する
        let mut b = SseBuffer::default();
        let bytes = "data: あ\n".as_bytes();
        assert!(b.feed(&bytes[..8]).is_empty());
        assert_eq!(b.feed(&bytes[8..]), vec!["data: あ"]);
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
    fn reset_clears_history_and_target() {
        let mut s = TranslationSession::default();
        let _ = s.build_request_json("m1", &SourceLanguage::Auto, &Language::Japanese, "seed");
        s.push_history("o1".into(), "t1".into());
        s.reset();
        // リセット後は履歴なしの2メッセージになる
        let v: serde_json::Value = serde_json::from_str(&s.build_request_json(
            "m1",
            &SourceLanguage::Auto,
            &Language::Japanese,
            "next",
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

}

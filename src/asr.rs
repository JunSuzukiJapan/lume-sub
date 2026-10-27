//! Meta Model API リアルタイム音声転写クライアント
//!
//! `wss://api.meta.ai/v1/asr/realtime` に接続し、48kHz float32 mono の
//! [`AudioFrame`](crate::audio::AudioFrame) を 24kHz PCM16 に変換して
//! 80ms フレームで送信、サーバーイベントを [`AsrEvent`] として流す。
//! 仕様: <https://dev.meta.ai/docs/speech-to-text>

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::audio::{AudioFrame, f32_to_pcm16le_bytes, resample_48k_to_24k};

pub const ASR_MODEL_DEFAULT: &str = "muse-voice-transcribe-1.0";
const EXPECTED_INPUT_RATE: u32 = 48_000;
const FRAME_MS: u64 = 80;
const FRAME_SAMPLES_24K: usize = 24_000 * FRAME_MS as usize / 1000; // 1920
const FRAME_BYTES: usize = FRAME_SAMPLES_24K * 2; // 3840
const SILENCE_FRAME: [u8; FRAME_BYTES] = [0u8; FRAME_BYTES];

pub struct AsrConfig {
    pub ws_url: String,
    pub api_key: String,
    pub model: String,
}

/// ASR セッションから上流へ流すイベント
pub enum AsrEvent {
    /// サーバーとハンドシェイク完了（表示用ではなくログ用）
    Session(String),
    /// 話し下し中の暫定テキスト（置換表示用）
    Partial(String),
    /// 1ターンの確定テキスト
    Complete { turn_id: i32, transcript: String },
    /// サーバーの error イベント
    Error(String),
}

/// 1セッションの終了理由
pub enum SessionEnd {
    /// 音声入力側が閉じた（終了処理へ）
    InputClosed,
    /// サーバーがソケットを閉じた
    ServerClose { code: Option<u16> },
    /// 接続・送受信の失敗
    TransportError(String),
}

#[derive(Serialize)]
struct Handshake<'a> {
    authorization: Auth<'a>,
    #[serde(rename = "audioEncoding")]
    audio_encoding: &'static str,
    model: &'a str,
    mode: &'static str,
    #[serde(rename = "partialMode")]
    partial_mode: &'static str,
    #[serde(rename = "emitAudioProgress")]
    emit_audio_progress: bool,
}

#[derive(Serialize)]
struct Auth<'a> {
    #[serde(rename = "accessToken")]
    access_token: &'a str,
}

fn handshake_json(api_key: &str, model: &str) -> String {
    let token = format!("Bearer {api_key}");
    serde_json::to_string(&Handshake {
        authorization: Auth {
            access_token: &token,
        },
        audio_encoding: "PCM_24KHZ",
        model,
        mode: "ENDPOINTING",
        partial_mode: "CUMULATIVE",
        emit_audio_progress: false,
    })
    .expect("handshake serialization")
}

/// サーバーイベントの分類結果
#[derive(Debug, PartialEq)]
enum ParsedEvent {
    Transcript(String),
    SpeechStart(i32),
    SpeechComplete { turn_id: i32, transcript: String },
    ServerError(String),
    /// ハンドシェイク応答・audioProgress・speaker・未知タイプ等
    Ignored,
}

/// サーバーの JSON テキストフレームを分類する。未知タイプは無視する
fn parse_server_event(text: &str) -> ParsedEvent {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return ParsedEvent::Ignored,
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("transcript") => match v.get("transcript").and_then(|t| t.as_str()) {
            Some(t) => ParsedEvent::Transcript(t.to_string()),
            None => ParsedEvent::Ignored,
        },
        Some("speechStart") => match v.get("turnId").and_then(|t| t.as_i64()) {
            Some(id) => ParsedEvent::SpeechStart(id as i32),
            None => ParsedEvent::Ignored,
        },
        Some("speechComplete") => {
            let id = v.get("turnId").and_then(|t| t.as_i64());
            let t = v.get("transcript").and_then(|t| t.as_str());
            match (id, t) {
                (Some(id), Some(t)) => ParsedEvent::SpeechComplete {
                    turn_id: id as i32,
                    transcript: t.to_string(),
                },
                _ => ParsedEvent::Ignored,
            }
        }
        Some("error") => {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            ParsedEvent::ServerError(msg.to_string())
        }
        // speechEnd / speaker / audioProgress / 未知タイプは字幕に不要
        _ => ParsedEvent::Ignored,
    }
}

/// 1セッションを実行する。終了理由を返す
pub async fn run_session(
    audio_rx: &mut mpsc::Receiver<AudioFrame>,
    event_tx: &mpsc::Sender<AsrEvent>,
    cfg: &AsrConfig,
) -> SessionEnd {
    let (ws, _) = match connect_async(&cfg.ws_url).await {
        Ok(v) => v,
        Err(e) => return SessionEnd::TransportError(format!("connect failed: {e}")),
    };
    let (mut sink, mut stream) = ws.split();

    // 1. ハンドシェイク（最初の JSON テキストフレーム）
    if let Err(e) = sink.send(Message::Text(handshake_json(&cfg.api_key, &cfg.model).into())).await {
        return SessionEnd::TransportError(format!("handshake send failed: {e}"));
    }
    // 2. ack を待ってから音声送信開始
    let session_id = match stream.next().await {
        Some(Ok(Message::Text(t))) => {
            let v: serde_json::Value = serde_json::from_str(&t.to_string()).unwrap_or_default();
            match v.get("sessionId").and_then(|s| s.as_str()) {
                Some(id) => id.to_string(),
                None => return SessionEnd::TransportError(format!("handshake rejected: {t}")),
            }
        }
        Some(Ok(_)) => return SessionEnd::TransportError("unexpected handshake reply".into()),
        Some(Err(e)) => return SessionEnd::TransportError(format!("handshake recv failed: {e}")),
        None => return SessionEnd::TransportError("closed during handshake".into()),
    };
    let _ = event_tx.send(AsrEvent::Session(session_id)).await;

    // 3. 送受信ループ：80ms ティッカーで1フレームずつ送信（リアルタイムペース）
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(FRAME_BYTES * 4);
    let mut ticker = tokio::time::interval(Duration::from_millis(FRAME_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frames_sent: u64 = 0;
    let mut format_logged = false;

    loop {
        tokio::select! {
            biased;
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        match parse_server_event(&t.to_string()) {
                            ParsedEvent::Transcript(text) => {
                                let _ = event_tx.send(AsrEvent::Partial(text)).await;
                            }
                            ParsedEvent::SpeechStart(_id) => {}
                            ParsedEvent::SpeechComplete { turn_id, transcript } => {
                                let _ = event_tx.send(AsrEvent::Complete { turn_id, transcript }).await;
                            }
                            ParsedEvent::ServerError(msg) => {
                                let _ = event_tx.send(AsrEvent::Error(msg)).await;
                            }
                            ParsedEvent::Ignored => {}
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        return SessionEnd::ServerClose { code: frame.map(|f| f.code.into()) };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return SessionEnd::TransportError(format!("recv failed: {e}")),
                    None => return SessionEnd::ServerClose { code: None },
                }
            }
            _ = ticker.tick() => {
                // 手元にある音声を全部取り込む（ブロックしない）
                let mut input_closed = false;
                loop {
                    match audio_rx.try_recv() {
                        Ok(frame) => {
                            if !format_logged {
                                println!(
                                    "🔊 ASR送信開始: {}Hz {}ch → 24kHz PCM16 mono",
                                    frame.sample_rate, frame.channels
                                );
                                format_logged = true;
                            }
                            if frame.sample_rate != EXPECTED_INPUT_RATE {
                                let _ = event_tx.send(AsrEvent::Error(format!(
                                    "unsupported input rate {}Hz (expected {EXPECTED_INPUT_RATE}Hz)",
                                    frame.sample_rate
                                ))).await;
                                continue;
                            }
                            let down = resample_48k_to_24k(&frame.samples);
                            pcm_buf.extend_from_slice(&f32_to_pcm16le_bytes(&down));
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            input_closed = true;
                            break;
                        }
                    }
                }
                if input_closed {
                    // endStream を送ってサーバーの flush を待つ
                    let _ = sink.send(Message::Text(r#"{"type": "endStream"}"#.into())).await;
                    return drain_until_close(&mut stream).await;
                }
                // 1フレーム送信（足りなければ無音パディング）
                let mut out = SILENCE_FRAME;
                let n = pcm_buf.len().min(FRAME_BYTES);
                out[..n].copy_from_slice(&pcm_buf[..n]);
                pcm_buf.drain(..n);
                // 送信が追いつかないほど溜まったら古い分を捨てて遅延を防ぐ
                const MAX_BACKLOG: usize = FRAME_BYTES * 25; // 約2秒
                if pcm_buf.len() > MAX_BACKLOG {
                    let drop = pcm_buf.len() - MAX_BACKLOG;
                    pcm_buf.drain(..drop);
                }
                if sink.send(Message::Binary(out.to_vec().into())).await.is_err() {
                    return SessionEnd::TransportError("send failed".into());
                }
                frames_sent += 1;
                if frames_sent % 375 == 0 {
                    println!("🔊 ASR送信中: {}フレーム（{}秒相当）", frames_sent, frames_sent * FRAME_MS / 1000);
                }
            }
        }
    }
}

/// `endStream` 後にサーバーが閉じるまで受信を続ける
async fn drain_until_close<S>(stream: &mut S) -> SessionEnd
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Close(frame)) => {
                return SessionEnd::ServerClose {
                    code: frame.map(|f| f.code.into()),
                };
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    SessionEnd::InputClosed
}

/// セッションを維持する監督ループ。60分上限・一時的失敗時は再接続する
pub async fn supervise(
    mut audio_rx: mpsc::Receiver<AudioFrame>,
    event_tx: mpsc::Sender<AsrEvent>,
    cfg: AsrConfig,
) {
    let mut backoff_secs = 2u64;
    loop {
        match run_session(&mut audio_rx, &event_tx, &cfg).await {
            SessionEnd::InputClosed => return,
            SessionEnd::ServerClose { code: Some(1000) } => return,
            SessionEnd::ServerClose { code: Some(1008) } => {
                let _ = event_tx
                    .send(AsrEvent::Error("rejected by server (1008): check config/audio".into()))
                    .await;
                return;
            }
            SessionEnd::ServerClose { code } => {
                println!("🔌 ASR切断（code={code:?}）、{backoff_secs}秒後に再接続します");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(60);
            }
            SessionEnd::TransportError(e) => {
                println!("🔌 ASR接続失敗（{e}）、{backoff_secs}秒後に再試行します");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(60);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_shape_matches_docs() {
        let v: serde_json::Value =
            serde_json::from_str(&handshake_json("KEY123", "muse-voice-transcribe-1.0")).unwrap();
        assert_eq!(v["authorization"]["accessToken"], "Bearer KEY123");
        assert_eq!(v["audioEncoding"], "PCM_24KHZ");
        assert_eq!(v["model"], "muse-voice-transcribe-1.0");
        assert_eq!(v["mode"], "ENDPOINTING");
        assert_eq!(v["partialMode"], "CUMULATIVE");
        assert_eq!(v["emitAudioProgress"], false);
    }

    #[test]
    fn parses_wire_example() {
        assert_eq!(
            parse_server_event(r#"{"type":"speechStart","turnId":1,"audioProcessedMs":1200}"#),
            ParsedEvent::SpeechStart(1)
        );
        assert_eq!(
            parse_server_event(
                r#"{"type":"transcript","transcript":"how is the","final":false,"audioProcessedMs":2400}"#
            ),
            ParsedEvent::Transcript("how is the".into())
        );
        assert_eq!(
            parse_server_event(
                r#"{"type":"speechComplete","turnId":1,"transcript":"How is the weather?","audioProcessedMs":3600}"#
            ),
            ParsedEvent::SpeechComplete {
                turn_id: 1,
                transcript: "How is the weather?".into(),
            }
        );
    }

    #[test]
    fn ignores_unneeded_and_unknown_events() {
        assert_eq!(
            parse_server_event(r#"{"type":"speechEnd","turnId":1,"audioProcessedMs":3600}"#),
            ParsedEvent::Ignored
        );
        assert_eq!(
            parse_server_event(r#"{"type":"speaker","label":"A","audioProcessedMs":2480}"#),
            ParsedEvent::Ignored
        );
        assert_eq!(
            parse_server_event(r#"{"type":"audioProgress","audioProcessedMs":1000}"#),
            ParsedEvent::Ignored
        );
        assert_eq!(
            parse_server_event(r#"{"sessionId":"abc"}"#),
            ParsedEvent::Ignored
        );
        assert_eq!(
            parse_server_event(r#"{"type":"futureEvent","x":1}"#),
            ParsedEvent::Ignored
        );
        assert_eq!(parse_server_event("not json"), ParsedEvent::Ignored);
    }

    #[test]
    fn parses_error_event() {
        assert_eq!(
            parse_server_event(r#"{"type":"error","message":"boom","sessionId":"s"}"#),
            ParsedEvent::ServerError("boom".into())
        );
    }
}

//! Meta Model API リアルタイム音声転写クライアント
//!
//! `wss://api.meta.ai/v1/asr/realtime` に接続し、48kHz float32 mono の
//! [`AudioFrame`](crate::audio::AudioFrame) を 24kHz PCM16 に変換して
//! 80ms フレームで送信、サーバーイベントを [`AsrEvent`] として流す。
//! 仕様: <https://dev.meta.ai/docs/speech-to-text>

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::audio::{AudioFrame, f32_to_pcm16le_bytes, resample_48k_to_24k};
use crate::dev_log;
use crate::language::SharedLanguageConfig;

pub const ASR_MODEL_DEFAULT: &str = "muse-voice-transcribe-1.0";
const EXPECTED_INPUT_RATE: u32 = 48_000;
const FRAME_MS: u64 = 80;
const FRAME_SAMPLES_24K: usize = 24_000 * FRAME_MS as usize / 1000; // 1920
const FRAME_BYTES: usize = FRAME_SAMPLES_24K * 2; // 3840
const SILENCE_FRAME: [u8; FRAME_BYTES] = [0u8; FRAME_BYTES];

#[derive(Clone)]
pub struct AsrConfig {
    pub ws_url: String,
    pub api_key: String,
    pub model: String,
}

/// ASR セッションから上流へ流すイベント
pub enum AsrEvent {
    /// サーバーとハンドシェイク完了（表示用ではなくログ用）
    Session(String),
    /// 新ターンの発話開始。途中文の区切り・古い文の破棄に使う
    SpeechStart(i32),
    /// 話し下し中の暫定テキスト（置換表示用）
    Partial(String),
    /// 1ターンの確定テキスト
    Complete { turn_id: i32, transcript: String },
    /// サーバーの error イベント
    Error(String),
    /// 発話区間の切れ目。true=開始、false=終了。認識との突き合わせ用
    SpeechActivity { speaking: bool },
}

/// 1セッションの終了理由
pub enum SessionEnd {
    /// 音声入力側が閉じた（終了処理へ）
    InputClosed,
    /// サーバーがソケットを閉じた
    ServerClose { code: Option<u16> },
    /// 接続・送受信の失敗
    TransportError(String),
    /// 音声があるのに認識イベントが来ない停滞（再接続で回復を試みる）
    Stall { had_progress: bool },
}

/// 停滞判定の音量下限（≈-35dB）。これ以上の音が続くのに認識がなければ停滞
const STALL_MIN_RMS: f32 = 0.018;
/// 停滞とみなす無進展の秒数。通常の初回 partial（約2秒）の2倍。
/// アラビア語のように認識したりしなかったりする言語の取りこぼしを
/// 区間内に救うため、短めに検出する
const STALL_AFTER: Duration = Duration::from_secs(4);
/// 無音がこの秒数続いたら有音の計数を振り出しに戻す。
/// 音声供給の瞬断では数え直さないための猶予
const QUIET_RESET_AFTER: Duration = Duration::from_millis(500);

/// 認識停滞の検知。大きな音が続くのに認識イベントが来なければ再接続する。
/// ソケットは開いたままサーバーが黙る場合（言語検出の不調など）の表面化と回復用
struct StallMonitor {
    loud_since: Option<Instant>,
    quiet_since: Option<Instant>,
    last_progress: Instant,
    max_turn: i32,
    progressed: bool,
}

impl StallMonitor {
    fn new(now: Instant) -> Self {
        Self {
            loud_since: None,
            quiet_since: None,
            last_progress: now,
            max_turn: i32::MIN,
            progressed: false,
        }
    }

    /// このセッションで認識進展があったか
    fn progressed(&self) -> bool {
        self.progressed
    }

    /// 音声の到着。停滞したら真（一度出たら次は4秒後）
    fn note_audio(&mut self, rms: f32, now: Instant) -> bool {
        if rms < STALL_MIN_RMS {
            // 短い無音（供給の瞬断）では振り出しに戻さない
            let quiet = *self.quiet_since.get_or_insert(now);
            if now.duration_since(quiet) >= QUIET_RESET_AFTER {
                self.loud_since = None;
            }
            return false;
        }
        self.quiet_since = None;
        let since = *self.loud_since.get_or_insert(now);
        if now.duration_since(since) >= STALL_AFTER
            && now.duration_since(self.last_progress) >= STALL_AFTER
        {
            self.loud_since = None;
            true
        } else {
            false
        }
    }

    /// 認識進展の到着
    fn note_progress(&mut self, now: Instant) {
        self.last_progress = now;
        self.loud_since = None;
        self.progressed = true;
    }

    /// 暫定文の到着。空文は進展とみなさない
    fn note_transcript(&mut self, text: &str, now: Instant) {
        if !text.trim().is_empty() {
            self.note_progress(now);
        }
    }

    /// 発話開始の到着。ターンが進んだときだけ進展とみなす
    fn note_speech_start(&mut self, turn_id: i32, now: Instant) {
        if turn_id > self.max_turn {
            self.max_turn = turn_id;
            self.note_progress(now);
        }
    }

    /// ターン確定の到着。番号を覚えて進展とする
    fn note_complete(&mut self, turn_id: i32, now: Instant) {
        self.max_turn = self.max_turn.max(turn_id);
        self.note_progress(now);
    }
}

/// 停滞後の再接続までの待ち秒数。進展のあったセッションの停滞は
/// 一時的な検出不調とみて短く、無進展のままなら通常のバックオフに委ねる
fn stall_wait_secs(had_progress: bool, backoff_secs: u64) -> u64 {
    if had_progress { 1 } else { backoff_secs }
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
    #[serde(rename = "languageBias", skip_serializing_if = "Option::is_none")]
    language_bias: Option<Vec<String>>,
}

#[derive(Serialize)]
struct Auth<'a> {
    #[serde(rename = "accessToken")]
    access_token: &'a str,
}

fn handshake_json(api_key: &str, model: &str, language_bias: Option<Vec<String>>) -> String {
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
        language_bias,
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

/// 発話区間の検出。音量としきい値＋ハングオーバーで開始・終了を出す。
/// 記録との突き合わせ用（すべての音声が認識されたかの確認）
struct SpeechTracker {
    speaking: bool,
    quiet_since: Option<Instant>,
}

/// 終了とみなす無音の継続時間。デモの言語間ギャップ（約0.9秒）より短く、
/// 文中の息継ぎより長く取る
const SPEECH_END_AFTER: Duration = Duration::from_millis(800);

impl SpeechTracker {
    fn new() -> Self {
        Self {
            speaking: false,
            quiet_since: None,
        }
    }

    /// 音声の到着。区切りが変わったら Some(true=開始 / false=終了)
    fn note_audio(&mut self, rms: f32, now: Instant) -> Option<bool> {
        if rms >= STALL_MIN_RMS {
            self.quiet_since = None;
            if !self.speaking {
                self.speaking = true;
                return Some(true);
            }
            return None;
        }
        if !self.speaking {
            return None;
        }
        let quiet = *self.quiet_since.get_or_insert(now);
        if now.duration_since(quiet) >= SPEECH_END_AFTER {
            self.speaking = false;
            self.quiet_since = None;
            return Some(false);
        }
        None
    }
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
    langs: &SharedLanguageConfig,
) -> SessionEnd {
    let (ws, _) = match connect_async(&cfg.ws_url).await {
        Ok(v) => v,
        Err(e) => return SessionEnd::TransportError(format!("connect failed: {e}")),
    };
    let (mut sink, mut stream) = ws.split();

    // 1. ハンドシェイク（最初の JSON テキストフレーム）
    let bias = langs.lock().ok().and_then(|c| c.language_bias());
    if let Err(e) = sink.send(Message::Text(handshake_json(&cfg.api_key, &cfg.model, bias).into())).await {
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
    let mut stall = StallMonitor::new(Instant::now());
    let mut speech = SpeechTracker::new();

    loop {
        tokio::select! {
            biased;
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        match parse_server_event(&t.to_string()) {
                            ParsedEvent::Transcript(text) => {
                                stall.note_transcript(&text, Instant::now());
                                let _ = event_tx.send(AsrEvent::Partial(text)).await;
                            }
                            ParsedEvent::SpeechStart(id) => {
                                stall.note_speech_start(id, Instant::now());
                                let _ = event_tx.send(AsrEvent::SpeechStart(id)).await;
                            }
                            ParsedEvent::SpeechComplete { turn_id, transcript } => {
                                stall.note_complete(turn_id, Instant::now());
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
                let mut tick_rms = 0.0f32;
                loop {
                    match audio_rx.try_recv() {
                        Ok(frame) => {
                            tick_rms = tick_rms.max(frame.rms);
                            if !format_logged {
                                dev_log!(
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
                if stall.note_audio(tick_rms, Instant::now()) {
                    dev_log!(
                        "⚠️ ASR停滞: 音声あり・無応答{}秒、再接続します",
                        STALL_AFTER.as_secs()
                    );
                    return SessionEnd::Stall {
                        had_progress: stall.progressed(),
                    };
                }
                // 発話区間の切れ目を知らせる。詰まっていたら捨てる
                // （助言情報のため。欠けても突き合わせは破綻しない）
                if let Some(speaking) = speech.note_audio(tick_rms, Instant::now()) {
                    let _ = event_tx.try_send(AsrEvent::SpeechActivity { speaking });
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
                    dev_log!("🔊 ASR送信中: {}フレーム（{}秒相当）", frames_sent, frames_sent * FRAME_MS / 1000);
                }
            }
        }
    }
}

/// `endStream` 後にサーバーが閉じるまで受信を続ける。
/// 停止処理中のため、閉じ方によらず再接続しない
async fn drain_until_close<S>(stream: &mut S) -> SessionEnd
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    SessionEnd::InputClosed
}

/// 監視ループの継続判定。停止指示・設定不良のときだけ抜ける。
/// 送受信中のサーバー切断（1000含む）は再接続する
fn should_reconnect(end: &SessionEnd) -> bool {
    match end {
        SessionEnd::InputClosed => false,
        SessionEnd::ServerClose { code: Some(1008) } => false,
        SessionEnd::ServerClose { .. } => true,
        SessionEnd::TransportError(_) => true,
        SessionEnd::Stall { .. } => true,
    }
}

/// セッションを維持する監督ループ。60分上限・一時的失敗時は再接続する
pub async fn supervise(
    mut audio_rx: mpsc::Receiver<AudioFrame>,
    event_tx: mpsc::Sender<AsrEvent>,
    cfg: AsrConfig,
    langs: SharedLanguageConfig,
) {
    let mut backoff_secs = 2u64;
    loop {
        let end = run_session(&mut audio_rx, &event_tx, &cfg, &langs).await;
        if !should_reconnect(&end) {
            if matches!(end, SessionEnd::ServerClose { code: Some(1008) }) {
                let _ = event_tx
                    .send(AsrEvent::Error("rejected by server (1008): check config/audio".into()))
                    .await;
            }
            return;
        }
        match &end {
            SessionEnd::ServerClose { code } => {
                // 停止指示済みなら再接続しない
                if audio_rx.is_closed() {
                    return;
                }
                dev_log!("🔌 ASR切断（code={code:?}）、{backoff_secs}秒後に再接続します");
            }
            SessionEnd::TransportError(e) => {
                if audio_rx.is_closed() {
                    return;
                }
                dev_log!("🔌 ASR接続失敗（{e}）、{backoff_secs}秒後に再試行します");
            }
            SessionEnd::Stall { had_progress } => {
                if audio_rx.is_closed() {
                    return;
                }
                let wait = stall_wait_secs(*had_progress, backoff_secs);
                dev_log!("🔌 ASR停滞のため、{wait}秒後に再接続します");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                backoff_secs = (backoff_secs * 2).min(60);
                continue;
            }
            // should_reconnect が偽の場合は上で抜けている
            SessionEnd::InputClosed => return,
        }
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_shape_matches_docs() {
        let v: serde_json::Value =
            serde_json::from_str(&handshake_json("KEY123", "muse-voice-transcribe-1.0", None)).unwrap();
        assert_eq!(v["authorization"]["accessToken"], "Bearer KEY123");
        assert_eq!(v["audioEncoding"], "PCM_24KHZ");
        assert_eq!(v["model"], "muse-voice-transcribe-1.0");
        assert_eq!(v["mode"], "ENDPOINTING");
        assert_eq!(v["partialMode"], "CUMULATIVE");
        assert_eq!(v["emitAudioProgress"], false);
        assert!(v.get("languageBias").is_none());
    }

    #[test]
    fn handshake_carries_language_bias() {
        let v: serde_json::Value = serde_json::from_str(&handshake_json(
            "KEY123",
            "muse-voice-transcribe-1.0",
            Some(vec!["English".to_string()]),
        ))
        .unwrap();
        assert_eq!(v["languageBias"], serde_json::json!(["English"]));
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

    #[tokio::test]
    async fn drain_reports_stopping_on_any_close() {
        use futures_util::stream::{self};
        use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
        async fn drain_with(code: CloseCode) -> SessionEnd {
            let msg = Message::Close(Some(CloseFrame {
                code,
                reason: Default::default(),
            }));
            let mut s = stream::iter(vec![Ok::<_, tokio_tungstenite::tungstenite::Error>(msg)]);
            drain_until_close(&mut s).await
        }
        // 停止処理中の閉じはコードによらず再接続しない
        assert!(matches!(
            drain_with(CloseCode::Normal).await,
            SessionEnd::InputClosed
        ));
        assert!(matches!(
            drain_with(CloseCode::Error).await,
            SessionEnd::InputClosed
        ));
        // 送受信失敗でも停止扱い
        let mut s = stream::iter(vec![Err::<Message, _>(
            tokio_tungstenite::tungstenite::Error::ConnectionClosed,
        )]);
        assert!(matches!(
            drain_until_close(&mut s).await,
            SessionEnd::InputClosed
        ));
    }

    #[test]
    fn reconnect_decision() {
        assert!(!should_reconnect(&SessionEnd::InputClosed));
        assert!(!should_reconnect(&SessionEnd::ServerClose {
            code: Some(1008)
        }));
        // 送受信中の切断は1000でも再接続する
        assert!(should_reconnect(&SessionEnd::ServerClose {
            code: Some(1000)
        }));
        assert!(should_reconnect(&SessionEnd::ServerClose {
            code: Some(1011)
        }));
        assert!(should_reconnect(&SessionEnd::ServerClose { code: None }));
        assert!(should_reconnect(&SessionEnd::TransportError("x".into())));
        assert!(should_reconnect(&SessionEnd::Stall { had_progress: true }));
        assert!(should_reconnect(&SessionEnd::Stall {
            had_progress: false
        }));
    }

    #[test]
    fn stall_fires_after_loud_audio_without_progress() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut m = StallMonitor::new(t0);
        assert!(!m.progressed());
        // 静音では出ない
        assert!(!m.note_audio(0.0, at(1)));
        // 大きな音が続いても4秒未満では出ない
        assert!(!m.note_audio(0.5, at(5)));
        assert!(!m.note_audio(0.5, at(8)));
        // 4秒たったら出る
        assert!(m.note_audio(0.5, at(9)));
        // 一度出たらすぐには出ない（次は4秒後）
        assert!(!m.note_audio(0.5, at(10)));
        assert!(m.note_audio(0.5, at(14)));
    }

    #[test]
    fn stall_resets_on_progress_and_silence() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut m = StallMonitor::new(t0);
        // 3秒経ってから進展があれば振り出しに戻る
        assert!(!m.note_audio(0.5, at(0)));
        m.note_progress(at(3));
        assert!(m.progressed());
        assert!(!m.note_audio(0.5, at(6)));
        assert!(!m.note_audio(0.5, at(9)));
        assert!(m.note_audio(0.5, at(10)));
        // 500ms続く静音で振り出しに戻る
        let mut m = StallMonitor::new(t0);
        assert!(!m.note_audio(0.5, at(0)));
        assert!(!m.note_audio(0.0, at(3)));
        assert!(!m.note_audio(0.0, at(3) + Duration::from_millis(600)));
        assert!(!m.note_audio(0.5, at(6)));
        assert!(!m.note_audio(0.5, at(9)));
        assert!(m.note_audio(0.5, at(10)));
    }

    #[test]
    fn stall_tolerates_brief_gaps_in_loud_audio() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut m = StallMonitor::new(t0);
        // 200msの瞬断を挟んでも計数は続く
        assert!(!m.note_audio(0.5, at(0)));
        assert!(!m.note_audio(0.0, at(1000)));
        assert!(!m.note_audio(0.0, at(1200)));
        assert!(!m.note_audio(0.5, at(2000)));
        assert!(m.note_audio(0.5, at(4000)));
    }

    #[test]
    fn stall_ignores_unproductive_events() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        // 空の暫定文は進展とみなさない
        let mut m = StallMonitor::new(t0);
        m.note_transcript("", at(1));
        m.note_transcript("   ", at(2));
        assert!(!m.progressed());
        m.note_transcript("hi", at(3));
        assert!(m.progressed());
        // 進まない発話開始は時計を戻さない
        let mut m = StallMonitor::new(t0);
        m.note_speech_start(1, at(1));
        assert!(m.progressed());
        assert!(!m.note_audio(0.5, at(2)));
        m.note_speech_start(1, at(3));
        assert!(m.note_audio(0.5, at(6)));
        // 確定より古い開始も進展とみなさない
        let mut m = StallMonitor::new(t0);
        m.note_complete(2, at(1));
        m.note_speech_start(2, at(2));
        assert!(!m.note_audio(0.5, at(2)));
        assert!(m.note_audio(0.5, at(6)));
    }

    #[test]
    fn speech_tracker_marks_utterance_boundaries() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut m = SpeechTracker::new();
        // 初めの無音では何も出ない
        assert_eq!(m.note_audio(0.0, at(0)), None);
        // 有音で開始
        assert_eq!(m.note_audio(0.5, at(100)), Some(true));
        assert_eq!(m.note_audio(0.5, at(200)), None);
        // 短い無音では終わらない
        assert_eq!(m.note_audio(0.0, at(300)), None);
        assert_eq!(m.note_audio(0.0, at(900)), None);
        assert_eq!(m.note_audio(0.5, at(1000)), None);
        // 800ms続く無音で終了
        assert_eq!(m.note_audio(0.0, at(2000)), None);
        assert_eq!(m.note_audio(0.0, at(2800)), Some(false));
        // 次の有音で再び開始
        assert_eq!(m.note_audio(0.5, at(3000)), Some(true));
    }

    #[test]
    fn stall_waits_short_after_progress() {
        // 進展のあったセッションの停滞は一時的とみて1秒で繋ぎ直す
        assert_eq!(stall_wait_secs(true, 2), 1);
        assert_eq!(stall_wait_secs(true, 16), 1);
        // 無進展のままなら通常のバックオフに委ねる
        assert_eq!(stall_wait_secs(false, 2), 2);
        assert_eq!(stall_wait_secs(false, 16), 16);
    }
}

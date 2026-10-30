//! パイプライン制御（開始・一時停止・終了）
//!
//! 字幕オーバーレイはマウス透過のため、操作は別ウィンドウから
//! [`PipelineController`] 経由で行う。
//!
//! - Start: 停止中→キャプチャ＋ASRを起動、一時停止中→ミュート解除
//! - Pause: ミュートする（無音を送り続けるのでWSセッションは維持される）
//! - Stop: キャプチャ終了→音声チャネル切断→ASRが `endStream` して正常終了

use screencapturekit::prelude::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::AppEvent;
use crate::asr::{AsrConfig, AsrEvent, supervise};
use crate::audio::{AudioFrame, extract_audio_frame, format_audio_status};
use crate::i18n::UiLocale;
use crate::language::SharedLanguageConfig;
use crate::store::SharedStore;

/// キャプチャの状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureState {
    Stopped,
    Running,
    Paused,
}

/// 操作ボタンの種別
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    Start,
    Pause,
    Stop,
}

/// 状態遷移表。無効な操作は現在状態を維持する
pub fn next_state(current: CaptureState, action: ControlAction) -> CaptureState {
    use CaptureState::*;
    use ControlAction::*;
    match (current, action) {
        (Stopped, Start) => Running,
        (Paused, Start) => Running,
        (Running, Pause) => Paused,
        (Running, Stop) => Stopped,
        (Paused, Stop) => Stopped,
        (s, _) => s,
    }
}

/// ボタンの有効・無効（Start, Pause/Resume, Stop の順）
pub fn buttons_enabled(status: CaptureState) -> (bool, bool, bool) {
    use CaptureState::*;
    match status {
        Stopped => (true, false, false),
        Running => (false, true, true),
        Paused => (false, true, true),
    }
}

/// キャプチャタスクへの指示
enum CaptureCmd {
    Stop,
}

/// 稼働中セッションの操作口
struct CaptureHandle {
    cmd_tx: mpsc::Sender<CaptureCmd>,
    muted: Arc<AtomicBool>,
}

// ScreenCaptureKit から音声データが届いたときに呼ばれる構造体
struct AudioOutputHandler {
    gui_tx: mpsc::Sender<AppEvent>,
    audio_tx: mpsc::Sender<AudioFrame>,
    muted: Arc<AtomicBool>,
    counter: AtomicUsize,
    asr_enabled: bool,
    store: SharedStore,
}

impl SCStreamOutputTrait for AudioOutputHandler {
    fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio || self.muted.load(Ordering::Relaxed) {
            return;
        }
        let frame = match extract_audio_frame(&sample_buffer) {
            Ok(frame) => frame,
            Err(reason) => {
                let text = self
                    .store
                    .lock()
                    .map(|s| s.ui_locale().strings().unsupported_audio_text(&reason))
                    .unwrap_or_else(|_| {
                        UiLocale::default().strings().unsupported_audio_text(&reason)
                    });
                let _ = self.gui_tx.try_send(AppEvent::UpdateSource(text));
                return;
            }
        };

        let count = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        // ASR無効時のみレベルメーターを表示（有効時は文字起こし・翻訳が字幕を取る）
        if !self.asr_enabled && count % 20 == 1 {
            let status = format_audio_status(
                frame.sample_rate,
                frame.channels,
                frame.samples.len(),
                frame.rms,
                count,
            );
            let _ = self.gui_tx.try_send(AppEvent::UpdateSource(status));
        }
        // 満杯なら破棄（リアルタイム優先）
        let _ = self.audio_tx.try_send(frame);
    }
}

/// セッション起動に必要な共有物
#[derive(Clone)]
pub struct SpawnContext {
    pub tokio: Handle,
    pub gui_tx: mpsc::Sender<AppEvent>,
    /// セッション毎に clone して渡す。原文はアプリ終了まで保持し、
    /// 翻訳タスクの受信ループを維持する
    pub asr_tx: mpsc::Sender<AsrEvent>,
    /// None のとき ASR は起動せず音声レベルのみ
    pub asr_config: Option<AsrConfig>,
    pub langs: SharedLanguageConfig,
    pub store: SharedStore,
}

struct PipelineInner {
    status: CaptureState,
    capture: Option<CaptureHandle>,
}

/// 操作ウィンドウが叩く制御口。Clone してUIと共有する
#[derive(Clone)]
pub struct PipelineController {
    ctx: SpawnContext,
    inner: Arc<Mutex<PipelineInner>>,
}

impl PipelineController {
    pub fn new(ctx: SpawnContext) -> Self {
        Self {
            ctx,
            inner: Arc::new(Mutex::new(PipelineInner {
                status: CaptureState::Stopped,
                capture: None,
            })),
        }
    }

    pub fn status(&self) -> CaptureState {
        self.inner.lock().map(|i| i.status).unwrap_or(CaptureState::Stopped)
    }

    pub fn start(&self) {
        let mut inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return,
        };
        let next = next_state(inner.status, ControlAction::Start);
        if next == inner.status {
            return;
        }
        match inner.status {
            CaptureState::Stopped => {
                inner.capture = Some(spawn_session(&self.ctx));
            }
            CaptureState::Paused => {
                if let Some(c) = &inner.capture {
                    c.muted.store(false, Ordering::Relaxed);
                }
                println!("▶️ 再開しました");
            }
            CaptureState::Running => {}
        }
        inner.status = next;
    }

    pub fn pause(&self) {
        let mut inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return,
        };
        if next_state(inner.status, ControlAction::Pause) == inner.status {
            return;
        }
        if let Some(c) = &inner.capture {
            c.muted.store(true, Ordering::Relaxed);
        }
        inner.status = CaptureState::Paused;
        println!("⏸️ 一時停止しました（セッションは維持）");
    }

    pub fn stop(&self) {
        let mut inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return,
        };
        if next_state(inner.status, ControlAction::Stop) == inner.status {
            return;
        }
        if let Some(c) = inner.capture.take() {
            let _ = c.cmd_tx.try_send(CaptureCmd::Stop);
        }
        inner.status = CaptureState::Stopped;
    }
}

/// キャプチャ＋ASRの1セッションを起動する
fn spawn_session(ctx: &SpawnContext) -> CaptureHandle {
    let (audio_tx, audio_rx) = mpsc::channel::<AudioFrame>(200);
    let asr_enabled = ctx.asr_config.is_some();
    let handle = spawn_capture(
        &ctx.tokio,
        ctx.gui_tx.clone(),
        audio_tx,
        asr_enabled,
        ctx.store.clone(),
    );
    if let Some(cfg) = &ctx.asr_config {
        ctx.tokio.spawn(supervise(
            audio_rx,
            ctx.asr_tx.clone(),
            cfg.clone(),
            ctx.langs.clone(),
        ));
    } else {
        // ASRなし：音声チャネルを空読みして詰まらせない
        ctx.tokio.spawn(async move {
            let mut rx = audio_rx;
            while rx.recv().await.is_some() {}
        });
    }
    println!("▶️ キャプチャを開始しました");
    handle
}

/// ScreenCaptureKit の初期化と開始。Stop 指示で終了する
fn spawn_capture(
    tokio: &Handle,
    gui_tx: mpsc::Sender<AppEvent>,
    audio_tx: mpsc::Sender<AudioFrame>,
    asr_enabled: bool,
    store: SharedStore,
) -> CaptureHandle {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<CaptureCmd>(4);
    let muted = Arc::new(AtomicBool::new(false));
    let handle = CaptureHandle {
        cmd_tx,
        muted: muted.clone(),
    };

    tokio.spawn(async move {
        println!("ScreenCaptureKit を初期化中...");

        // 1. キャプチャ可能な対象のリストを取得
        let content = SCShareableContent::get().expect("コンテンツの一覧取得に失敗");

        // 2. 音声設定を確実に構成する
        let mut config = SCStreamConfiguration::default();
        config.set_captures_audio(true);
        config.set_sample_rate(48_000);
        config.set_channel_count(2);
        config.set_excludes_current_process_audio(false);
        config.set_width(100);
        config.set_height(100);

        // 3. 画面全体のフィルターを作成
        let displays = content.displays();
        let display = displays.first().expect("ディスプレイが見つかりません");
        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build()
            .expect("cannot create filter");

        // 4. ストリームの作成
        let mut stream = SCStream::new(&filter, &config).expect("cannot create stream");

        // 5. コールバック（音声用）を登録
        let handler = AudioOutputHandler {
            gui_tx,
            audio_tx,
            muted,
            counter: AtomicUsize::new(0),
            asr_enabled,
            store,
        };
        stream
            .add_output_handler(handler, SCStreamOutputType::Audio)
            .expect("cannot add output handler");

        // 6. キャプチャ開始
        match stream.start_capture() {
            Ok(_) => println!("🚀 ScreenCaptureKit: ストリーム正常に開始！"),
            Err(e) => println!("❌ ScreenCaptureKit: 起動失敗エラー詳細: {:?}", e),
        }

        // Stop 指示まで維持
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    if matches!(cmd, Some(CaptureCmd::Stop) | None) {
                        let _ = stream.stop_capture();
                        println!("⏹️ キャプチャを終了しました");
                        break;
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {}
            }
        }
    });

    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions() {
        use CaptureState::*;
        use ControlAction::*;
        assert_eq!(next_state(Stopped, Start), Running);
        assert_eq!(next_state(Paused, Start), Running);
        assert_eq!(next_state(Running, Start), Running);
        assert_eq!(next_state(Running, Pause), Paused);
        assert_eq!(next_state(Stopped, Pause), Stopped);
        assert_eq!(next_state(Paused, Pause), Paused);
        assert_eq!(next_state(Running, Stop), Stopped);
        assert_eq!(next_state(Paused, Stop), Stopped);
        assert_eq!(next_state(Stopped, Stop), Stopped);
    }

    #[test]
    fn button_availability() {
        use CaptureState::*;
        assert_eq!(buttons_enabled(Stopped), (true, false, false));
        assert_eq!(buttons_enabled(Running), (false, true, true));
        assert_eq!(buttons_enabled(Paused), (false, true, true));
    }
}

mod asr;
mod audio;
mod fonts;
mod translate;

use eframe::egui;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

// ScreenCaptureKit 関連のインポート
use screencapturekit::prelude::*;

use asr::{ASR_MODEL_DEFAULT, AsrConfig, AsrEvent, supervise};
use audio::{AudioFrame, extract_audio_frame, format_audio_status};
use translate::{TRANSLATE_MODEL_DEFAULT, translate_to_japanese};

enum AppEvent {
    UpdateSubtitle(String),
}

// ScreenCaptureKit から音声データが届いたときに呼ばれる構造体
struct AudioOutputHandler {
    gui_tx: mpsc::Sender<AppEvent>,
    audio_tx: mpsc::Sender<AudioFrame>,
    counter: AtomicUsize,
    /// ASRが有効ならレベル表示を間引く（字幕は文字起こし優先）
    asr_enabled: bool,
}

impl SCStreamOutputTrait for AudioOutputHandler {
    fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }
        let frame = match extract_audio_frame(&sample_buffer) {
            Ok(frame) => frame,
            Err(reason) => {
                let _ = self.gui_tx.try_send(AppEvent::UpdateSubtitle(format!(
                    "⚠️ 音声フォーマット未対応: {reason}"
                )));
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
            let _ = self.gui_tx.try_send(AppEvent::UpdateSubtitle(status));
        }
        // 満杯なら破棄（リアルタイム優先）
        let _ = self.audio_tx.try_send(frame);
    }
}

fn resolve_api_key() -> Option<String> {
    std::env::var("META_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var("MODEL_API_KEY")
                .ok()
                .filter(|v| !v.is_empty())
        })
}

fn resolve_base_url() -> String {
    std::env::var("META_API_BASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.meta.ai/v1".to_string())
}

fn resolve_model(env_key: &str, default: &str) -> String {
    std::env::var(env_key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// RESTベースURLから realtime WS の URL を組み立てる
fn derive_ws_url(base: &str) -> Result<String, String> {
    let b = base.trim_end_matches('/');
    if let Some(rest) = b.strip_prefix("https://") {
        Ok(format!("wss://{rest}/asr/realtime"))
    } else if let Some(rest) = b.strip_prefix("http://") {
        Ok(format!("ws://{rest}/asr/realtime"))
    } else {
        Err(format!("unsupported META_API_BASE_URL scheme: {base}"))
    }
}

fn main() -> Result<(), eframe::Error> {
    // 💡 アプリ起動時に .env ファイルから環境変数を読み込む
    if let Err(e) = dotenvy::dotenv() {
        println!("⚠️ .env ファイルの読み込みに失敗しました（環境変数から直接読み込みます）: {}", e);
    }

    let api_key = resolve_api_key();
    let base_url = resolve_base_url();
    let asr_model = resolve_model("ASR_MODEL", ASR_MODEL_DEFAULT);
    let translate_model = resolve_model("TRANSLATE_MODEL", TRANSLATE_MODEL_DEFAULT);
    println!(
        "🗝️ API Key: {}",
        if api_key.is_some() { "OK" } else { "Missing（ASR・翻訳なし）" }
    );
    println!("🌐 API Base URL: {base_url}");

    // 1. 非同期処理（tokio）をマルチスレッドモードで起動
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokioの起動に失敗");

    let tokio_handle = rt.handle().clone();
    let (gui_tx, gui_rx) = mpsc::channel::<AppEvent>(100);
    let (audio_tx, audio_rx) = mpsc::channel::<AudioFrame>(200);
    let (asr_tx, mut asr_rx) = mpsc::channel::<AsrEvent>(100);

    // ASR・翻訳パイプライン（APIキーがある場合のみ）
    let asr_enabled = match (api_key.clone(), derive_ws_url(&base_url)) {
        (Some(key), Ok(ws_url)) => {
            println!("🔌 ASR接続先: {ws_url}（model={asr_model}）");
            let cfg = AsrConfig {
                ws_url,
                api_key: key.clone(),
                model: asr_model,
            };
            rt.spawn(supervise(audio_rx, asr_tx, cfg));

            // ASRイベント → 翻訳 → 字幕（順序保持のため逐次処理）
            let gui_tx2 = gui_tx.clone();
            let client = reqwest::Client::new();
            rt.spawn(async move {
                while let Some(event) = asr_rx.recv().await {
                    match event {
                        AsrEvent::Session(id) => {
                            println!("✅ ASRセッション開始: {id}");
                            let _ = gui_tx2
                                .try_send(AppEvent::UpdateSubtitle(
                                    "🎧 音声を聞き取り中...".to_string(),
                                ));
                        }
                        AsrEvent::Partial(text) => {
                            let _ = gui_tx2.try_send(AppEvent::UpdateSubtitle(format!("{text} …")));
                        }
                        AsrEvent::Complete { turn_id, transcript } => {
                            println!("📝 確定#{turn_id}: {transcript}");
                            let _ = gui_tx2
                                .try_send(AppEvent::UpdateSubtitle(transcript.clone()));
                            match translate_to_japanese(
                                &client,
                                &base_url,
                                &key,
                                &translate_model,
                                &transcript,
                            )
                            .await
                            {
                                Ok(ja) => {
                                    println!("🇯🇵 翻訳#{turn_id}: {ja}");
                                    let _ = gui_tx2.try_send(AppEvent::UpdateSubtitle(ja));
                                }
                                Err(e) => println!("⚠️ 翻訳失敗#{turn_id}: {e}"),
                            }
                        }
                        AsrEvent::Error(e) => println!("⚠️ ASRエラー: {e}"),
                    }
                }
            });
            true
        }
        (Some(_), Err(e)) => {
            println!("⚠️ {e}（ASR・翻訳なし）");
            false
        }
        (None, _) => {
            println!("⚠️ APIキー未設定のためASR・翻訳は無効です（レベル表示のみ）");
            false
        }
    };

    // 【裏方スレッド：ScreenCaptureKitの初期化と開始】
    let tx_clone = gui_tx.clone();
    rt.spawn(async move {
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
            gui_tx: tx_clone,
            audio_tx,
            counter: AtomicUsize::new(0),
            asr_enabled,
        };
        stream.add_output_handler(handler, SCStreamOutputType::Audio).expect("cannot add output handler");

        // 6. キャプチャ開始のログを詳細にする
        match stream.start_capture() {
            Ok(_) => println!("🚀 ScreenCaptureKit: ストリーム正常に開始！"),
            Err(e) => println!("❌ ScreenCaptureKit: 起動失敗エラー詳細: {:?}", e),
        }

        // スレッドが終了しないように維持
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    });

    // GUI（egui）のウィンドウ設定
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_transparent(true)
            .with_decorations(false)
            .with_always_on_top()
            .with_mouse_passthrough(true)
            .with_inner_size([800.0, 150.0]),
        ..Default::default()
    };

    let initial_text = if asr_enabled {
        "ASRに接続中...".to_string()
    } else {
        "YouTubeなどを再生して音を鳴らしてください...".to_string()
    };
    eframe::run_native(
        "LumeSub",
        options,
        Box::new(move |cc| {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 150);
            cc.egui_ctx.set_visuals(visuals);
            let font_manager = fonts::FontManager::load();
            println!("{}", font_manager.report());
            font_manager.install(&cc.egui_ctx);
            Box::new(LumeSubApp::new(
                cc.egui_ctx.clone(),
                gui_rx,
                tokio_handle,
                initial_text,
            ))
        }),
    )
}

struct LumeSubApp {
    subtitle_text: String,
    receiver: Arc<Mutex<mpsc::Receiver<AppEvent>>>,
}

impl LumeSubApp {
    fn new(
        ctx: egui::Context,
        mut rx: mpsc::Receiver<AppEvent>,
        handle: Handle,
        initial_text: String,
    ) -> Self {
        let (tx_internal, rx_internal) = mpsc::channel(100);
        let app = Self {
            subtitle_text: initial_text,
            receiver: Arc::new(Mutex::new(rx_internal)),
        };

        let receiver_ctx = ctx.clone();
        handle.spawn(async move {
            while let Some(event) = rx.recv().await {
                let _ = tx_internal.send(event).await;
                receiver_ctx.request_repaint();
            }
        });

        app
    }
}

impl eframe::App for LumeSubApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Ok(mut rx) = self.receiver.try_lock() {
            while let Ok(event) = rx.try_recv() {
                match event {
                    AppEvent::UpdateSubtitle(text) => {
                        self.subtitle_text = text;
                    }
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&self.subtitle_text)
                            .font(egui::FontId::proportional(28.0))
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .wrap(true),
                );
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_derivation() {
        assert_eq!(
            derive_ws_url("https://api.meta.ai/v1").unwrap(),
            "wss://api.meta.ai/v1/asr/realtime"
        );
        assert_eq!(
            derive_ws_url("https://api.meta.ai/v1/").unwrap(),
            "wss://api.meta.ai/v1/asr/realtime"
        );
        assert_eq!(
            derive_ws_url("http://localhost:8080").unwrap(),
            "ws://localhost:8080/asr/realtime"
        );
        assert!(derive_ws_url("ftp://x").is_err());
        assert!(derive_ws_url("api.meta.ai/v1").is_err());
    }
}

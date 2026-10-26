use eframe::egui;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::runtime::Handle;

// ScreenCaptureKit 関連のインポート
use screencapturekit::prelude::*;

enum AppEvent {
    UpdateSubtitle(String),
}

// ScreenCaptureKit から音声データが届いたときに呼ばれる構造体
struct AudioOutputHandler {
    tx: mpsc::Sender<AppEvent>,
}

impl SCStreamOutputTrait for AudioOutputHandler {
    fn did_output_sample_buffer(&self, _sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
        // 音声データが届いた場合のみ処理
        if of_type == SCStreamOutputType::Audio {
            // 💡 画面上の字幕をリアルタイムに更新して、音が拾えていることを視覚的に確認
            // let _ = self.tx.try_send(AppEvent::UpdateSubtitle("🎤 音声データ受信中...".to_string()));
            let _ = self.tx.try_send(AppEvent::UpdateSubtitle("🎤 Receiving audio data...".to_string()));
            
            // 本番ではここで sample_buffer から PCM（f32配列）を取り出して Meta AI に送ります
        }
    }
}

fn main() -> Result<(), eframe::Error> {
    // 💡 アプリ起動時に .env ファイルから環境変数を読み込む
    if let Err(e) = dotenvy::dotenv() {
        println!("⚠️ .env ファイルの読み込みに失敗しました（環境変数から直接読み込みます）: {}", e);
    }

    // 環境変数からAPIキーとURLを取得して確認（開発中のデバッグ用）
    let api_key = std::env::var("META_API_KEY").unwrap_or_else(|_| "NOT_FOUND".to_string());
    let base_url = std::env::var("META_API_BASE_URL").unwrap_or_else(|_| "NOT_FOUND".to_string());
    println!("🗝️ Meta API Key Loaded: {}", if api_key != "NOT_FOUND" { "OK" } else { "Missing" });
    println!("🌐 Meta API Base URL: {}", base_url);

    // 1. 非同期処理（tokio）をマルチスレッドモードで起動
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokioの起動に失敗");

    let tokio_handle = rt.handle().clone();
    let (tx, rx) = mpsc::channel::<AppEvent>(100);

    // 【裏方スレッド：ScreenCaptureKitの初期化と開始】
    let tx_clone = tx.clone();
    rt.spawn(async move {
        // スレッド内で環境変数を取得
        let api_key = std::env::var("META_API_KEY").expect("META_API_KEY must be set");
        let base_url = std::env::var("META_API_BASE_URL").expect("META_API_BASE_URL must be set");

        // HTTPクライアントの作成
        let client = reqwest::Client::new();

        println!("ScreenCaptureKit を初期化中...");
        
        // 1. キャプチャ可能な対象のリストを取得
        let content = SCShareableContent::get().expect("コンテンツの一覧取得に失敗");
        
        // // 2. 音声のみをキャプチャする設定を構築
        // let mut config = SCStreamConfiguration::default();
        // config.set_captures_audio(true);
        // config.set_width(100);
        // config.set_height(100);
        // 2. 音声設定を確実に構成する
        let mut config = SCStreamConfiguration::default();
        config.set_captures_audio(true);
        // config.set_excludes_wave_audio(false); // システム全体の音を拾う設定
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
        let handler = AudioOutputHandler { tx: tx_clone };
        
        // 💡 修正点：Screen ではなく Audio を指定する
        stream.add_output_handler(handler, SCStreamOutputType::Audio).expect("cannot add output handler");

        // 6. キャプチャ開始
        // match stream.start_capture() {
        //     Ok(_) => println!("ScreenCaptureKit によるキャプチャを開始しました！"),
        //     Err(e) => println!("キャプチャ開始に失敗: {:?}", e),
        // }
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

    eframe::run_native(
        "LumeSub",
        options,
        Box::new(move |cc| {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 150);
            cc.egui_ctx.set_visuals(visuals);
            Box::new(LumeSubApp::new(cc.egui_ctx.clone(), rx, tokio_handle))
        }),
    )
}

struct LumeSubApp {
    subtitle_text: String,
    receiver: Arc<Mutex<mpsc::Receiver<AppEvent>>>,
}

impl LumeSubApp {
    fn new(ctx: egui::Context, mut rx: mpsc::Receiver<AppEvent>, handle: Handle) -> Self {
        let (tx_internal, rx_internal) = mpsc::channel(100);
        let app = Self {
            subtitle_text: "YouTubeなどを再生して音を鳴らしてください...".to_string(),
            // subtitle_text: "Play YouTube or some audio...".to_string(),
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

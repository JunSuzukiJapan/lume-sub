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
            let _ = self.tx.try_send(AppEvent::UpdateSubtitle("🎤 音声データ受信中...".to_string()));
            
            // 本番ではここで sample_buffer から PCM（f32配列）を取り出して Meta AI に送ります
        }
    }
}

fn main() -> Result<(), eframe::Error> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokioの起動に失敗");

    let tokio_handle = rt.handle().clone();
    let (tx, rx) = mpsc::channel::<AppEvent>(100);

    // 【裏方スレッド：ScreenCaptureKitの初期化と開始】
    let tx_clone = tx.clone();
    rt.spawn(async move {
        println!("ScreenCaptureKit を初期化中...");
        
        // 1. キャプチャ可能な対象（画面、ウィンドウ、音声）のリストを取得
        // 💡 実行時に Mac から「画面収録の許可」を求められるポップアップが出ることがあります
        let content = SCShareableContent::get().expect("コンテンツの一覧取得に失敗");
        
        // 2. 音声のみをキャプチャする設定を構築
        let mut config = SCStreamConfiguration::default();
        config.set_captures_audio(true);
        config.set_width(100);
        config.set_height(100);
        
        // 3. 画面全体（または特定のアプリ）のフィルターを作成
        // 今回はシンプルに、メインディスプレイ全体の音声を対象にします
        let displays = content.displays();
        let display = displays.first().expect("ディスプレイが見つかりません");
        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build().expect("cannot create filter");

        // 4. ストリームの作成
        // let mut stream = SCStream::new(filter, config);
        let mut stream = SCStream::new(&filter, &config).expect("cannot create stream");

        // 5. コールバック（音声が届いた時の処理）を登録
        let handler = AudioOutputHandler { tx: tx_clone };
        // stream.add_output(handler, SCStreamOutputType::Audio);
        stream.add_output_handler(handler, SCStreamOutputType::Screen);

        // 6. キャプチャ開始
        match stream.start_capture() {
            Ok(_) => println!("ScreenCaptureKit によるキャプチャを開始しました！"),
            Err(e) => println!("キャプチャ開始に失敗: {:?}", e),
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

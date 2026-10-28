mod asr;
mod audio;
mod control;
mod fonts;
mod geometry;
mod hover;
mod language;
mod text;
mod translate;

use eframe::egui;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use asr::{ASR_MODEL_DEFAULT, AsrConfig, AsrEvent};
use control::{CaptureState, PipelineController, SpawnContext, buttons_enabled};
use language::{Language, LanguageConfig, SharedLanguageConfig, SourceLanguage, shared};
use text::shape_for_display;
use translate::{TRANSLATE_MODEL_DEFAULT, TranslationSession};

pub(crate) enum AppEvent {
    UpdateSubtitle(String),
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
    // rustls の暗号プロバイダを明示的に選択する。
    // 依存の features だけでは自動判定できず TLS 初回使用時に panic するため
    let _ = rustls::crypto::ring::default_provider().install_default();

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
    let lang_config = LanguageConfig::from_env();
    println!("🈯 言語: {}", lang_config.describe());
    let languages = shared(lang_config);

    // 非同期処理（tokio）をマルチスレッドモードで起動
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokioの起動に失敗");

    let tokio_handle = rt.handle().clone();
    let (gui_tx, gui_rx) = mpsc::channel::<AppEvent>(100);
    let (asr_tx, mut asr_rx) = mpsc::channel::<AsrEvent>(100);

    let asr_config = match (api_key, derive_ws_url(&base_url)) {
        (Some(key), Ok(ws_url)) => {
            println!("🔌 ASR接続先: {ws_url}（model={asr_model}）");
            Some(AsrConfig {
                ws_url,
                api_key: key,
                model: asr_model,
            })
        }
        (Some(_), Err(e)) => {
            println!("⚠️ {e}（ASR・翻訳なし）");
            None
        }
        (None, _) => {
            println!("⚠️ APIキー未設定のためASR・翻訳は無効です（レベル表示のみ）");
            None
        }
    };
    let asr_enabled = asr_config.is_some();

    // ASRイベント → 翻訳 → 字幕（順序保持のため逐次処理）
    if let Some(cfg) = asr_config.clone() {
        let gui_tx2 = gui_tx.clone();
        let client = reqwest::Client::new();
        let langs = languages.clone();
        let base = base_url.clone();
        let model = translate_model.clone();
        rt.spawn(async move {
            let mut session = TranslationSession::default();
            while let Some(event) = asr_rx.recv().await {
                match event {
                    AsrEvent::Session(id) => {
                        println!("✅ ASRセッション開始: {id}");
                        let pair = langs
                            .lock()
                            .map(|c| c.describe())
                            .unwrap_or_else(|_| "?".to_string());
                        let _ = gui_tx2
                            .try_send(AppEvent::UpdateSubtitle(format!("🎧 {pair} 聞き取り中...")));
                    }
                    AsrEvent::Partial(text) => {
                        let _ = gui_tx2.try_send(AppEvent::UpdateSubtitle(format!("{text} …")));
                    }
                    AsrEvent::Complete { turn_id, transcript } => {
                        println!("📝 確定#{turn_id}: {transcript}");
                        let _ =
                            gui_tx2.try_send(AppEvent::UpdateSubtitle(transcript.clone()));
                        let pair =
                            langs.lock().map(|c| c.clone()).unwrap_or_default();
                        if !pair.needs_translation() {
                            println!("⏭️ 同一言語のため翻訳スキップ#{turn_id}");
                            continue;
                        }
                        match session
                            .translate_next(
                                &client,
                                &base,
                                &cfg.api_key,
                                &model,
                                &pair.source,
                                &pair.target,
                                &transcript,
                            )
                            .await
                        {
                            Ok(ja) => {
                                println!("🌐 翻訳#{turn_id}: {ja}");
                                let _ = gui_tx2.try_send(AppEvent::UpdateSubtitle(ja));
                            }
                            Err(e) => println!("⚠️ 翻訳失敗#{turn_id}: {e}"),
                        }
                    }
                    AsrEvent::Error(e) => println!("⚠️ ASRエラー: {e}"),
                }
            }
        });
    }

    // パイプライン制御（起動時は自動開始）
    let controller = PipelineController::new(SpawnContext {
        tokio: tokio_handle.clone(),
        gui_tx,
        asr_tx,
        asr_config,
        langs: languages.clone(),
    });
    controller.start();

    // GUI（egui）のウィンドウ設定：字幕オーバーレイ
    let geom_path = geometry::default_path();
    let saved = geom_path.as_deref().and_then(geometry::load);
    if saved.is_some() {
        println!("🪟 字幕位置を復元します");
    }
    let mut viewport = egui::ViewportBuilder::default()
        .with_transparent(true)
        .with_decorations(false)
        .with_always_on_top()
        .with_mouse_passthrough(true)
        .with_inner_size(
            saved.map(|g| [g.w, g.h])
                .unwrap_or([geometry::DEFAULT_W, geometry::DEFAULT_H]),
        )
        .with_min_inner_size([geometry::MIN_W, geometry::MIN_H]);
    if let Some(g) = saved {
        viewport = viewport.with_position([g.x, g.y]);
    }
    let restored = saved.is_some();
    let options = eframe::NativeOptions {
        viewport,
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
                controller,
                languages,
                geom_path.clone(),
                restored,
            ))
        }),
    )
}

fn status_label(status: CaptureState) -> &'static str {
    match status {
        CaptureState::Stopped => "⏹ 停止中",
        CaptureState::Running => "▶ 動作中",
        CaptureState::Paused => "⏸ 一時停止中",
    }
}

fn source_label(source: SourceLanguage) -> String {
    match source {
        SourceLanguage::Auto => "auto（自動検出）".to_string(),
        SourceLanguage::Specific(l) => format!("{} ({})", l.display_name(), l.code()),
    }
}

struct LumeSubApp {
    subtitle_text: String,
    receiver: Arc<Mutex<mpsc::Receiver<AppEvent>>>,
    controller: PipelineController,
    langs: SharedLanguageConfig,
    hover: hover::HoverState,
    geom: geometry::GeometrySaver,
    placed_initial: bool,
    /// 整形済み字幕の覚え書き（論理文・幅・視覚文）。変化時のみ再計算する
    shaped_cache: (String, u32, String),
}

impl LumeSubApp {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: egui::Context,
        mut rx: mpsc::Receiver<AppEvent>,
        handle: Handle,
        initial_text: String,
        controller: PipelineController,
        langs: SharedLanguageConfig,
        geom_path: Option<std::path::PathBuf>,
        restored: bool,
    ) -> Self {
        let (tx_internal, rx_internal) = mpsc::channel(100);
        let app = Self {
            subtitle_text: initial_text,
            receiver: Arc::new(Mutex::new(rx_internal)),
            controller,
            langs,
            hover: hover::HoverState::new(),
            geom: geometry::GeometrySaver::new(geom_path),
            placed_initial: restored,
            shaped_cache: (String::new(), u32::MAX, String::new()),
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

    /// 操作ウィンドウ（字幕オーバーレイはマウス透過のため入力はここで受ける）
    fn control_ui(&self, ui: &mut egui::Ui) {
        ui.heading("LumeSub");
        ui.label(format!("状態: {}", status_label(self.controller.status())));
        ui.separator();

        if let Ok(mut langs) = self.langs.lock() {
            egui::ComboBox::from_label("入力言語")
                .selected_text(shape_for_display(&source_label(langs.source)).into_owned())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut langs.source, SourceLanguage::Auto, "auto（自動検出）");
                    for l in Language::all() {
                        ui.selectable_value(
                            &mut langs.source,
                            SourceLanguage::Specific(*l),
                            shape_for_display(&format!("{} ({})", l.display_name(), l.code()))
                                .into_owned(),
                        );
                    }
                });
            egui::ComboBox::from_label("出力言語")
                .selected_text(shape_for_display(langs.target.display_name()).into_owned())
                .show_ui(ui, |ui| {
                    for l in Language::all() {
                        ui.selectable_value(
                            &mut langs.target,
                            *l,
                            shape_for_display(&format!("{} ({})", l.display_name(), l.code()))
                                .into_owned(),
                        );
                    }
                });
            ui.label(shape_for_display(&format!("設定: {}", langs.describe())).into_owned());
        } else {
            ui.colored_label(egui::Color32::RED, "言語設定の読み取りに失敗");
        }
        ui.small("※ 入力言語は次回接続から反映（Stop→Start）");
        ui.separator();

        let status = self.controller.status();
        let (can_start, can_pause, can_stop) = buttons_enabled(status);
        ui.horizontal(|ui| {
            if ui.add_enabled(can_start, egui::Button::new("▶ Start")).clicked() {
                self.controller.start();
            }
            let pause_label = if status == CaptureState::Paused {
                "▶ Resume"
            } else {
                "⏸ Pause"
            };
            if ui.add_enabled(can_pause, egui::Button::new(pause_label)).clicked() {
                if status == CaptureState::Paused {
                    self.controller.start();
                } else {
                    self.controller.pause();
                }
            }
            if ui.add_enabled(can_stop, egui::Button::new("⏹ Stop")).clicked() {
                self.controller.stop();
            }
            if ui.button("⏻ Quit").clicked() {
                self.controller.stop();
                ui.ctx().send_viewport_cmd_to(
                    egui::ViewportId::ROOT,
                    egui::ViewportCommand::Close,
                );
            }
        });
        ui.separator();
        // 保存した画面外位置などに迷子になった場合の救済
        if ui.button("字幕位置をリセット").clicked()
            && let Some(monitor) = ui.ctx().input(|i| i.viewport().monitor_size)
        {
            let size = egui::vec2(geometry::DEFAULT_W, geometry::DEFAULT_H);
            let pos = geometry::bottom_center(monitor, size, geometry::MARGIN_BOTTOM);
            ui.ctx().send_viewport_cmd_to(
                egui::ViewportId::ROOT,
                egui::ViewportCommand::OuterPosition(pos),
            );
            ui.ctx().send_viewport_cmd_to(
                egui::ViewportId::ROOT,
                egui::ViewportCommand::InnerSize(size),
            );
        }
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

        let interactive = self.hover.poll(ctx);
        // 初回（保存なし）はモニター情報が掴めた初フレームで中央下部へ
        if !self.placed_initial {
            let info = ctx.input(|i| (i.viewport().monitor_size, i.viewport().inner_rect));
            if let (Some(monitor), Some(inner)) = info {
                let pos = geometry::bottom_center(
                    monitor,
                    inner.size(),
                    geometry::MARGIN_BOTTOM,
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                self.placed_initial = true;
            }
        }
        self.geom.tick(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                // RTL は論理順のまま実フォント幅で割り、行ごとに整形する（行順保持）
                let max_w = ui.available_width() - 4.0;
                let width_key = (max_w * 2.0).round().max(0.0) as u32;
                let visual = if self.shaped_cache.0 == self.subtitle_text
                    && self.shaped_cache.1 == width_key
                {
                    self.shaped_cache.2.clone()
                } else {
                    let shaped = ui.fonts(|fonts| {
                        let measure = |s: &str| {
                            fonts
                                .layout_no_wrap(
                                    s.to_owned(),
                                    egui::FontId::proportional(28.0),
                                    egui::Color32::WHITE,
                                )
                                .size()
                                .x
                        };
                        text::shape_paragraph_for_display(&self.subtitle_text, max_w, &measure)
                            .into_owned()
                    });
                    self.shaped_cache =
                        (self.subtitle_text.clone(), width_key, shaped.clone());
                    shaped
                };
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(visual)
                            .font(egui::FontId::proportional(28.0))
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .wrap(true),
                );
            });
            if interactive {
                hover::paint_frame(ui);
            }
        });

        // ホバー検出のため待機中も再描画する（操作中は毎フレーム）
        if interactive {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // 操作ウィンドウは常時表示（閉じても次フレームで復活する）
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("lume-sub-control"),
            egui::ViewportBuilder::default()
                .with_title("LumeSub Control")
                .with_inner_size([320.0, 400.0]),
            |ctx, _class| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.control_ui(ui);
                });
            },
        );
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

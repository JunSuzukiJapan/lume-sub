mod asr;
mod audio;
mod control;
mod fonts;
mod geometry;
mod hover;
mod i18n;
mod language;
mod text;
mod translate;

use eframe::egui;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use asr::{ASR_MODEL_DEFAULT, AsrConfig, AsrEvent};
use control::{CaptureState, PipelineController, SpawnContext, buttons_enabled};
use i18n::{SharedUiLocale, UiLocale, UiStrings};
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
    let locale_path = i18n::default_path();
    let ui_locale = locale_path
        .as_deref()
        .and_then(i18n::load)
        .or_else(UiLocale::from_env)
        .unwrap_or_default();
    println!(
        "🗣️ 表示言語: {} ({})",
        ui_locale.display_name(),
        ui_locale.code()
    );
    let ui_locales = i18n::shared(ui_locale);

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
        let ui_langs = ui_locales.clone();
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
                        let text = ui_langs
                            .lock()
                            .map(|l| l.strings().listening_text(&pair))
                            .unwrap_or_else(|_| {
                                UiLocale::default().strings().listening_text(&pair)
                            });
                        let _ = gui_tx2.try_send(AppEvent::UpdateSubtitle(text));
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
        ui_locale: ui_locales.clone(),
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

    let initial_text = {
        let t = ui_locale.strings();
        if asr_enabled {
            t.connecting.to_string()
        } else {
            t.play_prompt.to_string()
        }
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
                ui_locales.clone(),
                locale_path.clone(),
                geom_path.clone(),
                restored,
            ))
        }),
    )
}

fn status_label(status: CaptureState, t: &UiStrings) -> &'static str {
    match status {
        CaptureState::Stopped => t.status_stopped,
        CaptureState::Running => t.status_running,
        CaptureState::Paused => t.status_paused,
    }
}

fn source_label(source: SourceLanguage, t: &UiStrings) -> String {
    match source {
        SourceLanguage::Auto => t.auto_detect.to_string(),
        SourceLanguage::Specific(l) => format!("{} ({})", l.display_name(), l.code()),
    }
}

struct LumeSubApp {
    subtitle_text: String,
    receiver: Arc<Mutex<mpsc::Receiver<AppEvent>>>,
    controller: PipelineController,
    langs: SharedLanguageConfig,
    ui_locale: SharedUiLocale,
    locale_path: Option<std::path::PathBuf>,
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
        ui_locale: SharedUiLocale,
        locale_path: Option<std::path::PathBuf>,
        geom_path: Option<std::path::PathBuf>,
        restored: bool,
    ) -> Self {
        let (tx_internal, rx_internal) = mpsc::channel(100);
        let app = Self {
            subtitle_text: initial_text,
            receiver: Arc::new(Mutex::new(rx_internal)),
            controller,
            langs,
            ui_locale,
            locale_path,
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
        let locale = self.ui_locale.lock().map(|l| *l).unwrap_or_default();
        let t = locale.strings();
        ui.heading("LumeSub");
        ui.label(t.status_line(status_label(self.controller.status(), t)));
        ui.separator();

        if let Ok(mut langs) = self.langs.lock() {
            egui::ComboBox::from_label(t.input_language)
                .selected_text(shape_for_display(&source_label(langs.source, t)).into_owned())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut langs.source, SourceLanguage::Auto, t.auto_detect);
                    for l in Language::all() {
                        ui.selectable_value(
                            &mut langs.source,
                            SourceLanguage::Specific(*l),
                            shape_for_display(&format!("{} ({})", l.display_name(), l.code()))
                                .into_owned(),
                        );
                    }
                });
            egui::ComboBox::from_label(t.output_language)
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
            ui.label(shape_for_display(&t.settings_line(&langs.describe())).into_owned());
        } else {
            ui.colored_label(egui::Color32::RED, t.lang_read_failed);
        }
        ui.small(t.input_note);
        ui.separator();

        // 表示言語の切替。変えたら即保存する
        if let Ok(mut current) = self.ui_locale.lock() {
            let before = *current;
            egui::ComboBox::from_label(current.strings().display_language)
                .selected_text(current.display_name())
                .show_ui(ui, |ui| {
                    for l in UiLocale::all() {
                        ui.selectable_value(&mut *current, *l, l.display_name());
                    }
                });
            if *current != before {
                println!(
                    "🗣️ 表示言語: {} ({})",
                    current.display_name(),
                    current.code()
                );
                if let Some(path) = &self.locale_path
                    && let Err(e) = i18n::save(path, *current)
                {
                    println!("⚠️ 表示言語の保存に失敗: {e}");
                }
            }
        }
        ui.separator();

        let status = self.controller.status();
        let (can_start, can_pause, can_stop) = buttons_enabled(status);
        ui.horizontal(|ui| {
            if ui.add_enabled(can_start, egui::Button::new(t.start)).clicked() {
                self.controller.start();
            }
            let pause_label = if status == CaptureState::Paused {
                t.resume
            } else {
                t.pause
            };
            if ui.add_enabled(can_pause, egui::Button::new(pause_label)).clicked() {
                if status == CaptureState::Paused {
                    self.controller.start();
                } else {
                    self.controller.pause();
                }
            }
            if ui.add_enabled(can_stop, egui::Button::new(t.stop)).clicked() {
                self.controller.stop();
            }
            if ui.button(t.quit).clicked() {
                self.controller.stop();
                ui.ctx().send_viewport_cmd_to(
                    egui::ViewportId::ROOT,
                    egui::ViewportCommand::Close,
                );
            }
        });
        ui.separator();
        // 保存した画面外位置などに迷子になった場合の救済
        if ui.button(t.reset_position).clicked()
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
                let drag = self
                    .ui_locale
                    .lock()
                    .map(|l| l.strings().drag_to_move)
                    .unwrap_or_else(|_| UiLocale::default().strings().drag_to_move);
                hover::paint_frame(ui, drag);
            }
        });

        // ホバー検出のため待機中も再描画する（操作中は毎フレーム）
        if interactive {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // 操作ウィンドウは常時表示（閉じても次フレームで復活する）
        let control_title = self
            .ui_locale
            .lock()
            .map(|l| l.strings().control_title)
            .unwrap_or_else(|_| UiLocale::default().strings().control_title);
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("lume-sub-control"),
            egui::ViewportBuilder::default()
                .with_title(control_title)
                .with_inner_size([320.0, 440.0]),
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

mod asr;
mod audio;
mod control;
mod fonts;
mod geometry;
mod hover;
mod i18n;
mod language;
mod screen;
mod store;
mod text;
mod translate;

use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use asr::{ASR_MODEL_DEFAULT, AsrConfig, AsrEvent};
use control::{CaptureState, PipelineController, SpawnContext, buttons_enabled};
use i18n::{UiLocale, UiStrings};
use language::{Language, SharedLanguageConfig, SourceLanguage, shared};
use screen::DiagLog;
use store::{ApiConfig, SharedStore, Store};
use text::shape_for_display;
use translate::{TRANSLATE_MODEL_DEFAULT, TranslationSession};

pub(crate) enum AppEvent {
    UpdateSource(String),
    UpdateTranslation(String),
}

/// 高速モードの途中翻訳の間引き間隔。呼び出し回数を抑える
const PARTIAL_TRANSLATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 高速モードの途中訳の追跡。間引き判定と確定時の使い回しを持つ。
/// 時刻は引数で受けてテスト可能にする
struct PartialTracker {
    last_at: Option<Instant>,
    last_text: String,
    last_translation: String,
}

impl PartialTracker {
    fn new() -> Self {
        Self {
            last_at: None,
            last_text: String::new(),
            last_translation: String::new(),
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    /// 文言が変わり、前回から間引き間隔が過ぎたら翻訳する
    fn should_translate(&self, text: &str, now: Instant) -> bool {
        if text.is_empty() || text == self.last_text {
            return false;
        }
        match self.last_at {
            None => true,
            Some(at) => now >= at + PARTIAL_TRANSLATE_INTERVAL,
        }
    }

    fn note_translated(&mut self, text: String, translation: String, now: Instant) {
        self.last_at = Some(now);
        self.last_text = text;
        self.last_translation = translation;
    }

    /// 確定文が直前の途中訳と一致すれば使い回す。なければ None
    fn reuse(&self, transcript: &str) -> Option<String> {
        if !transcript.is_empty()
            && !self.last_translation.is_empty()
            && transcript == self.last_text
        {
            Some(self.last_translation.clone())
        } else {
            None
        }
    }
}

/// 原文窓（ROOT）の窓タイトル。AppKit 照会と共有する
const SOURCE_VIEWPORT_TITLE: &str = "LumeSub";
/// 翻訳窓の窓タイトル。AppKit 照会と共有する
const TRANSLATION_VIEWPORT_TITLE: &str = "LumeSub Translation";

/// 字幕窓の非表示要求。両方隠しは拒否してログを出す
fn hide_subtitle(store: &SharedStore, hide_source: bool) {
    let mut s = store.lock().expect("store lock poisoned");
    let (source, translation) = if hide_source {
        (false, s.show_translation())
    } else {
        (s.show_source(), false)
    };
    if !s.set_shows(source, translation) {
        println!("⚠️ 両方の字幕を非表示にはできません");
    }
}

/// 保存幾何→字幕枠
fn rect_from_geometry(g: geometry::WindowGeometry) -> egui::Rect {
    egui::Rect::from_min_size(egui::Pos2::new(g.x, g.y), egui::vec2(g.w, g.h))
}

/// 字幕窓がメイン画面の外にあれば中央下部へ移す
fn follow_window(
    ctx: &egui::Context,
    target: egui::ViewportId,
    outer: Option<egui::Rect>,
    diag: &mut DiagLog,
) {
    match (outer, screen::active_screen_snapshot()) {
        (Some(outer), Ok(main)) => {
            if let Some(pos) = screen::reconcile_target(main, outer) {
                println!("🖥️ 追従: 移動します {outer:?} → {pos:?}（メイン画面 {main:?}）");
                ctx.send_viewport_cmd_to(target, egui::ViewportCommand::OuterPosition(pos));
            } else {
                diag.log_if_changed(
                    &format!("stay|{main:?}"),
                    &format!("🖥️ 追従: 待機中（字幕 {outer:?} はメイン画面 {main:?} の内側）"),
                );
            }
        }
        (outer, snap) => {
            diag.log_if_changed(
                &format!("blocked|outer={}|snap={snap:?}", outer.is_some()),
                &format!("🖥️ 追従: 判定できません（字幕位置 {outer:?} / 画面 {snap:?}）"),
            );
        }
    }
}

/// 字幕1窓分を描く。バツが押されたら true を返す。
/// もう片方の窓が非表示中のときは `close_enabled` を false にしてバツを無効化する
fn paint_subtitle_window(
    ui: &mut egui::Ui,
    text: &str,
    shaped_cache: &mut (String, u32, String),
    interactive: bool,
    drag_hint: &str,
    tag: &str,
    close_enabled: bool,
) -> bool {
    ui.vertical_centered(|ui| {
        ui.add_space(20.0);
        // RTL は論理順のまま実フォント幅で割り、行ごとに整形する（行順保持）
        let max_w = ui.available_width() - 4.0;
        let width_key = (max_w * 2.0).round().max(0.0) as u32;
        let visual = if shaped_cache.0 == text && shaped_cache.1 == width_key {
            shaped_cache.2.clone()
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
                text::shape_paragraph_for_display(text, max_w, &measure).into_owned()
            });
            *shaped_cache = (text.to_string(), width_key, shaped.clone());
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
        hover::paint_frame(ui, drag_hint, tag, close_enabled)
    } else {
        false
    }
}

/// 保存値→環境変数の順でAPIキーを決める
fn resolve_api_key(stored: &str) -> Option<String> {
    if !stored.is_empty() {
        return Some(stored.to_string());
    }
    std::env::var("META_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var("MODEL_API_KEY")
                .ok()
                .filter(|v| !v.is_empty())
        })
}

/// 保存値→環境変数→既定値の順でベースURLを決める
fn resolve_base_url(stored: &str) -> String {
    if !stored.is_empty() {
        return stored.to_string();
    }
    std::env::var("META_API_BASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.meta.ai/v1".to_string())
}

/// 保存値→環境変数→既定値の順でモデル名を決める
fn resolve_model(stored: &str, env_key: &str, default: &str) -> String {
    if !stored.is_empty() {
        return stored.to_string();
    }
    std::env::var(env_key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// 現在有効なAPI設定（保存値→環境変数→既定値の順）。設定画面の初期値にも使う
fn effective_api(store: &SharedStore) -> ApiConfig {
    let stored = store.lock().expect("store lock poisoned").api();
    ApiConfig {
        api_key: resolve_api_key(&stored.api_key).unwrap_or_default(),
        base_url: resolve_base_url(&stored.base_url),
        asr_model: resolve_model(&stored.asr_model, "ASR_MODEL", ASR_MODEL_DEFAULT),
        translate_model: resolve_model(
            &stored.translate_model,
            "TRANSLATE_MODEL",
            TRANSLATE_MODEL_DEFAULT,
        ),
    }
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

    let store = store::shared(Store::load());
    // UI_LANG があれば初回の表示言語に使う（保存後は保存値が優先）
    {
        let mut s = store.lock().expect("store lock poisoned");
        if s.is_fresh()
            && let Some(env_locale) = UiLocale::from_env()
        {
            s.set_ui_locale(env_locale);
        }
    }
    let effective = effective_api(&store);
    let api_key = if effective.api_key.is_empty() {
        None
    } else {
        Some(effective.api_key.clone())
    };
    let base_url = effective.base_url.clone();
    let asr_model = effective.asr_model.clone();
    let translate_model = effective.translate_model.clone();
    let (saved, saved_control, follow_main, ui_locale, key_saved) = {
        let s = store.lock().expect("store lock poisoned");
        (
            s.source(),
            s.control(),
            s.follow_main_screen(),
            s.ui_locale(),
            !s.api().api_key.is_empty(),
        )
    };
    println!(
        "🗝️ API Key: {}",
        if key_saved {
            "保存済み設定を使用"
        } else if api_key.is_some() {
            "環境変数を使用"
        } else {
            "未設定（ASR・翻訳なし）"
        }
    );
    println!("🌐 API Base URL: {base_url}");
    println!(
        "🗣️ 表示言語: {} ({})",
        ui_locale.display_name(),
        ui_locale.code()
    );
    println!(
        "🖥️ メイン画面追従: {}",
        if follow_main { "ON" } else { "OFF" }
    );
    if saved.is_some() {
        println!("🪟 字幕位置を復元します");
    }
    if saved_control.is_some() {
        println!("🪟 コントロール位置を復元します");
    }
    // 保存済みキーがなければ初回ダイアログを出す。入力欄には有効値を入れる
    let settings_open = !key_saved;
    let settings_draft = effective;
    let lang_config = store.lock().expect("store lock poisoned").language_config();
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
        let task_store = store.clone();
        let base = base_url.clone();
        let model = translate_model.clone();
        rt.spawn(async move {
            let mut session = TranslationSession::default();
            let mut partials = PartialTracker::new();
            while let Some(event) = asr_rx.recv().await {
                match event {
                    AsrEvent::Session(id) => {
                        println!("✅ ASRセッション開始: {id}");
                        let pair = langs
                            .lock()
                            .map(|c| c.describe())
                            .unwrap_or_else(|_| "?".to_string());
                        let text = task_store
                            .lock()
                            .map(|s| s.ui_locale().strings().listening_text(&pair))
                            .unwrap_or_else(|_| {
                                UiLocale::default().strings().listening_text(&pair)
                            });
                        let _ = gui_tx2.try_send(AppEvent::UpdateSource(text));
                        // 新セッションでは古い翻訳を消す
                        let _ = gui_tx2.try_send(AppEvent::UpdateTranslation(String::new()));
                        partials.reset();
                    }
                    AsrEvent::Partial(text) => {
                        let _ = gui_tx2.try_send(AppEvent::UpdateSource(format!("{text} …")));
                        // 高速モード：途中文を間引いて翻訳する
                        let fast = task_store
                            .lock()
                            .map(|s| s.fast_translation())
                            .unwrap_or(false);
                        if fast && partials.should_translate(&text, Instant::now()) {
                            let pair =
                                langs.lock().map(|c| c.clone()).unwrap_or_default();
                            if pair.needs_translation() {
                                match session
                                    .translate_preview(
                                        &client,
                                        &base,
                                        &cfg.api_key,
                                        &model,
                                        &pair.source,
                                        &pair.target,
                                        &text,
                                    )
                                    .await
                                {
                                    Ok(tr) => {
                                        println!("🌐 途中訳: {tr}");
                                        partials.note_translated(
                                            text,
                                            tr.clone(),
                                            Instant::now(),
                                        );
                                        let _ = gui_tx2.try_send(AppEvent::UpdateTranslation(
                                            format!("{tr} …"),
                                        ));
                                    }
                                    Err(e) => println!("⚠️ 途中翻訳失敗: {e}"),
                                }
                            }
                        }
                    }
                    AsrEvent::Complete { turn_id, transcript } => {
                        println!("📝 確定#{turn_id}: {transcript}");
                        let _ =
                            gui_tx2.try_send(AppEvent::UpdateSource(transcript.clone()));
                        let pair =
                            langs.lock().map(|c| c.clone()).unwrap_or_default();
                        if !pair.needs_translation() {
                            println!("⏭️ 同一言語のため翻訳スキップ#{turn_id}");
                            let _ = gui_tx2
                                .try_send(AppEvent::UpdateTranslation(transcript.clone()));
                            continue;
                        }
                        // 高速モードの途中訳と一致すれば使い回す
                        if let Some(reuse) = partials.reuse(&transcript) {
                            println!("⚡ 途中訳を使い回し#{turn_id}");
                            session.push_history(transcript.clone(), reuse.clone());
                            let _ = gui_tx2.try_send(AppEvent::UpdateTranslation(reuse));
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
                                let _ = gui_tx2.try_send(AppEvent::UpdateTranslation(ja));
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
        store: store.clone(),
    });
    controller.start();

    // GUI（egui）のウィンドウ設定：字幕オーバーレイ
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
    let control_restored = saved_control.is_some();

    let initial_text = {
        let t = ui_locale.strings();
        if asr_enabled {
            t.connecting.to_string()
        } else {
            t.play_prompt.to_string()
        }
    };
    eframe::run_native(
        SOURCE_VIEWPORT_TITLE,
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
                store.clone(),
                restored,
                saved_control,
                control_restored,
                settings_open,
                settings_draft,
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
    source_text: String,
    translation_text: String,
    receiver: Arc<Mutex<mpsc::Receiver<AppEvent>>>,
    controller: PipelineController,
    langs: SharedLanguageConfig,
    store: SharedStore,
    follow_diag: DiagLog,
    follow_diag_translation: DiagLog,
    overlay_fullscreen_fixed: bool,
    translation_fullscreen_fixed: bool,
    source_visible: bool,
    fs_panel: Option<screen::FullscreenPanel>,
    panel_diag: DiagLog,
    fs_drag: screen::PanelDrag,
    fs_panel_translation: Option<screen::FullscreenPanel>,
    panel_diag_translation: DiagLog,
    fs_drag_translation: screen::PanelDrag,
    hover: hover::HoverState,
    hover_translation: hover::HoverState,
    placed_initial: bool,
    placed_translation_initial: bool,
    control_saved: Option<geometry::WindowGeometry>,
    placed_control_initial: bool,
    settings_open: bool,
    settings_draft: ApiConfig,
    show_source_draft: bool,
    show_translation_draft: bool,
    /// 整形済み字幕の覚え書き（論理文・幅・視覚文）。変化時のみ再計算する
    source_cache: (String, u32, String),
    translation_cache: (String, u32, String),
}

/// 設定ダイアログでの操作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsAction {
    Save,
    Close,
    None,
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
        store: SharedStore,
        restored: bool,
        control_saved: Option<geometry::WindowGeometry>,
        control_restored: bool,
        settings_open: bool,
        settings_draft: ApiConfig,
    ) -> Self {
        let (tx_internal, rx_internal) = mpsc::channel(100);
        let (show_source_draft, show_translation_draft) = {
            let s = store.lock().expect("store lock poisoned");
            (s.show_source(), s.show_translation())
        };
        let app = Self {
            source_text: initial_text,
            translation_text: String::new(),
            receiver: Arc::new(Mutex::new(rx_internal)),
            controller,
            langs,
            store,
            follow_diag: DiagLog::new(),
            follow_diag_translation: DiagLog::new(),
            overlay_fullscreen_fixed: false,
            translation_fullscreen_fixed: false,
            source_visible: true,
            fs_panel: None,
            panel_diag: DiagLog::new(),
            fs_drag: screen::PanelDrag::new(),
            fs_panel_translation: None,
            panel_diag_translation: DiagLog::new(),
            fs_drag_translation: screen::PanelDrag::new(),
            hover: hover::HoverState::new(),
            hover_translation: hover::HoverState::new(),
            placed_initial: restored,
            placed_translation_initial: false,
            control_saved,
            placed_control_initial: control_restored,
            settings_open,
            settings_draft,
            show_source_draft,
            show_translation_draft,
            source_cache: (String::new(), u32::MAX, String::new()),
            translation_cache: (String::new(), u32::MAX, String::new()),
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

    /// アプリ終了（Quitボタン・コントロール窓のバツ共通）
    fn quit(&self, ctx: &egui::Context) {
        self.controller.stop();
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
    }

    /// コントロール窓1フレーム分を描き、現在幾何を返す。
    /// バツボタンが押されていたらQuitと同じくアプリを終了する
    fn control_frame(&mut self, ctx: &egui::Context) -> Option<geometry::WindowGeometry> {
        if ctx.input(|i| i.viewport().close_requested()) {
            self.quit(ctx);
        }
        let current = ctx.input(|i| {
            match (
                i.viewport().outer_rect.map(|r| r.min),
                i.viewport().inner_rect.map(|r| r.size()),
            ) {
                (Some(pos), Some(size)) => Some(geometry::WindowGeometry {
                    x: pos.x,
                    y: pos.y,
                    w: size.x,
                    h: size.y,
                }),
                _ => None,
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            self.control_ui(ui);
        });
        current
    }

    /// 翻訳窓1フレーム分を描き、現在幾何を返す
    fn translation_frame(&mut self, ctx: &egui::Context) -> Option<geometry::WindowGeometry> {
        let interactive = self.hover_translation.poll(ctx);
        let current = ctx.input(|i| {
            match (
                i.viewport().outer_rect.map(|r| r.min),
                i.viewport().inner_rect.map(|r| r.size()),
            ) {
                (Some(pos), Some(size)) => Some(geometry::WindowGeometry {
                    x: pos.x,
                    y: pos.y,
                    w: size.x,
                    h: size.y,
                }),
                _ => None,
            }
        });
        let (drag_hint, close_enabled) = {
            let s = self.store.lock().expect("store lock poisoned");
            let hint = shape_for_display(s.ui_locale().strings().drag_to_move).into_owned();
            (hint, s.show_source())
        };
        let text = &self.translation_text;
        let cache = &mut self.translation_cache;
        egui::CentralPanel::default().show(ctx, |ui| {
            if paint_subtitle_window(
                ui,
                text,
                cache,
                interactive,
                &drag_hint,
                "translation",
                close_enabled,
            ) {
                hide_subtitle(&self.store, false);
            }
        });
        // ホバー検出のため待機中も再描画する（操作中は毎フレーム）
        if interactive {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        current
    }

    /// 設定ダイアログを開く。入力欄には現在有効な値を入れる
    fn open_settings(&mut self) {
        self.settings_draft = effective_api(&self.store);
        let s = self.store.lock().expect("store lock poisoned");
        self.show_source_draft = s.show_source();
        self.show_translation_draft = s.show_translation();
        self.settings_open = true;
    }

    /// 設定ダイアログの入力を保存して閉じる。API設定値は次回起動から有効
    fn apply_settings(&mut self) {
        let mut s = self.store.lock().expect("store lock poisoned");
        s.set_api(self.settings_draft.clone());
        if !s.set_shows(self.show_source_draft, self.show_translation_draft) {
            println!("⚠️ 両方の字幕を非表示にはできません（表示設定は変更しません）");
        }
        self.settings_open = false;
        println!("⚙️ 設定を保存しました（API設定は次回起動から有効）");
    }

    /// 設定ダイアログ1フレーム分を描き、操作を返す
    fn settings_frame(&mut self, ctx: &egui::Context) -> SettingsAction {
        if ctx.input(|i| i.viewport().close_requested()) {
            return SettingsAction::Close;
        }
        let locale = self.store.lock().expect("store lock poisoned").ui_locale();
        let t = locale.strings();
        let sh = |s: &str| shape_for_display(s).into_owned();
        let mut action = SettingsAction::None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(sh(t.api_key));
            ui.add(egui::TextEdit::singleline(&mut self.settings_draft.api_key).password(true));
            ui.label(sh(t.base_url));
            ui.add(egui::TextEdit::singleline(
                &mut self.settings_draft.base_url,
            ));
            ui.label(sh(t.asr_model));
            ui.add(egui::TextEdit::singleline(
                &mut self.settings_draft.asr_model,
            ));
            ui.label(sh(t.translate_model));
            ui.add(egui::TextEdit::singleline(
                &mut self.settings_draft.translate_model,
            ));
            ui.small(sh(t.settings_restart_note));
            ui.separator();
            let mut show_source = self.show_source_draft;
            let mut show_translation = self.show_translation_draft;
            ui.checkbox(&mut show_source, sh(t.show_source));
            ui.checkbox(&mut show_translation, sh(t.show_translation));
            // 両方非表示は不可：最後の1つのチェックは外せない
            if show_source || show_translation {
                self.show_source_draft = show_source;
                self.show_translation_draft = show_translation;
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(sh(t.save)).clicked() {
                    action = SettingsAction::Save;
                }
                if ui.button(sh(t.close)).clicked() {
                    action = SettingsAction::Close;
                }
            });
        });
        action
    }

    /// 操作ウィンドウ（字幕オーバーレイはマウス透過のため入力はここで受ける）
    fn control_ui(&mut self, ui: &mut egui::Ui) {
        let locale = self.store.lock().expect("store lock poisoned").ui_locale();
        let t = locale.strings();
        // RTLロケール（アラビア語・ヘブライ語）でも崩れないよう表示時に整形する。
        // RTLを含まない文言はそのまま返る
        let sh = |s: &str| shape_for_display(s).into_owned();
        ui.heading("LumeSub");
        ui.label(sh(&t.status_line(status_label(self.controller.status(), t))));
        ui.separator();

        // 入出力言語の切替。変えたら Store に即保存する
        let changed = if let Ok(mut langs) = self.langs.lock() {
            let before = langs.clone();
            egui::ComboBox::from_label(sh(t.input_language))
                .selected_text(shape_for_display(&source_label(langs.source, t)).into_owned())
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut langs.source,
                        SourceLanguage::Auto,
                        sh(t.auto_detect),
                    );
                    for l in Language::all() {
                        ui.selectable_value(
                            &mut langs.source,
                            SourceLanguage::Specific(*l),
                            shape_for_display(&format!("{} ({})", l.display_name(), l.code()))
                                .into_owned(),
                        );
                    }
                });
            egui::ComboBox::from_label(sh(t.output_language))
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
            if *langs != before {
                Some(langs.clone())
            } else {
                None
            }
        } else {
            ui.colored_label(egui::Color32::RED, sh(t.lang_read_failed));
            None
        };
        if let Some(cfg) = changed {
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_languages(cfg.source, cfg.target);
        }
        ui.small(sh(t.input_note));
        ui.separator();

        // 表示言語の切替。変えたら即保存する
        let before = self.store.lock().expect("store lock poisoned").ui_locale();
        let mut current = before;
        egui::ComboBox::from_label(sh(current.strings().display_language))
            .selected_text(sh(current.display_name()))
            .show_ui(ui, |ui| {
                for l in UiLocale::all() {
                    ui.selectable_value(&mut current, *l, sh(l.display_name()));
                }
            });
        if current != before {
            println!(
                "🗣️ 表示言語: {} ({})",
                current.display_name(),
                current.code()
            );
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_ui_locale(current);
        }
        ui.separator();

        let status = self.controller.status();
        let (can_start, can_pause, can_stop) = buttons_enabled(status);
        ui.horizontal(|ui| {
            if ui.add_enabled(can_start, egui::Button::new(sh(t.start))).clicked() {
                self.controller.start();
            }
            let pause_label = sh(if status == CaptureState::Paused {
                t.resume
            } else {
                t.pause
            });
            if ui.add_enabled(can_pause, egui::Button::new(pause_label)).clicked() {
                if status == CaptureState::Paused {
                    self.controller.start();
                } else {
                    self.controller.pause();
                }
            }
            if ui.add_enabled(can_stop, egui::Button::new(sh(t.stop))).clicked() {
                self.controller.stop();
            }
            if ui.button(sh(t.quit)).clicked() {
                self.quit(ui.ctx());
            }
        });
        ui.separator();
        // メイン画面追従のオン・オフ。変えたら即保存する
        let before = self
            .store
            .lock()
            .expect("store lock poisoned")
            .follow_main_screen();
        let mut follow = before;
        ui.checkbox(&mut follow, sh(t.follow_main_screen));
        if follow != before {
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_follow_main_screen(follow);
        }
        // 高速翻訳のオン・オフ。変えたら即保存する
        let before = self
            .store
            .lock()
            .expect("store lock poisoned")
            .fast_translation();
        let mut fast = before;
        ui.checkbox(&mut fast, sh(t.fast_translation));
        if fast != before {
            println!("🌐 高速翻訳: {}", if fast { "ON" } else { "OFF" });
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_fast_translation(fast);
        }
        // 原文・翻訳の表示文字数。変えたら即保存する（次の字幕から反映）
        let (before_source, before_translation) = {
            let s = self.store.lock().expect("store lock poisoned");
            (s.source_tail_chars(), s.translation_tail_chars())
        };
        let mut source_len = before_source;
        let mut translation_len = before_translation;
        ui.horizontal(|ui| {
            ui.label(sh(t.source_length));
            ui.add(egui::Slider::new(
                &mut source_len,
                text::TAIL_CHARS_MIN..=text::TAIL_CHARS_MAX,
            ));
        });
        ui.horizontal(|ui| {
            ui.label(sh(t.translation_length));
            ui.add(egui::Slider::new(
                &mut translation_len,
                text::TAIL_CHARS_MIN..=text::TAIL_CHARS_MAX,
            ));
        });
        if source_len != before_source || translation_len != before_translation {
            let mut s = self.store.lock().expect("store lock poisoned");
            s.set_source_tail_chars(source_len);
            s.set_translation_tail_chars(translation_len);
        }
        ui.horizontal(|ui| {
            if ui.button(sh(t.settings_button)).clicked() {
                self.open_settings();
            }
            // 保存した画面外位置などに迷子になった場合の救済
            if ui.button(sh(t.reset_position)).clicked()
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
        });
    }
}

impl eframe::App for LumeSubApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Ok(mut rx) = self.receiver.try_lock() {
            // 累積文は末尾に丸めて表示する（古い文が残り続けないよう）
            let (source_max, translation_max) = {
                let s = self.store.lock().expect("store lock poisoned");
                (s.source_tail_chars(), s.translation_tail_chars())
            };
            while let Ok(event) = rx.try_recv() {
                match event {
                    AppEvent::UpdateSource(text) => {
                        self.source_text =
                            text::tail_for_display(&text, source_max).to_string();
                    }
                    AppEvent::UpdateTranslation(text) => {
                        self.translation_text =
                            text::tail_for_display(&text, translation_max).to_string();
                    }
                }
            }
        }

        let (show_source, show_translation, follow) = {
            let s = self.store.lock().expect("store lock poisoned");
            (
                s.show_source(),
                s.show_translation(),
                s.follow_main_screen(),
            )
        };
        // 原文窓（ROOT）の表示切替は可視コマンドで行う
        if show_source != self.source_visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(show_source));
            self.source_visible = show_source;
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
        // 原文オーバーレイを常時最前面に。窓が出るまで毎フレーム試す
        if !self.overlay_fullscreen_fixed {
            if screen::ensure_window_over_fullscreen(SOURCE_VIEWPORT_TITLE) {
                self.overlay_fullscreen_fixed = true;
                println!("🖥️ 原文窓を常時最前面にしました（全Space＋レベル1000）");
            } else {
                self.follow_diag.log_if_changed(
                    "overlay|missing",
                    "🖥️ 最前面化: 原文ウィンドウが見つかりません",
                );
            }
        }
        // フルスクリーン動画中はNSPanel字幕を出す（NSWindowは他アプリの
        // フルスクリーンSpaceに入れない）。非表示中はパネルも隠す
        // パネル移動バーの文言（論理順のまま渡し、整形はAppKit任せ）
        let drag_hint_raw = self
            .store
            .lock()
            .expect("store lock poisoned")
            .ui_locale()
            .strings()
            .drag_to_move;
        let mut panel_drag_live = false;
        let root_outer = ctx.input(|i| i.viewport().outer_rect);
        if show_source {
            if let Some(outer) = root_outer {
                // ドラッグ後は実窓（隠れて古い）の代わりに保存値を鏡写しする
                let mirror = if self.fs_drag.is_dirty() {
                    self.store
                        .lock()
                        .expect("store lock poisoned")
                        .source()
                        .map(rect_from_geometry)
                        .unwrap_or(outer)
                } else {
                    outer
                };
                let flip_h = screen::flip_height();
                let shown = screen::sync_fullscreen_panel(
                    &mut self.fs_panel,
                    screen::window_occlusion_visible(SOURCE_VIEWPORT_TITLE),
                    mirror,
                    flip_h,
                    &self.source_text,
                    &mut self.panel_diag,
                );
                if shown {
                    let pointer = screen::poll_pointer();
                    if let Some(panel) = self.fs_panel.as_mut() {
                        let out = screen::interact_fullscreen_panel(
                            panel,
                            &mut self.fs_drag,
                            mirror,
                            flip_h,
                            pointer,
                            drag_hint_raw,
                            show_translation,
                        );
                        if let Some(rect) = out.desired {
                            let g = geometry::WindowGeometry {
                                x: rect.min.x,
                                y: rect.min.y,
                                w: rect.width(),
                                h: rect.height(),
                            };
                            {
                                let mut s =
                                    self.store.lock().expect("store lock poisoned");
                                s.set_source_geometry(g);
                                if out.finished {
                                    s.flush();
                                }
                            }
                            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                                rect.min,
                            ));
                            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(rect.size()));
                        } else if out.finished {
                            self.store.lock().expect("store lock poisoned").flush();
                        }
                        if out.close_requested {
                            hide_subtitle(&self.store, true);
                        }
                        panel_drag_live |= self.fs_drag.is_active();
                    }
                } else {
                    self.fs_drag.reset();
                }
            }
        } else {
            screen::sync_fullscreen_panel(
                &mut self.fs_panel,
                Some(true),
                egui::Rect::NOTHING,
                0.0,
                "",
                &mut self.panel_diag,
            );
            self.fs_drag.reset();
        }
        // メイン画面追従（字幕がメイン画面の外にあれば自動移動）。
        // ドラッグ中は古い実窓での判定を止める（保存値が正）
        if follow && show_source && !self.fs_drag.is_dirty() {
            follow_window(
                ctx,
                egui::ViewportId::ROOT,
                root_outer,
                &mut self.follow_diag,
            );
        }
        // 原文窓の移動・リサイズを間引き保存する。
        // ドラッグ中は古い実窓で保存値を上書きしない
        let source_geom = ctx.input(|i| {
            match (
                i.viewport().outer_rect.map(|r| r.min),
                i.viewport().inner_rect.map(|r| r.size()),
            ) {
                (Some(pos), Some(size)) => Some(geometry::WindowGeometry {
                    x: pos.x,
                    y: pos.y,
                    w: size.x,
                    h: size.y,
                }),
                _ => None,
            }
        });
        if !self.fs_drag.is_dirty()
            && let Some(current) = &source_geom
        {
            self.store
                .lock()
                .expect("store lock poisoned")
                .offer_source(current);
        }

        let drag_hint = {
            let drag = self
                .store
                .lock()
                .expect("store lock poisoned")
                .ui_locale()
                .strings()
                .drag_to_move;
            shape_for_display(drag).into_owned()
        };
        egui::CentralPanel::default().show(ctx, |ui| {
            if paint_subtitle_window(
                ui,
                &self.source_text,
                &mut self.source_cache,
                interactive,
                &drag_hint,
                "source",
                show_translation,
            ) {
                hide_subtitle(&self.store, true);
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
            .store
            .lock()
            .expect("store lock poisoned")
            .ui_locale()
            .strings()
            .control_title;
        let control_id = egui::ViewportId::from_hash_of("lume-sub-control");
        let mut builder = egui::ViewportBuilder::default()
            .with_title(control_title)
            .with_inner_size(
                self.control_saved
                    .map(|g| [g.w, g.h])
                    .unwrap_or([geometry::CONTROL_DEFAULT_W, geometry::CONTROL_DEFAULT_H]),
            );
        if let Some(g) = self.control_saved {
            builder = builder.with_position([g.x, g.y]);
        }
        let mut control_current: Option<geometry::WindowGeometry> = None;
        ctx.show_viewport_immediate(control_id, builder, |ctx, _class| {
            control_current = self.control_frame(ctx);
        });
        if let Some(current) = &control_current {
            self.store
                .lock()
                .expect("store lock poisoned")
                .offer_control(current);
        }
        // 初回（保存なし）はモニター情報が掴めた初フレームで右上へ
        if !self.placed_control_initial
            && let Some(monitor) = ctx.input(|i| i.viewport().monitor_size)
        {
            let size = control_current
                .map(|g| egui::vec2(g.w, g.h))
                .unwrap_or(egui::vec2(
                    geometry::CONTROL_DEFAULT_W,
                    geometry::CONTROL_DEFAULT_H,
                ));
            let pos =
                geometry::top_right(monitor, size, geometry::MARGIN_TOP, geometry::MARGIN_RIGHT);
            ctx.send_viewport_cmd_to(control_id, egui::ViewportCommand::OuterPosition(pos));
            self.placed_control_initial = true;
        }

        // 翻訳窓（非表示中は出さない）
        if show_translation {
            let translation_id = egui::ViewportId::from_hash_of("lume-sub-translation");
            let saved_translation = self
                .store
                .lock()
                .expect("store lock poisoned")
                .translation();
            let mut builder = egui::ViewportBuilder::default()
                .with_title(TRANSLATION_VIEWPORT_TITLE)
                .with_transparent(true)
                .with_decorations(false)
                .with_always_on_top()
                .with_mouse_passthrough(true)
                .with_inner_size(
                    saved_translation
                        .map(|g| [g.w, g.h])
                        .unwrap_or([geometry::DEFAULT_W, geometry::DEFAULT_H]),
                )
                .with_min_inner_size([geometry::MIN_W, geometry::MIN_H]);
            if let Some(g) = saved_translation {
                builder = builder.with_position([g.x, g.y]);
            }
            let mut translation_current: Option<geometry::WindowGeometry> = None;
            ctx.show_viewport_immediate(translation_id, builder, |ctx, _class| {
                translation_current = self.translation_frame(ctx);
            });
            // ドラッグ中は古い実窓で保存値を上書きしない
            if !self.fs_drag_translation.is_dirty()
                && let Some(current) = &translation_current
            {
                self.store
                    .lock()
                    .expect("store lock poisoned")
                    .offer_translation(current);
            }
            let translation_outer = translation_current.map(rect_from_geometry);
            // 翻訳窓も常時最前面に。窓が出るまで毎フレーム試す
            if !self.translation_fullscreen_fixed {
                if screen::ensure_window_over_fullscreen(TRANSLATION_VIEWPORT_TITLE) {
                    self.translation_fullscreen_fixed = true;
                    println!("🖥️ 翻訳窓を常時最前面にしました（全Space＋レベル1000）");
                } else {
                    self.follow_diag_translation.log_if_changed(
                        "overlay|missing",
                        "🖥️ 最前面化: 翻訳ウィンドウが見つかりません",
                    );
                }
            }
            // フルスクリーン動画中はNSPanel字幕を出す
            if let Some(outer) = translation_outer {
                // ドラッグ後は実窓（隠れて古い）の代わりに保存値を鏡写しする
                let mirror = if self.fs_drag_translation.is_dirty() {
                    self.store
                        .lock()
                        .expect("store lock poisoned")
                        .translation()
                        .map(rect_from_geometry)
                        .unwrap_or(outer)
                } else {
                    outer
                };
                let flip_h = screen::flip_height();
                let shown = screen::sync_fullscreen_panel(
                    &mut self.fs_panel_translation,
                    screen::window_occlusion_visible(TRANSLATION_VIEWPORT_TITLE),
                    mirror,
                    flip_h,
                    &self.translation_text,
                    &mut self.panel_diag_translation,
                );
                if shown {
                    let pointer = screen::poll_pointer();
                    // 両窓のハンドルが重なり得るため、先に掴んだ方を優先する
                    let pointer = (
                        pointer.0,
                        pointer.1
                            && !(self.fs_drag.is_active()
                                && !self.fs_drag_translation.is_active()),
                    );
                    if let Some(panel) = self.fs_panel_translation.as_mut() {
                        let out = screen::interact_fullscreen_panel(
                            panel,
                            &mut self.fs_drag_translation,
                            mirror,
                            flip_h,
                            pointer,
                            drag_hint_raw,
                            show_source,
                        );
                        if let Some(rect) = out.desired {
                            let g = geometry::WindowGeometry {
                                x: rect.min.x,
                                y: rect.min.y,
                                w: rect.width(),
                                h: rect.height(),
                            };
                            {
                                let mut s =
                                    self.store.lock().expect("store lock poisoned");
                                s.set_translation_geometry(g);
                                if out.finished {
                                    s.flush();
                                }
                            }
                            ctx.send_viewport_cmd_to(
                                translation_id,
                                egui::ViewportCommand::OuterPosition(rect.min),
                            );
                            ctx.send_viewport_cmd_to(
                                translation_id,
                                egui::ViewportCommand::InnerSize(rect.size()),
                            );
                        } else if out.finished {
                            self.store.lock().expect("store lock poisoned").flush();
                        }
                        if out.close_requested {
                            hide_subtitle(&self.store, false);
                        }
                        panel_drag_live |= self.fs_drag_translation.is_active();
                    }
                } else {
                    self.fs_drag_translation.reset();
                }
            }
            // ドラッグ中は古い実窓での判定を止める（保存値が正）
            if follow && !self.fs_drag_translation.is_dirty() {
                follow_window(
                    ctx,
                    translation_id,
                    translation_outer,
                    &mut self.follow_diag_translation,
                );
            }
            // 初回（保存なし）は原文窓の上に配置する
            if saved_translation.is_none() && !self.placed_translation_initial {
                let size = translation_current
                    .map(|g| egui::vec2(g.w, g.h))
                    .unwrap_or(egui::vec2(geometry::DEFAULT_W, geometry::DEFAULT_H));
                let (monitor, source_outer) =
                    ctx.input(|i| (i.viewport().monitor_size, i.viewport().outer_rect));
                let pos = source_outer
                    .map(|source| {
                        egui::Pos2::new(
                            source.min.x,
                            (source.min.y - geometry::TRANSLATION_GAP - size.y).max(0.0),
                        )
                    })
                    .or_else(|| {
                        monitor.map(|m| {
                            geometry::bottom_center(
                                m,
                                size,
                                geometry::MARGIN_BOTTOM
                                    + geometry::DEFAULT_H
                                    + geometry::TRANSLATION_GAP,
                            )
                        })
                    });
                if let Some(pos) = pos {
                    ctx.send_viewport_cmd_to(
                        translation_id,
                        egui::ViewportCommand::OuterPosition(pos),
                    );
                    self.placed_translation_initial = true;
                }
            }
        } else {
            // 非表示中は配置・ホバー状態を捨て、パネルも隠す
            self.placed_translation_initial = false;
            self.hover_translation = hover::HoverState::new();
            screen::sync_fullscreen_panel(
                &mut self.fs_panel_translation,
                Some(true),
                egui::Rect::NOTHING,
                0.0,
                "",
                &mut self.panel_diag_translation,
            );
            self.fs_drag_translation.reset();
        }

        // フルスクリーン中のドラッグは毎フレーム駆動する
        // （待機中は100ms間引きの再描画が効く）
        if panel_drag_live {
            ctx.request_repaint();
        }

        // 設定ダイアログ（初回または設定ボタンで開く）
        if self.settings_open {
            let settings_id = egui::ViewportId::from_hash_of("lume-sub-settings");
            let title = self
                .store
                .lock()
                .expect("store lock poisoned")
                .ui_locale()
                .strings()
                .settings_title;
            let action = ctx.show_viewport_immediate(
                settings_id,
                egui::ViewportBuilder::default()
                    .with_title(title)
                    .with_inner_size([400.0, 380.0]),
                |ctx, _class| self.settings_frame(ctx),
            );
            match action {
                SettingsAction::Save => self.apply_settings(),
                SettingsAction::Close => self.settings_open = false,
                SettingsAction::None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LanguageConfig;

    #[test]
    fn partial_tracker_throttles_and_reuses() {
        use std::time::Duration;
        let mut p = PartialTracker::new();
        let t0 = Instant::now();
        // 初回は空でなければ訳す
        assert!(p.should_translate("hello", t0));
        assert!(!p.should_translate("", t0));
        p.note_translated("hello".into(), "こんにちは".into(), t0);
        // 同文・間引き内は訳さない
        assert!(!p.should_translate("hello", t0));
        assert!(!p.should_translate("hello world", t0));
        // 変化＋1秒経過で訳す
        assert!(p.should_translate("hello world", t0 + Duration::from_secs(2)));
        // 確定一致で使い回す
        assert_eq!(p.reuse("hello"), Some("こんにちは".to_string()));
        assert_eq!(p.reuse("other"), None);
        // リセットで空に戻る
        p.reset();
        assert_eq!(p.reuse("hello"), None);
        assert!(p.should_translate("hello", t0));
    }

    fn test_app(handle: Handle) -> LumeSubApp {
        let (gui_tx, gui_rx) = mpsc::channel::<AppEvent>(100);
        let (asr_tx, _asr_rx) = mpsc::channel::<AsrEvent>(100);
        let langs = crate::language::shared(LanguageConfig::default());
        let store = crate::store::shared(Store::load_from(None));
        let controller = PipelineController::new(SpawnContext {
            tokio: handle.clone(),
            gui_tx,
            asr_tx,
            asr_config: None,
            langs: langs.clone(),
            store: store.clone(),
        });
        LumeSubApp::new(
            egui::Context::default(),
            gui_rx,
            handle,
            String::new(),
            controller,
            langs,
            store,
            false,
            None,
            false,
            false,
            ApiConfig::default(),
        )
    }

    fn root_commands(output: &egui::FullOutput) -> Vec<egui::ViewportCommand> {
        output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|o| o.commands.clone())
            .unwrap_or_default()
    }

    fn raw_input_with(events: Vec<egui::ViewportEvent>) -> egui::RawInput {
        let mut raw = egui::RawInput::default();
        raw.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                events,
                ..Default::default()
            },
        );
        raw
    }

    fn click_input(pos: egui::Pos2, pressed: bool) -> egui::RawInput {
        let mut raw = raw_input_with(vec![]);
        raw.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        raw
    }

    fn click_close(
        ctx: &egui::Context,
        cache: &mut (String, u32, String),
        pos: egui::Pos2,
        enabled: bool,
        pressed: bool,
    ) -> bool {
        let mut clicked = false;
        let _ = ctx.run(click_input(pos, pressed), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                clicked = paint_subtitle_window(ui, "hello", cache, true, "drag", "test", enabled);
            });
        });
        clicked
    }

    #[test]
    fn control_close_requests_app_quit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = test_app(rt.handle().clone());
        let ctx = egui::Context::default();

        // 通常フレームでは終了しない
        let plain = ctx.run(raw_input_with(vec![]), |ctx| {
            app.control_frame(ctx);
        });
        assert!(!root_commands(&plain).contains(&egui::ViewportCommand::Close));

        // バツ（Closeイベント）でQuitと同じくROOTへClose
        let closed = ctx.run(raw_input_with(vec![egui::ViewportEvent::Close]), |ctx| {
            app.control_frame(ctx);
        });
        assert!(root_commands(&closed).contains(&egui::ViewportCommand::Close));
    }

    #[test]
    fn apply_settings_saves_draft_to_store() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = test_app(rt.handle().clone());
        app.settings_draft = ApiConfig {
            api_key: "key".to_string(),
            base_url: "https://example.test/v1".to_string(),
            asr_model: "asr".to_string(),
            translate_model: "tr".to_string(),
        };
        app.settings_open = true;
        app.apply_settings();
        assert!(!app.settings_open);
        let saved = app.store.lock().unwrap().api();
        assert_eq!(saved, app.settings_draft);
    }

    #[test]
    fn hide_last_subtitle_is_refused() {
        let store = crate::store::shared(Store::load_from(None));
        hide_subtitle(&store, true);
        assert!(!store.lock().unwrap().show_source());
        assert!(store.lock().unwrap().show_translation());
        hide_subtitle(&store, false);
        assert!(!store.lock().unwrap().show_source());
        assert!(store.lock().unwrap().show_translation());
    }

    #[test]
    fn subtitle_paint_caches_shaped_text() {
        let ctx = egui::Context::default();
        let mut cache = (String::new(), u32::MAX, String::new());
        let mut clicked = true;
        let _ = ctx.run(raw_input_with(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                clicked =
                    paint_subtitle_window(ui, "hello", &mut cache, false, "drag", "test", true);
            });
        });
        assert!(!clicked);
        assert_eq!(cache.0, "hello");
        assert!(!cache.2.is_empty());
    }

    fn paint_frame_for_test(
        ctx: &egui::Context,
        cache: &mut (String, u32, String),
        enabled: bool,
        locate: Option<&mut egui::Pos2>,
    ) {
        let _ = ctx.run(raw_input_with(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                if let Some(out) = locate {
                    let rect = ui.max_rect();
                    *out = egui::Pos2::new(rect.max.x - 17.0, rect.min.y + 12.0);
                }
                paint_subtitle_window(ui, "hello", cache, true, "drag", "test", enabled);
            });
        });
    }

    #[test]
    fn disabled_close_ignores_clicks() {
        let ctx = egui::Context::default();
        let mut cache = (String::new(), u32::MAX, String::new());
        // バツの位置を特定する
        let mut x_center = egui::Pos2::ZERO;
        paint_frame_for_test(&ctx, &mut cache, true, Some(&mut x_center));
        // 無効なバツは押しても閉じない
        click_close(&ctx, &mut cache, x_center, false, true);
        assert!(!click_close(&ctx, &mut cache, x_center, false, false));
        // 有効に戻して配置を確定させてから押す（egui は前フレームの配置で判定する）
        paint_frame_for_test(&ctx, &mut cache, true, None);
        click_close(&ctx, &mut cache, x_center, true, true);
        assert!(click_close(&ctx, &mut cache, x_center, true, false));
    }

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

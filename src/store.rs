//! アプリ全体の永続化を一手に担う
//!
//! 窓位置・設定・表示言語・入出力言語・API設定を `~/.config/lume-sub/store.json`
//! 一つに保存する。読込・保存はすべて [`Store`] 経由で行い、他モジュールは
//! 個別の保存処理を持たない。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::geometry::WindowGeometry;
use crate::i18n::UiLocale;
use crate::dev_log;
use crate::fonts;
use crate::language::{Language, LanguageConfig, SourceLanguage};
use crate::text;

/// API設定。空文字は未設定（環境変数・既定値に譲る）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub asr_model: String,
    #[serde(default)]
    pub translate_model: String,
}

fn default_true() -> bool {
    true
}

fn default_tail_chars() -> usize {
    text::DEFAULT_TAIL_CHARS
}

fn default_font_size() -> f32 {
    fonts::DEFAULT_SUBTITLE_SIZE
}

/// 保存する全項目
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreData {
    #[serde(default)]
    pub source: Option<WindowGeometry>,
    #[serde(default)]
    pub translation: Option<WindowGeometry>,
    #[serde(default)]
    pub control: Option<WindowGeometry>,
    #[serde(default = "default_true")]
    pub follow_main_screen: bool,
    #[serde(default = "default_true")]
    pub show_source: bool,
    #[serde(default = "default_true")]
    pub show_translation: bool,
    #[serde(default)]
    pub fast_translation: bool,
    #[serde(default)]
    pub dev_logs: bool,
    #[serde(default = "default_tail_chars")]
    pub source_tail_chars: usize,
    #[serde(default = "default_tail_chars")]
    pub translation_tail_chars: usize,
    /// 原文字体（空文字＝既定チェーン、候補ID＝その字体優先）
    #[serde(default)]
    pub source_font: String,
    #[serde(default = "default_font_size")]
    pub source_font_size: f32,
    #[serde(default)]
    pub translation_font: String,
    #[serde(default = "default_font_size")]
    pub translation_font_size: f32,
    #[serde(default)]
    pub ui_locale: UiLocale,
    #[serde(default)]
    pub languages: LanguageConfig,
    #[serde(default)]
    pub api: ApiConfig,
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            source: None,
            translation: None,
            control: None,
            follow_main_screen: true,
            show_source: true,
            show_translation: true,
            fast_translation: false,
            dev_logs: false,
            source_tail_chars: text::DEFAULT_TAIL_CHARS,
            translation_tail_chars: text::DEFAULT_TAIL_CHARS,
            source_font: String::new(),
            source_font_size: fonts::DEFAULT_SUBTITLE_SIZE,
            translation_font: String::new(),
            translation_font_size: fonts::DEFAULT_SUBTITLE_SIZE,
            ui_locale: UiLocale::default(),
            languages: LanguageConfig::default(),
            api: ApiConfig::default(),
        }
    }
}

/// 全保存処理の窓口。UIスレッドとバックグラウンドタスクで共有する
///
/// 窓位置は実行中メモリにだけ追跡し、ファイルへの保存は終了時のみ行う
pub struct Store {
    path: Option<PathBuf>,
    data: StoreData,
    fresh: bool,
}

impl Store {
    /// 保存先。`~/.config/lume-sub/store.json`（`XDG_CONFIG_HOME` 尊重）
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("lume-sub").join("store.json"))
    }

    /// 既定保存先から読む。存在しない・壊れている場合は既定値
    pub fn load() -> Self {
        Self::load_from(Self::default_path())
    }

    /// 指定先から読む。存在しない・壊れている場合は既定値
    pub fn load_from(path: Option<PathBuf>) -> Self {
        let loaded = path.as_deref().and_then(read_file);
        let fresh = loaded.is_none();
        if path.is_none() {
            dev_log!("⚠️ 保存先が決まらないため設定は保存されません");
        }
        let mut data = loaded.unwrap_or_default();
        if !data.show_source && !data.show_translation {
            dev_log!("⚠️ 両方の字幕が非表示になっていたため表示に戻します");
            data.show_source = true;
            data.show_translation = true;
        }
        Self {
            path,
            data,
            fresh,
        }
    }

    /// 保存ファイルが存在しなかった（初回起動相当）
    pub fn is_fresh(&self) -> bool {
        self.fresh
    }

    pub fn source(&self) -> Option<WindowGeometry> {
        self.data.source
    }

    pub fn translation(&self) -> Option<WindowGeometry> {
        self.data.translation
    }

    pub fn control(&self) -> Option<WindowGeometry> {
        self.data.control
    }

    pub fn follow_main_screen(&self) -> bool {
        self.data.follow_main_screen
    }

    pub fn show_source(&self) -> bool {
        self.data.show_source
    }

    pub fn show_translation(&self) -> bool {
        self.data.show_translation
    }

    pub fn fast_translation(&self) -> bool {
        self.data.fast_translation
    }

    pub fn dev_logs(&self) -> bool {
        self.data.dev_logs
    }

    pub fn source_tail_chars(&self) -> usize {
        self.data.source_tail_chars
    }

    pub fn translation_tail_chars(&self) -> usize {
        self.data.translation_tail_chars
    }

    pub fn source_font(&self) -> &str {
        &self.data.source_font
    }

    pub fn source_font_size(&self) -> f32 {
        self.data.source_font_size
    }

    pub fn translation_font(&self) -> &str {
        &self.data.translation_font
    }

    pub fn translation_font_size(&self) -> f32 {
        self.data.translation_font_size
    }

    pub fn ui_locale(&self) -> UiLocale {
        self.data.ui_locale
    }

    pub fn language_config(&self) -> LanguageConfig {
        self.data.languages.clone()
    }

    pub fn api(&self) -> ApiConfig {
        self.data.api.clone()
    }

    /// 追従設定を変えて即保存する
    pub fn set_follow_main_screen(&mut self, follow: bool) {
        self.data.follow_main_screen = follow;
        self.save_now();
    }

    /// 高速翻訳モードを変えて即保存する
    pub fn set_fast_translation(&mut self, fast: bool) {
        self.data.fast_translation = fast;
        self.save_now();
    }

    /// 開発ログを変えて即保存する
    pub fn set_dev_logs(&mut self, on: bool) {
        self.data.dev_logs = on;
        self.save_now();
    }

    /// 原文の表示文字数を変えて即保存する
    pub fn set_source_tail_chars(&mut self, max: usize) {
        self.data.source_tail_chars = max;
        self.save_now();
    }

    /// 翻訳の表示文字数を変えて即保存する
    pub fn set_translation_tail_chars(&mut self, max: usize) {
        self.data.translation_tail_chars = max;
        self.save_now();
    }

    /// 原文字体を変えて即保存する
    pub fn set_source_font(&mut self, id: String) {
        self.data.source_font = id;
        self.save_now();
    }

    /// 原文サイズを変えて即保存する
    pub fn set_source_font_size(&mut self, size: f32) {
        self.data.source_font_size = size;
        self.save_now();
    }

    /// 翻訳字体を変えて即保存する
    pub fn set_translation_font(&mut self, id: String) {
        self.data.translation_font = id;
        self.save_now();
    }

    /// 翻訳サイズを変えて即保存する
    pub fn set_translation_font_size(&mut self, size: f32) {
        self.data.translation_font_size = size;
        self.save_now();
    }

    /// 表示言語を変えて即保存する
    pub fn set_ui_locale(&mut self, locale: UiLocale) {
        self.data.ui_locale = locale;
        self.save_now();
    }

    /// 入出力言語を変えて即保存する
    pub fn set_languages(&mut self, source: SourceLanguage, target: Language) {
        self.data.languages = LanguageConfig { source, target };
        self.save_now();
    }

    /// API設定を変えて即保存する
    pub fn set_api(&mut self, api: ApiConfig) {
        self.data.api = api;
        self.save_now();
    }

    /// 両字幕窓の表示切替。両方非表示は拒否して false を返す
    pub fn set_shows(&mut self, source: bool, translation: bool) -> bool {
        if !source && !translation {
            return false;
        }
        self.data.show_source = source;
        self.data.show_translation = translation;
        self.save_now();
        true
    }

    /// 原文窓の現在幾何をメモリに覚える。ファイルへの保存は終了時のみ
    pub fn track_source(&mut self, current: &WindowGeometry) {
        self.data.source = Some(*current);
    }

    /// 原文窓の幾何をメモリに覚える。フルスクリーン中のドラッグ用。
    /// ファイルへの保存は終了時のみ
    pub fn set_source_geometry(&mut self, current: WindowGeometry) {
        self.data.source = Some(current);
    }

    /// 翻訳窓の幾何をメモリに覚える。フルスクリーン中のドラッグ用。
    /// ファイルへの保存は終了時のみ
    pub fn set_translation_geometry(&mut self, current: WindowGeometry) {
        self.data.translation = Some(current);
    }

    /// 終了時点の窓位置を確定保存する。None の窓は据え置く
    pub fn persist_geometries(
        &mut self,
        source: Option<WindowGeometry>,
        translation: Option<WindowGeometry>,
        control: Option<WindowGeometry>,
    ) {
        if let Some(g) = source {
            self.data.source = Some(g);
        }
        if let Some(g) = translation {
            self.data.translation = Some(g);
        }
        if let Some(g) = control {
            self.data.control = Some(g);
        }
        self.save_now();
    }

    /// 翻訳窓の現在幾何をメモリに覚える。ファイルへの保存は終了時のみ
    pub fn track_translation(&mut self, current: &WindowGeometry) {
        self.data.translation = Some(*current);
    }

    /// コントロール窓の現在幾何をメモリに覚える。ファイルへの保存は終了時のみ
    pub fn track_control(&mut self, current: &WindowGeometry) {
        self.data.control = Some(*current);
    }

    /// その場で全項目を保存する。パスなしのときは何もしない
    fn save_now(&mut self) {
        let Some(path) = &self.path else { return };
        if let Err(e) = write_file(path, &self.data) {
            dev_log!("⚠️ 設定の保存に失敗: {e}");
        }
    }
}

/// 保存済み設定を読む。存在しない・壊れている場合は None
fn read_file(path: &Path) -> Option<StoreData> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            dev_log!("⚠️ 設定ファイルを読めません（{}）: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(data) => Some(data),
        Err(e) => {
            dev_log!("⚠️ 設定ファイルが壊れています（{}）: {e}", path.display());
            None
        }
    }
}

/// 全項目を保存する。親ディレクトリがなければ作る
fn write_file(path: &Path, data: &StoreData) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(data).expect("store serialization");
    std::fs::write(path, text)?;
    restrict_permissions(path);
    Ok(())
}

/// APIキーを含むため所有者のみ読み書き可にする
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        dev_log!("⚠️ 設定ファイルの権限変更に失敗: {e}");
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// GUI とバックグラウンドタスクで共有する保存口
pub type SharedStore = Arc<Mutex<Store>>;

pub fn shared(store: Store) -> SharedStore {
    Arc::new(Mutex::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lume-sub-store-{}-{name}.json", std::process::id()))
    }

    fn sample_geometry() -> WindowGeometry {
        WindowGeometry {
            x: 320.5,
            y: 900.0,
            w: 800.0,
            h: 150.0,
        }
    }

    #[test]
    fn defaults_follow_on_english_empty_api() {
        let data = StoreData::default();
        assert!(data.follow_main_screen);
        assert!(data.show_source);
        assert!(data.show_translation);
        assert!(!data.fast_translation);
        assert!(!data.dev_logs);
        assert_eq!(data.source_tail_chars, crate::text::DEFAULT_TAIL_CHARS);
        assert_eq!(data.translation_tail_chars, crate::text::DEFAULT_TAIL_CHARS);
        assert!(data.source_font.is_empty());
        assert_eq!(data.source_font_size, crate::fonts::DEFAULT_SUBTITLE_SIZE);
        assert!(data.translation_font.is_empty());
        assert_eq!(data.translation_font_size, crate::fonts::DEFAULT_SUBTITLE_SIZE);
        assert_eq!(data.ui_locale, UiLocale::English);
        assert_eq!(data.api, ApiConfig::default());
        assert_eq!(data.languages, crate::language::LanguageConfig::default());
        assert_eq!(data.source, None);
        assert_eq!(data.translation, None);
        assert_eq!(data.control, None);
        // 空オブジェクトでも既定が効く
        let data: StoreData = serde_json::from_str("{}").unwrap();
        assert!(data.follow_main_screen);
        assert!(data.show_source);
        assert!(data.show_translation);
        assert!(!data.fast_translation);
        assert!(!data.dev_logs);
        assert_eq!(data.source_tail_chars, crate::text::DEFAULT_TAIL_CHARS);
        assert_eq!(data.translation_tail_chars, crate::text::DEFAULT_TAIL_CHARS);
        assert!(data.source_font.is_empty());
        assert_eq!(data.source_font_size, crate::fonts::DEFAULT_SUBTITLE_SIZE);
        assert!(data.translation_font.is_empty());
        assert_eq!(data.translation_font_size, crate::fonts::DEFAULT_SUBTITLE_SIZE);
        assert_eq!(data.ui_locale, UiLocale::English);
        assert_eq!(data.languages, crate::language::LanguageConfig::default());
    }

    #[test]
    fn roundtrip_save_load() {
        let path = scratch("roundtrip");
        let mut store = Store::load_from(Some(path.clone()));
        store.set_follow_main_screen(false);
        store.set_fast_translation(true);
        store.set_dev_logs(true);
        store.set_source_tail_chars(80);
        store.set_translation_tail_chars(120);
        store.set_source_font("jp-hiragino".to_string());
        store.set_source_font_size(32.0);
        store.set_translation_font("kr-gothic".to_string());
        store.set_translation_font_size(24.0);
        store.set_ui_locale(UiLocale::Japanese);
        store.set_api(ApiConfig {
            api_key: "key".to_string(),
            base_url: "https://example.test/v1".to_string(),
            asr_model: "asr".to_string(),
            translate_model: "tr".to_string(),
        });
        let loaded = Store::load_from(Some(path.clone()));
        assert_eq!(loaded.data, store.data);
        assert_eq!(loaded.source(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn geometries_roundtrip() {
        let path = scratch("geometries");
        let mut store = Store::load_from(Some(path.clone()));
        store.data.source = Some(sample_geometry());
        store.data.translation = Some(sample_geometry());
        store.data.control = Some(sample_geometry());
        store.save_now();
        let loaded = Store::load_from(Some(path.clone()));
        assert_eq!(loaded.source(), Some(sample_geometry()));
        assert_eq!(loaded.translation(), Some(sample_geometry()));
        assert_eq!(loaded.control(), Some(sample_geometry()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exit_geometries_persist_without_throttle() {
        // 終了時点の窓位置は間引きなしで確定保存する
        let path = scratch("exit-geoms");
        let mut store = Store::load_from(Some(path.clone()));
        let g = sample_geometry();
        store.persist_geometries(Some(g), Some(g), Some(g));
        let loaded = Store::load_from(Some(path.clone()));
        assert_eq!(loaded.source(), Some(g));
        assert_eq!(loaded.translation(), Some(g));
        assert_eq!(loaded.control(), Some(g));
        // None の窓は据え置く
        store.persist_geometries(None, None, None);
        let loaded = Store::load_from(Some(path.clone()));
        assert_eq!(loaded.source(), Some(g));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cannot_hide_both_subtitles() {
        let path = scratch("shows");
        let mut store = Store::load_from(Some(path.clone()));
        assert!(store.set_shows(true, true));
        assert!(store.set_shows(false, true));
        assert!(!store.show_source());
        assert!(store.set_shows(true, false));
        assert!(store.show_source());
        assert!(!store.set_shows(false, false));
        // 拒否時は据え置き
        assert!(store.show_source());
        assert!(!store.show_translation());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn both_hidden_is_repaired_on_load() {
        let path = scratch("both-hidden");
        std::fs::write(&path, r#"{"show_source":false,"show_translation":false}"#).unwrap();
        let store = Store::load_from(Some(path.clone()));
        assert!(store.show_source());
        assert!(store.show_translation());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn all_locales_roundtrip() {
        for l in UiLocale::all() {
            let path = scratch(l.code());
            let mut store = Store::load_from(Some(path.clone()));
            store.set_ui_locale(*l);
            let loaded = Store::load_from(Some(path.clone()));
            assert_eq!(loaded.ui_locale(), *l);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn missing_or_corrupt_loads_default() {
        let store = Store::load_from(Some(scratch("definitely-missing")));
        assert_eq!(store.data, StoreData::default());
        let path = scratch("corrupt");
        std::fs::write(&path, "{oops").unwrap();
        let store = Store::load_from(Some(path.clone()));
        assert_eq!(store.data, StoreData::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fresh_until_file_exists() {
        let path = scratch("fresh");
        assert!(Store::load_from(Some(path.clone())).is_fresh());
        let mut store = Store::load_from(Some(path.clone()));
        store.set_follow_main_screen(false);
        assert!(!Store::load_from(Some(path.clone())).is_fresh());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn languages_roundtrip() {
        use crate::language::{Language, SourceLanguage};
        let path = scratch("languages");
        let mut store = Store::load_from(Some(path.clone()));
        assert_eq!(store.language_config(), crate::language::LanguageConfig::default());
        store.set_languages(
            SourceLanguage::Specific(Language::English),
            Language::Korean,
        );
        let loaded = Store::load_from(Some(path.clone()));
        assert_eq!(
            loaded.language_config(),
            crate::language::LanguageConfig {
                source: SourceLanguage::Specific(Language::English),
                target: Language::Korean,
            }
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn live_geometry_reflects_immediately_without_file() {
        let path = scratch("live-geom");
        let mut store = Store::load_from(Some(path.clone()));
        assert_eq!(store.source(), None);
        let g = sample_geometry();
        store.set_source_geometry(g);
        // メモリには即反映するがファイルは書かない
        assert_eq!(store.source(), Some(g));
        assert!(!path.exists());
        store.set_translation_geometry(g);
        assert_eq!(store.translation(), Some(g));
        assert!(!path.exists());
    }

    #[test]
    fn geometry_tracking_writes_file_only_on_exit() {
        let path = scratch("track-geom");
        let mut store = Store::load_from(Some(path.clone()));
        let g = sample_geometry();
        store.track_source(&g);
        store.track_translation(&g);
        store.track_control(&g);
        // メモリには即反映するがファイルは書かない
        assert_eq!(store.source(), Some(g));
        assert_eq!(store.translation(), Some(g));
        assert_eq!(store.control(), Some(g));
        assert!(!path.exists());
        // 終了時の確定保存で初めて書く
        store.persist_geometries(store.source(), store.translation(), store.control());
        assert!(path.exists());
        let loaded = Store::load_from(Some(path.clone()));
        assert_eq!(loaded.source(), Some(g));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[cfg(unix)]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("perms");
        let mut store = Store::load_from(Some(path.clone()));
        store.set_follow_main_screen(false);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(&path);
    }
}

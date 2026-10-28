//! UI表示言語の多言語化
//!
//! 字幕パイプラインの入出力言語（[`language`](crate::language)）とは別に、
//! 操作ウィンドウ・字幕メッセージなどのUI文言を切り替える。
//! 全文言は [`UiStrings`] ひとつに集約し、[`UiLocale::strings`] で取得する。
//! 文言を足すときは全ロケールの定数と網羅テストに足すこと。
//! 選択は `~/.config/lume-sub/ui_locale.json` に保存し、次回起動時に復元する。

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// UI表示言語
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiLocale {
    #[default]
    Japanese,
    English,
}

impl UiLocale {
    pub fn all() -> &'static [UiLocale] {
        use UiLocale::*;
        &[Japanese, English]
    }

    /// 短縮コード（設定ファイル・環境変数・ログ用）
    pub fn code(self) -> &'static str {
        match self {
            UiLocale::Japanese => "ja",
            UiLocale::English => "en",
        }
    }

    /// 選択UIに並べる自称
    pub fn display_name(self) -> &'static str {
        match self {
            UiLocale::Japanese => "日本語",
            UiLocale::English => "English",
        }
    }

    /// このロケールの文言表
    pub fn strings(self) -> &'static UiStrings {
        match self {
            UiLocale::Japanese => &JA_STRINGS,
            UiLocale::English => &EN_STRINGS,
        }
    }

    /// `UI_LANG` 環境変数から読む。未設定なら None
    pub fn from_env() -> Option<Self> {
        let v = std::env::var("UI_LANG")
            .ok()
            .filter(|v| !v.is_empty())?;
        match v.parse() {
            Ok(l) => Some(l),
            Err(e) => {
                println!("⚠️ {e}（表示言語は日本語にします）");
                Some(UiLocale::Japanese)
            }
        }
    }
}

impl FromStr for UiLocale {
    type Err = String;

    /// コード・英語名・自称を受け付ける（大小無視）
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "ja" | "japanese" | "日本語" => Ok(UiLocale::Japanese),
            "en" | "english" => Ok(UiLocale::English),
            _ => Err(format!("unknown UI locale: {s}")),
        }
    }
}

/// UI文言の一元管理型。`{}` を含む文は直接触らず、対応するメソッドで埋める
/// （ロケールによって語順が違うため）。実体はロケール別の定数のみ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiStrings {
    pub display_language: &'static str,
    pub status_stopped: &'static str,
    pub status_running: &'static str,
    pub status_paused: &'static str,
    pub input_language: &'static str,
    pub output_language: &'static str,
    pub auto_detect: &'static str,
    pub lang_read_failed: &'static str,
    pub input_note: &'static str,
    pub start: &'static str,
    pub resume: &'static str,
    pub pause: &'static str,
    pub stop: &'static str,
    pub quit: &'static str,
    pub reset_position: &'static str,
    pub control_title: &'static str,
    pub connecting: &'static str,
    pub play_prompt: &'static str,
    pub drag_to_move: &'static str,
    tpl_status_line: &'static str,
    tpl_settings_line: &'static str,
    tpl_listening: &'static str,
    tpl_unsupported_audio: &'static str,
}

impl UiStrings {
    fn fill(template: &str, arg: &str) -> String {
        template.replacen("{}", arg, 1)
    }

    /// 状態行（例: `状態: ▶ 動作中`）
    pub fn status_line(&self, label: &str) -> String {
        Self::fill(self.tpl_status_line, label)
    }

    /// 言語設定行（例: `設定: auto → 日本語`）
    pub fn settings_line(&self, describe: &str) -> String {
        Self::fill(self.tpl_settings_line, describe)
    }

    /// ASR開始直後の字幕（例: `🎧 auto → 日本語 聞き取り中...`）
    pub fn listening_text(&self, pair: &str) -> String {
        Self::fill(self.tpl_listening, pair)
    }

    /// 未対応音声フォーマットの字幕
    pub fn unsupported_audio_text(&self, reason: &str) -> String {
        Self::fill(self.tpl_unsupported_audio, reason)
    }
}

const JA_STRINGS: UiStrings = UiStrings {
    display_language: "表示言語",
    status_stopped: "⏹ 停止中",
    status_running: "▶ 動作中",
    status_paused: "⏸ 一時停止中",
    input_language: "入力言語",
    output_language: "出力言語",
    auto_detect: "auto（自動検出）",
    lang_read_failed: "言語設定の読み取りに失敗",
    input_note: "※ 入力言語は次回接続から反映（停止→開始）",
    start: "▶ 開始",
    resume: "▶ 再開",
    pause: "⏸ 一時停止",
    stop: "⏹ 停止",
    quit: "⏻ 終了",
    reset_position: "字幕位置をリセット",
    control_title: "LumeSub Control",
    connecting: "ASRに接続中...",
    play_prompt: "YouTubeなどを再生して音を鳴らしてください...",
    drag_to_move: "ドラッグで移動",
    tpl_status_line: "状態: {}",
    tpl_settings_line: "設定: {}",
    tpl_listening: "🎧 {} 聞き取り中...",
    tpl_unsupported_audio: "⚠️ 音声フォーマット未対応: {}",
};

const EN_STRINGS: UiStrings = UiStrings {
    display_language: "Display language",
    status_stopped: "⏹ Stopped",
    status_running: "▶ Running",
    status_paused: "⏸ Paused",
    input_language: "Input language",
    output_language: "Output language",
    auto_detect: "auto (auto-detect)",
    lang_read_failed: "Failed to read language settings",
    input_note: "* Input language applies on next connection (Stop → Start)",
    start: "▶ Start",
    resume: "▶ Resume",
    pause: "⏸ Pause",
    stop: "⏹ Stop",
    quit: "⏻ Quit",
    reset_position: "Reset subtitle position",
    control_title: "LumeSub Control",
    connecting: "Connecting to ASR...",
    play_prompt: "Play something with audio (e.g. YouTube)...",
    drag_to_move: "drag to move",
    tpl_status_line: "Status: {}",
    tpl_settings_line: "Settings: {}",
    tpl_listening: "🎧 Listening ({})...",
    tpl_unsupported_audio: "⚠️ Unsupported audio format: {}",
};

/// 保存先。`~/.config/lume-sub/ui_locale.json`（`XDG_CONFIG_HOME` 尊重）
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lume-sub").join("ui_locale.json"))
}

/// 保存済みロケールを読む。存在しない・壊れている場合は None
pub fn load(path: &std::path::Path) -> Option<UiLocale> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            println!("⚠️ 表示言語ファイルを読めません（{}）: {e}", path.display());
            return None;
        }
    };
    let code: String = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            println!("⚠️ 表示言語ファイルが壊れています（{}）: {e}", path.display());
            return None;
        }
    };
    match code.parse() {
        Ok(l) => Some(l),
        Err(e) => {
            println!("⚠️ 表示言語ファイルが壊れています（{}）: {e}", path.display());
            None
        }
    }
}

/// ロケールを保存する。親ディレクトリがなければ作る
pub fn save(path: &std::path::Path, locale: UiLocale) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&locale.code()).expect("locale serialization");
    std::fs::write(path, text)
}

/// GUI とバックグラウンドタスクで共有する表示言語。操作ウィンドウから実行時変更する
pub type SharedUiLocale = Arc<Mutex<UiLocale>>;

pub fn shared(locale: UiLocale) -> SharedUiLocale {
    Arc::new(Mutex::new(locale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codes_names_and_display() {
        assert_eq!("ja".parse::<UiLocale>().unwrap(), UiLocale::Japanese);
        assert_eq!("JA".parse::<UiLocale>().unwrap(), UiLocale::Japanese);
        assert_eq!("Japanese".parse::<UiLocale>().unwrap(), UiLocale::Japanese);
        assert_eq!("日本語".parse::<UiLocale>().unwrap(), UiLocale::Japanese);
        assert_eq!("en".parse::<UiLocale>().unwrap(), UiLocale::English);
        assert_eq!("EN".parse::<UiLocale>().unwrap(), UiLocale::English);
        assert_eq!("English".parse::<UiLocale>().unwrap(), UiLocale::English);
        assert!("xx".parse::<UiLocale>().is_err());
    }

    #[test]
    fn default_is_japanese() {
        assert_eq!(UiLocale::default(), UiLocale::Japanese);
    }

    #[test]
    fn all_locales_cover_every_string() {
        for l in UiLocale::all() {
            let t = l.strings();
            // 文言を足したらここにも足すこと
            for s in [
                t.display_language,
                t.status_stopped,
                t.status_running,
                t.status_paused,
                t.input_language,
                t.output_language,
                t.auto_detect,
                t.lang_read_failed,
                t.input_note,
                t.start,
                t.resume,
                t.pause,
                t.stop,
                t.quit,
                t.reset_position,
                t.control_title,
                t.connecting,
                t.play_prompt,
                t.drag_to_move,
            ] {
                assert!(!s.is_empty(), "{l:?} has an empty string");
            }
            for tpl in [
                t.tpl_status_line,
                t.tpl_settings_line,
                t.tpl_listening,
                t.tpl_unsupported_audio,
            ] {
                assert_eq!(tpl.matches("{}").count(), 1, "{l:?} template: {tpl}");
            }
        }
    }

    #[test]
    fn templates_fill_in_locale_word_order() {
        let ja = UiLocale::Japanese.strings();
        assert_eq!(ja.status_line("X"), "状態: X");
        assert_eq!(ja.settings_line("Y"), "設定: Y");
        assert_eq!(ja.listening_text("P"), "🎧 P 聞き取り中...");
        assert_eq!(ja.unsupported_audio_text("R"), "⚠️ 音声フォーマット未対応: R");
        let en = UiLocale::English.strings();
        assert_eq!(en.status_line("X"), "Status: X");
        assert_eq!(en.settings_line("Y"), "Settings: Y");
        assert_eq!(en.listening_text("P"), "🎧 Listening (P)...");
        assert_eq!(
            en.unsupported_audio_text("R"),
            "⚠️ Unsupported audio format: R"
        );
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lume-sub-locale-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn roundtrip_save_load() {
        for l in UiLocale::all() {
            let path = scratch(l.code());
            save(&path, *l).unwrap();
            assert_eq!(load(&path), Some(*l));
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn missing_or_corrupt_loads_none() {
        assert_eq!(load(&scratch("definitely-missing")), None);
        let path = scratch("corrupt");
        std::fs::write(&path, "{oops").unwrap();
        assert_eq!(load(&path), None);
        let _ = std::fs::remove_file(&path);
        let path = scratch("unknown-code");
        std::fs::write(&path, "\"xx\"").unwrap();
        assert_eq!(load(&path), None);
        let _ = std::fs::remove_file(&path);
    }
}

//! 入出力言語の選択
//!
//! ASR の25言語を [`Language`] で表し、入力（自動検出可）と出力を
//! [`LanguageConfig`] ひとつにまとめる。実行時の切替（将来のGUI用）に備え、
//! パイプライン間では [`SharedLanguageConfig`] で共有する。

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// ASR が対応する25言語。UI表示言語としてもそのまま使う
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Language {
    Arabic,
    Bengali,
    Dutch,
    /// 既定（UI表示用）
    #[default]
    English,
    French,
    German,
    Hebrew,
    Hindi,
    Indonesian,
    Italian,
    Japanese,
    Kannada,
    Korean,
    Malay,
    Mandarin,
    Marathi,
    Polish,
    Portuguese,
    Spanish,
    Tagalog,
    Tamil,
    Telugu,
    Thai,
    Turkish,
    Vietnamese,
}

impl Language {
    pub fn all() -> &'static [Language] {
        use Language::*;
        &[
            Arabic, Bengali, Dutch, English, French, German, Hebrew, Hindi, Indonesian, Italian,
            Japanese, Kannada, Korean, Malay, Mandarin, Marathi, Polish, Portuguese, Spanish,
            Tagalog, Tamil, Telugu, Thai, Turkish, Vietnamese,
        ]
    }

    /// 短縮コード（設定・ログ用）
    pub fn code(self) -> &'static str {
        use Language::*;
        match self {
            Arabic => "ar",
            Bengali => "bn",
            Dutch => "nl",
            English => "en",
            French => "fr",
            German => "de",
            Hebrew => "he",
            Hindi => "hi",
            Indonesian => "id",
            Italian => "it",
            Japanese => "ja",
            Kannada => "kn",
            Korean => "ko",
            Malay => "ms",
            Mandarin => "zh",
            Marathi => "mr",
            Polish => "pl",
            Portuguese => "pt",
            Spanish => "es",
            Tagalog => "tl",
            Tamil => "ta",
            Telugu => "te",
            Thai => "th",
            Turkish => "tr",
            Vietnamese => "vi",
        }
    }

    /// API の `languageBias` に渡す英語名
    pub fn api_name(self) -> &'static str {
        use Language::*;
        match self {
            Arabic => "Arabic",
            Bengali => "Bengali",
            Dutch => "Dutch",
            English => "English",
            French => "French",
            German => "German",
            Hebrew => "Hebrew",
            Hindi => "Hindi",
            Indonesian => "Indonesian",
            Italian => "Italian",
            Japanese => "Japanese",
            Kannada => "Kannada",
            Korean => "Korean",
            Malay => "Malay",
            Mandarin => "Mandarin Chinese",
            Marathi => "Marathi",
            Polish => "Polish",
            Portuguese => "Portuguese",
            Spanish => "Spanish",
            Tagalog => "Tagalog",
            Tamil => "Tamil",
            Telugu => "Telugu",
            Thai => "Thai",
            Turkish => "Turkish",
            Vietnamese => "Vietnamese",
        }
    }

    /// 各言語の自称（表示用）
    pub fn display_name(self) -> &'static str {
        use Language::*;
        match self {
            Arabic => "العربية",
            Bengali => "বাংলা",
            Dutch => "Nederlands",
            English => "English",
            French => "Français",
            German => "Deutsch",
            Hebrew => "עברית",
            Hindi => "हिन्दी",
            Indonesian => "Indonesia",
            Italian => "Italiano",
            Japanese => "日本語",
            Kannada => "ಕನ್ನಡ",
            Korean => "한국어",
            Malay => "Melayu",
            Mandarin => "中文",
            Marathi => "मराठी",
            Polish => "Polski",
            Portuguese => "Português",
            Spanish => "Español",
            Tagalog => "Tagalog",
            Tamil => "தமிழ்",
            Telugu => "తెలుగు",
            Thai => "ไทย",
            Turkish => "Türkçe",
            Vietnamese => "Tiếng Việt",
        }
    }
}

impl FromStr for Language {
    type Err = String;

    /// コード・英語名・自称を受け付ける（大小無視）
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let q = s.trim().to_lowercase();
        // よくある別名
        if q == "mandarin" || q == "chinese" || q == "cmn" {
            return Ok(Language::Mandarin);
        }
        Language::all()
            .iter()
            .find(|l| l.code() == q || l.api_name().to_lowercase() == q || l.display_name() == s.trim())
            .copied()
            .ok_or_else(|| format!("unknown language: {s}"))
    }
}

/// 入力言語。Auto はモデルに検出させる
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SourceLanguage {
    #[default]
    Auto,
    Specific(Language),
}

impl FromStr for SourceLanguage {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim().eq_ignore_ascii_case("auto") {
            Ok(SourceLanguage::Auto)
        } else {
            Language::from_str(s).map(SourceLanguage::Specific)
        }
    }
}

fn default_target() -> Language {
    Language::Japanese
}

/// 入出力言語の組
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageConfig {
    #[serde(default)]
    pub source: SourceLanguage,
    #[serde(default = "default_target")]
    pub target: Language,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            source: SourceLanguage::Auto,
            target: Language::Japanese,
        }
    }
}

impl LanguageConfig {
    /// ASR ハンドシェイクの `languageBias` 用。自動検出時は None
    pub fn language_bias(&self) -> Option<Vec<String>> {
        match self.source {
            SourceLanguage::Auto => None,
            SourceLanguage::Specific(l) => Some(vec![l.api_name().to_string()]),
        }
    }

    /// 翻訳APIを呼ぶ必要があるか。入力と出力が明示的に同じ言語なら不要
    pub fn needs_translation(&self) -> bool {
        match self.source {
            SourceLanguage::Auto => true,
            SourceLanguage::Specific(s) => s != self.target,
        }
    }

    /// ログ・字幕用の短い表記（例: `auto → 日本語`）
    pub fn describe(&self) -> String {
        let src = match self.source {
            SourceLanguage::Auto => "auto".to_string(),
            SourceLanguage::Specific(l) => l.display_name().to_string(),
        };
        format!("{} → {}", src, self.target.display_name())
    }
}

/// パイプライン間で共有する言語設定。GUI 等からの実行時変更に備える
pub type SharedLanguageConfig = Arc<Mutex<LanguageConfig>>;

pub fn shared(config: LanguageConfig) -> SharedLanguageConfig {
    Arc::new(Mutex::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codes_names_and_aliases() {
        assert_eq!("ja".parse::<Language>().unwrap(), Language::Japanese);
        assert_eq!("JA".parse::<Language>().unwrap(), Language::Japanese);
        assert_eq!("Japanese".parse::<Language>().unwrap(), Language::Japanese);
        assert_eq!("日本語".parse::<Language>().unwrap(), Language::Japanese);
        assert_eq!("mandarin".parse::<Language>().unwrap(), Language::Mandarin);
        assert_eq!("zh".parse::<Language>().unwrap(), Language::Mandarin);
        assert!("klingon".parse::<Language>().is_err());
    }

    #[test]
    fn default_is_english() {
        assert_eq!(Language::default(), Language::English);
    }

    #[test]
    fn all_lists_25_unique_languages() {
        let all = Language::all();
        assert_eq!(all.len(), 25);
        let mut codes = std::collections::HashSet::new();
        for l in all {
            assert!(codes.insert(l.code()), "duplicate code: {}", l.code());
        }
    }

    #[test]
    fn source_auto_and_specific() {
        assert_eq!("auto".parse::<SourceLanguage>().unwrap(), SourceLanguage::Auto);
        assert_eq!(
            "en".parse::<SourceLanguage>().unwrap(),
            SourceLanguage::Specific(Language::English)
        );
    }

    #[test]
    fn bias_only_for_specific_source() {
        let auto = LanguageConfig::default();
        assert_eq!(auto.language_bias(), None);
        let en = LanguageConfig {
            source: SourceLanguage::Specific(Language::English),
            target: Language::Japanese,
        };
        assert_eq!(en.language_bias(), Some(vec!["English".to_string()]));
    }

    #[test]
    fn describe_format() {
        assert_eq!(LanguageConfig::default().describe(), "auto → 日本語");
    }

    #[test]
    fn same_language_needs_no_translation() {
        let same = LanguageConfig {
            source: SourceLanguage::Specific(Language::Japanese),
            target: Language::Japanese,
        };
        assert!(!same.needs_translation());
        assert!(LanguageConfig::default().needs_translation());
        let diff = LanguageConfig {
            source: SourceLanguage::Specific(Language::English),
            target: Language::Japanese,
        };
        assert!(diff.needs_translation());
    }
}

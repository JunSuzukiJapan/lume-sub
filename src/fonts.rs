//! 多言語フォント管理
//!
//! egui の既定フォントはラテン文字中心（Ubuntu-Light）のため、日本語などの
//! 字幕は豆腐（□）になる。macOS のシステムフォントをスクリプト別に読み込み、
//! egui のフォールバックチェーンに組み込む。`.ttc` は [`egui::FontData::index`]
//! で先頭フェイスを使う（ab_glyph がコレクションに対応）。

use crate::dev_log;
use crate::language::Script;

/// 字幕フォントサイズの既定（pt）
pub const DEFAULT_SUBTITLE_SIZE: f32 = 28.0;
/// 字幕フォントサイズの調整範囲（pt）
pub const FONT_SIZE_MIN: f32 = 12.0;
pub const FONT_SIZE_MAX: f32 = 64.0;

/// 読み込み候補のシステムフォント1件。配列順＝フォールバック優先順
#[derive(Clone, Copy)]
pub struct FontCandidate {
    /// egui 上のフォントID
    pub id: &'static str,
    /// システムフォントのパス
    pub path: &'static str,
    /// カバーする言語・スクリプトの表示名
    pub scripts: &'static str,
    /// `.ttc` 内のフェイス番号
    pub index: u32,
    /// 字幕フォント選択肢としての表示名。None＝裏側の多言語
    /// フォールバック専用で選択肢に出さない
    pub style: Option<&'static str>,
    /// 選択肢に出すスクリプト群。`style` と対（Some/None一致）
    pub script: Option<Script>,
}

/// 読み込み済みフォントの概要
pub struct LoadedFont {
    pub scripts: String,
    pub bytes: usize,
}

pub struct FontManager {
    fonts: Vec<(FontCandidate, Vec<u8>)>,
    missing: Vec<&'static str>,
}

impl FontManager {
    /// 候補テーブル。ASR 25言語のスクリプトをカバーする。
    /// 選択肢用の書体はチェーン末尾に置き、既定の表示を変えない。
    /// 丸ゴは先頭フェイスが Pro のため ProN（index 1）を、
    /// Noto Kannada は Regular（index 3）を使う
    pub fn candidates() -> &'static [FontCandidate] {
        use Script::*;
        &[
            FontCandidate { id: "jp-hiragino", path: "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc", scripts: "Japanese gothic", index: 0, style: Some("ヒラギノ角ゴシック"), script: Some(Japanese) },
            FontCandidate { id: "cjk-symbols", path: "/System/Library/Fonts/CJKSymbolsFallback.ttc", scripts: "CJK symbols", index: 0, style: None, script: None },
            FontCandidate { id: "zh-heiti", path: "/System/Library/Fonts/STHeiti Medium.ttc", scripts: "Chinese", index: 0, style: Some("STHeiti"), script: Some(Mandarin) },
            FontCandidate { id: "kr-gothic", path: "/System/Library/Fonts/AppleSDGothicNeo.ttc", scripts: "Korean", index: 0, style: Some("Apple SD Gothic Neo"), script: Some(Korean) },
            FontCandidate { id: "th-thonburi", path: "/System/Library/Fonts/ThonburiUI.ttc", scripts: "Thai", index: 0, style: Some("Thonburi"), script: Some(Thai) },
            FontCandidate { id: "ar-geeza", path: "/System/Library/Fonts/GeezaPro.ttc", scripts: "Arabic", index: 0, style: Some("Geeza Pro"), script: Some(Arabic) },
            FontCandidate { id: "he-peninim", path: "/System/Library/Fonts/Supplemental/NewPeninimMT.ttc", scripts: "Hebrew", index: 0, style: Some("New Peninim"), script: Some(Hebrew) },
            FontCandidate { id: "in-devanagari", path: "/System/Library/Fonts/Kohinoor.ttc", scripts: "Hindi/Marathi", index: 0, style: Some("Kohinoor"), script: Some(Devanagari) },
            FontCandidate { id: "in-bengali", path: "/System/Library/Fonts/KohinoorBangla.ttc", scripts: "Bengali", index: 0, style: Some("Kohinoor Bangla"), script: Some(Bengali) },
            FontCandidate { id: "in-tamil", path: "/System/Library/Fonts/Supplemental/Tamil Sangam MN.ttc", scripts: "Tamil", index: 0, style: Some("Tamil Sangam MN"), script: Some(Tamil) },
            FontCandidate { id: "in-telugu", path: "/System/Library/Fonts/Supplemental/Telugu Sangam MN.ttc", scripts: "Telugu", index: 0, style: Some("Telugu Sangam MN"), script: Some(Telugu) },
            FontCandidate { id: "in-kannada", path: "/System/Library/Fonts/Supplemental/Kannada Sangam MN.ttc", scripts: "Kannada", index: 0, style: Some("Kannada Sangam MN"), script: Some(Kannada) },
            // 最後の砦：ほぼ全スクリプトを含むメガフォント
            FontCandidate { id: "pan-unicode", path: "/System/Library/Fonts/Supplemental/Arial Unicode.ttf", scripts: "pan-Unicode fallback", index: 0, style: None, script: None },
            FontCandidate { id: "jp-hiragino-bold", path: "/System/Library/Fonts/ヒラギノ角ゴシック W6.ttc", scripts: "Japanese gothic bold", index: 0, style: Some("ヒラギノ角ゴシック 太字"), script: Some(Japanese) },
            FontCandidate { id: "jp-mincho", path: "/System/Library/Fonts/ヒラギノ明朝 ProN.ttc", scripts: "Japanese mincho", index: 0, style: Some("ヒラギノ明朝"), script: Some(Japanese) },
            FontCandidate { id: "jp-maru", path: "/System/Library/Fonts/ヒラギノ丸ゴ ProN W4.ttc", scripts: "Japanese maru gothic", index: 1, style: Some("ヒラギノ丸ゴシック"), script: Some(Japanese) },
            FontCandidate { id: "latin-helvetica", path: "/System/Library/Fonts/Helvetica.ttc", scripts: "Latin sans", index: 0, style: Some("Helvetica"), script: Some(Latin) },
            FontCandidate { id: "latin-palatino", path: "/System/Library/Fonts/Palatino.ttc", scripts: "Latin serif", index: 0, style: Some("Palatino"), script: Some(Latin) },
            FontCandidate { id: "latin-courier", path: "/System/Library/Fonts/Courier.ttc", scripts: "Latin mono", index: 0, style: Some("Courier"), script: Some(Latin) },
            FontCandidate { id: "zh-sansgb", path: "/System/Library/Fonts/Hiragino Sans GB.ttc", scripts: "Chinese simplified gothic", index: 0, style: Some("Hiragino Sans GB"), script: Some(Mandarin) },
            FontCandidate { id: "kr-myungjo", path: "/System/Library/Fonts/Supplemental/AppleMyungjo.ttf", scripts: "Korean serif", index: 0, style: Some("AppleMyungjo"), script: Some(Korean) },
            FontCandidate { id: "th-ayuthaya", path: "/System/Library/Fonts/Supplemental/Ayuthaya.ttf", scripts: "Thai traditional", index: 0, style: Some("Ayuthaya"), script: Some(Thai) },
            FontCandidate { id: "th-silom", path: "/System/Library/Fonts/Supplemental/Silom.ttf", scripts: "Thai modern", index: 0, style: Some("Silom"), script: Some(Thai) },
            FontCandidate { id: "ar-baghdad", path: "/System/Library/Fonts/Supplemental/Baghdad.ttc", scripts: "Arabic modern", index: 0, style: Some("Baghdad"), script: Some(Arabic) },
            FontCandidate { id: "ar-naskh", path: "/System/Library/Fonts/Supplemental/DecoTypeNaskh.ttc", scripts: "Arabic naskh", index: 0, style: Some("DecoType Naskh"), script: Some(Arabic) },
            FontCandidate { id: "he-raanana", path: "/System/Library/Fonts/Supplemental/Raanana.ttc", scripts: "Hebrew modern", index: 0, style: Some("Raanana"), script: Some(Hebrew) },
            FontCandidate { id: "in-devanagari-sangam", path: "/System/Library/Fonts/Supplemental/Devanagari Sangam MN.ttc", scripts: "Hindi/Marathi sangam", index: 0, style: Some("Devanagari Sangam MN"), script: Some(Devanagari) },
            FontCandidate { id: "in-bengali-sangam", path: "/System/Library/Fonts/Supplemental/Bangla Sangam MN.ttc", scripts: "Bengali sangam", index: 0, style: Some("Bangla Sangam MN"), script: Some(Bengali) },
            FontCandidate { id: "in-tamil-inaimathi", path: "/System/Library/Fonts/Supplemental/InaiMathi-MN.ttc", scripts: "Tamil modern", index: 0, style: Some("InaiMathi"), script: Some(Tamil) },
            FontCandidate { id: "in-telugu-kohinoor", path: "/System/Library/Fonts/KohinoorTelugu.ttc", scripts: "Telugu modern", index: 0, style: Some("Kohinoor Telugu"), script: Some(Telugu) },
            FontCandidate { id: "in-kannada-noto", path: "/System/Library/Fonts/NotoSansKannada.ttc", scripts: "Kannada noto", index: 3, style: Some("Noto Sans Kannada"), script: Some(Kannada) },
        ]
    }

    /// 存在する候補を全て読み込む。欠けは警告して続行する
    pub fn load() -> Self {
        let mut fonts = Vec::new();
        let mut missing = Vec::new();
        for cand in Self::candidates() {
            match std::fs::read(cand.path) {
                Ok(data) => fonts.push((*cand, data)),
                Err(_) => {
                    dev_log!("⚠️ フォント未検出（{}）: {}", cand.scripts, cand.path);
                    missing.push(cand.id);
                }
            }
        }
        Self { fonts, missing }
    }

    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    pub fn loaded(&self) -> Vec<LoadedFont> {
        self.fonts
            .iter()
            .map(|(cand, data)| LoadedFont {
                scripts: cand.scripts.to_string(),
                bytes: data.len(),
            })
            .collect()
    }

    /// 選択肢用（ID・書体名・スクリプト）。字幕窓のフォント切替
    /// コンボに渡す。style のある書体のみ。裏側の多言語
    /// フォールバックは出さない
    pub fn loaded_styles(&self) -> Vec<(String, String, Script)> {
        self.fonts
            .iter()
            .filter_map(|(cand, _)| {
                cand.style
                    .zip(cand.script)
                    .map(|(label, script)| (cand.id.to_string(), label.to_string(), script))
            })
            .collect()
    }

    /// egui にフォントを登録し、フォールバックチェーンを組み立てる。
    /// `self` を消費してバイト列をムーブする（大容量のため clone しない）。
    /// 読み込めた候補ごとに「その字体優先」の名付きファミリも作る
    pub fn install(self, ctx: &egui::Context) {
        let mut defs = egui::FontDefinitions::default();
        let ids: Vec<String> = self.fonts.iter().map(|(c, _)| c.id.to_string()).collect();
        for (cand, data) in self.fonts {
            defs.font_data.insert(
                cand.id.to_owned(),
                egui::FontData {
                    index: cand.index,
                    ..egui::FontData::from_owned(data)
                },
            );
        }
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(chain) = defs.families.get_mut(&family) {
                *chain = insert_fallbacks(chain, &ids);
            }
        }
        let prop = defs
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        for id in &ids {
            defs.families.insert(
                egui::FontFamily::Name(id.clone().into()),
                preferred_chain(&prop, id),
            );
        }
        ctx.set_fonts(defs);
    }

    /// 起動ログ用のカバレッジ概要
    pub fn report(&self) -> String {
        let loaded = self.loaded();
        let total_mb: f64 = loaded.iter().map(|f| f.bytes).sum::<usize>() as f64 / 1e6;
        let mut s = format!("🔤 フォント {}件（{:.1}MB）: ", self.len(), total_mb);
        s.push_str(
            &loaded
                .iter()
                .map(|f| f.scripts.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        if !self.missing.is_empty() {
            s.push_str(&format!("（未検出: {}）", self.missing.join(", ")));
        }
        s
    }
}

/// 既定チェーンの先頭（ラテン用）の直後に多言語フォントを挿入する。
/// 先頭を残すことで英数字の見た目を変えず、グリフ欠けだけ拾う。
fn insert_fallbacks(base: &[String], ids: &[String]) -> Vec<String> {
    let mut chain = base.to_vec();
    let pos = chain
        .iter()
        .position(|name| name == "Ubuntu-Light" || name == "Hack")
        .map(|i| i + 1)
        .unwrap_or(0);
    for (i, id) in ids.iter().enumerate() {
        chain.insert(pos + i, id.clone());
    }
    chain
}

/// 指定IDを先頭にしたフォールバックチェーン。他スクリプトは既定で拾う
fn preferred_chain(base: &[String], id: &str) -> Vec<String> {
    let mut chain = vec![id.to_string()];
    chain.extend(base.iter().filter(|name| *name != id).cloned());
    chain
}

/// 言語別の選択肢（ID・書体名）。None（自動検出）＝全書体
pub fn choices_for(
    all: &[(String, String, Script)],
    script: Option<Script>,
) -> Vec<(String, String)> {
    all.iter()
        .filter(|(_, _, s)| script.is_none_or(|w| *s == w))
        .map(|(id, label, _)| (id.clone(), label.clone()))
        .collect()
}

/// 字幕用フォントID。空文字・未読込＝既定チェーン、候補ID＝その字体優先。
/// サイズは有限値に正す
pub fn subtitle_font_id(
    preferred: &str,
    loaded: &[(String, String)],
    size: f32,
) -> egui::FontId {
    let size = if size.is_finite() {
        size.clamp(8.0, 96.0)
    } else {
        DEFAULT_SUBTITLE_SIZE
    };
    if !preferred.is_empty() && loaded.iter().any(|(id, _)| id == preferred) {
        egui::FontId::new(size, egui::FontFamily::Name(preferred.into()))
    } else {
        egui::FontId::proportional(size)
    }
}

/// フルスクリーンパネル（AppKit）用のPostScript名。egui側が読む
/// `.ttc` 先頭フェイスに対応する。名前解決できない候補は None
/// （パネルはシステムフォントに落ち、不足グリフはカスケードで拾う）。
/// - `cjk-symbols`: 全フェイスがドット始まりの隠し名で解決不可
/// - `th-thonburi`: UI変種も隠し名のため、同デザインの非UI名を使う
pub fn panel_postscript(id: &str) -> Option<&'static str> {
    match id {
        "jp-hiragino" => Some("HiraginoSans-W3"),
        "jp-hiragino-bold" => Some("HiraginoSans-W6"),
        "jp-mincho" => Some("HiraMinProN-W3"),
        "jp-maru" => Some("HiraMaruProN-W4"),
        "latin-helvetica" => Some("Helvetica"),
        "latin-palatino" => Some("Palatino-Roman"),
        "latin-courier" => Some("Courier"),
        "zh-sansgb" => Some("HiraginoSansGB-W3"),
        "kr-myungjo" => Some("AppleMyungjo"),
        "th-ayuthaya" => Some("Ayuthaya"),
        "th-silom" => Some("Silom"),
        "ar-baghdad" => Some("Baghdad"),
        "ar-naskh" => Some("DecoTypeNaskh"),
        "he-raanana" => Some("Raanana"),
        "in-devanagari-sangam" => Some("DevanagariSangamMN"),
        "in-bengali-sangam" => Some("BanglaSangamMN"),
        "in-tamil-inaimathi" => Some("InaiMathi"),
        "in-telugu-kohinoor" => Some("KohinoorTelugu-Regular"),
        "in-kannada-noto" => Some("NotoSansKannada-Regular"),
        "zh-heiti" => Some("STHeitiTC-Medium"),
        "kr-gothic" => Some("AppleSDGothicNeo-Regular"),
        "th-thonburi" => Some("Thonburi"),
        "ar-geeza" => Some("GeezaPro"),
        "he-peninim" => Some("NewPeninimMT"),
        "in-devanagari" => Some("KohinoorDevanagari-Regular"),
        "in-bengali" => Some("KohinoorBangla-Regular"),
        "in-tamil" => Some("TamilSangamMN"),
        "in-telugu" => Some("TeluguSangamMN"),
        "in-kannada" => Some("KannadaSangamMN"),
        "pan-unicode" => Some("ArialUnicodeMS"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn candidate_table_is_sane() {
        let cands = FontManager::candidates();
        assert!(!cands.is_empty());
        let mut ids = HashSet::new();
        for cand in cands {
            assert!(ids.insert(cand.id), "duplicate id: {}", cand.id);
            assert!(cand.path.starts_with('/'), "not absolute: {}", cand.path);
            assert!(
                cand.path.ends_with(".ttf") || cand.path.ends_with(".ttc") || cand.path.ends_with(".otf"),
                "bad extension: {}",
                cand.path
            );
            assert!(!cand.scripts.is_empty());
        }
    }

    #[test]
    fn fallbacks_keep_latin_first_and_emoji_last() {
        let base = vec!["Ubuntu-Light".to_string(), "NotoEmoji-Regular".to_string()];
        let ids = vec!["jp-hiragino".to_string(), "pan-unicode".to_string()];
        let chain = insert_fallbacks(&base, &ids);
        assert_eq!(
            chain,
            vec!["Ubuntu-Light", "jp-hiragino", "pan-unicode", "NotoEmoji-Regular"]
        );
    }

    #[test]
    fn preferred_chain_puts_id_first_without_dupes() {
        let base = vec!["a".to_string(), "b".to_string()];
        assert_eq!(preferred_chain(&base, "b"), vec!["b", "a"]);
        assert_eq!(preferred_chain(&base, "z"), vec!["z", "a", "b"]);
    }

    fn loaded_sample() -> Vec<(String, String)> {
        vec![("jp".to_string(), "Japanese".to_string())]
    }

    #[test]
    fn subtitle_font_id_resolves() {
        let loaded = loaded_sample();
        // 空文字＝既定チェーン
        let id = subtitle_font_id("", &loaded, 28.0);
        assert_eq!(id.size, 28.0);
        assert!(matches!(id.family, egui::FontFamily::Proportional));
        // 読込済みID＝その字体優先
        let id = subtitle_font_id("jp", &loaded, 32.0);
        assert_eq!(id.size, 32.0);
        assert!(matches!(id.family, egui::FontFamily::Name(ref n) if n.to_string() == "jp"));
        // 未読込ID＝既定に落ちる
        let id = subtitle_font_id("nope", &loaded, 32.0);
        assert!(matches!(id.family, egui::FontFamily::Proportional));
    }

    #[test]
    fn subtitle_font_id_sanitizes_size() {
        let loaded = loaded_sample();
        assert_eq!(subtitle_font_id("", &loaded, 0.0).size, 8.0);
        assert_eq!(subtitle_font_id("", &loaded, 500.0).size, 96.0);
        assert_eq!(
            subtitle_font_id("", &loaded, f32::NAN).size,
            DEFAULT_SUBTITLE_SIZE
        );
    }

    #[test]
    fn loaded_styles_lists_styles_only() {
        let mgr = FontManager {
            fonts: vec![
                (
                    FontCandidate {
                        id: "s",
                        path: "/s",
                        scripts: "S-ish",
                        index: 0,
                        style: Some("書体S"),
                        script: Some(Script::Thai),
                    },
                    vec![0u8; 10],
                ),
                (
                    FontCandidate {
                        id: "c",
                        path: "/c",
                        scripts: "C-ish",
                        index: 0,
                        style: None,
                        script: None,
                    },
                    vec![0u8; 10],
                ),
            ],
            missing: vec![],
        };
        assert_eq!(
            mgr.loaded_styles(),
            vec![("s".to_string(), "書体S".to_string(), Script::Thai)]
        );
    }

    #[test]
    fn choices_for_filters_by_script() {
        let all = vec![
            ("a".to_string(), "A".to_string(), Script::Thai),
            ("b".to_string(), "B".to_string(), Script::Latin),
        ];
        assert_eq!(
            choices_for(&all, Some(Script::Thai)),
            vec![("a".to_string(), "A".to_string())]
        );
        assert_eq!(choices_for(&all, None).len(), 2);
        assert!(choices_for(&all, Some(Script::Arabic)).is_empty());
    }

    #[test]
    fn style_labels_are_unique_and_present() {
        let mut seen = HashSet::new();
        let mut scripts = HashSet::new();
        for cand in FontManager::candidates() {
            // style と script は対（Some/None一致）
            assert_eq!(
                cand.style.is_some(),
                cand.script.is_some(),
                "style/script mismatch: {}",
                cand.id
            );
            if let Some(label) = cand.style {
                assert!(seen.insert(label), "duplicate style label: {label}");
                scripts.insert(cand.script.unwrap());
            }
        }
        assert!(!seen.is_empty(), "no selectable styles");
        // 12群すべてに選択肢がある
        assert_eq!(scripts.len(), 12, "missing script groups");
    }

    #[test]
    fn report_mentions_loaded_scripts() {
        let mgr = FontManager {
            fonts: vec![(
                FontCandidate {
                    id: "x",
                    path: "/p",
                    scripts: "Japanese",
                    index: 0,
                    style: None,
                    script: None,
                },
                vec![0u8; 100],
            )],
            missing: vec!["y"],
        };
        let r = mgr.report();
        assert!(r.contains("Japanese"), "{r}");
        assert!(r.contains('y'), "{r}");
    }

    #[test]
    fn panel_postscript_covers_candidates() {
        for cand in FontManager::candidates() {
            match panel_postscript(cand.id) {
                Some(name) => assert!(!name.is_empty(), "{}", cand.id),
                None => assert_eq!(cand.id, "cjk-symbols", "unexpected unmapped: {}", cand.id),
            }
        }
        assert_eq!(panel_postscript(""), None);
        assert_eq!(panel_postscript("nope"), None);
    }

    /// 実機のシステムフォントが読めること（macOSのみ）
    #[cfg(target_os = "macos")]
    #[test]
    fn loads_system_fonts_on_macos() {
        let mgr = FontManager::load();
        assert!(mgr.len() > 0, "no system fonts loaded");
        let scripts: Vec<String> = mgr.loaded().iter().map(|f| f.scripts.clone()).collect();
        assert!(
            scripts
                .iter()
                .any(|s| s.starts_with("Japanese") || s == "pan-Unicode fallback"),
            "no CJK coverage: {scripts:?}"
        );
    }
}

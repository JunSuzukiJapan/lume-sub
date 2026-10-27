//! 多言語フォント管理
//!
//! egui の既定フォントはラテン文字中心（Ubuntu-Light）のため、日本語などの
//! 字幕は豆腐（□）になる。macOS のシステムフォントをスクリプト別に読み込み、
//! egui のフォールバックチェーンに組み込む。`.ttc` は [`egui::FontData::index`]
//! で先頭フェイスを使う（ab_glyph がコレクションに対応）。

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
    /// ラテン文字系は egui 既定のまま先頭に残し、ここには非ラテンを置く。
    pub fn candidates() -> &'static [FontCandidate] {
        &[
            FontCandidate { id: "jp-hiragino", path: "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc", scripts: "Japanese", index: 0 },
            FontCandidate { id: "cjk-symbols", path: "/System/Library/Fonts/CJKSymbolsFallback.ttc", scripts: "CJK symbols", index: 0 },
            FontCandidate { id: "zh-heiti", path: "/System/Library/Fonts/STHeiti Medium.ttc", scripts: "Chinese", index: 0 },
            FontCandidate { id: "kr-gothic", path: "/System/Library/Fonts/AppleSDGothicNeo.ttc", scripts: "Korean", index: 0 },
            FontCandidate { id: "th-thonburi", path: "/System/Library/Fonts/ThonburiUI.ttc", scripts: "Thai", index: 0 },
            FontCandidate { id: "ar-geeza", path: "/System/Library/Fonts/GeezaPro.ttc", scripts: "Arabic", index: 0 },
            FontCandidate { id: "he-peninim", path: "/System/Library/Fonts/Supplemental/NewPeninimMT.ttc", scripts: "Hebrew", index: 0 },
            FontCandidate { id: "in-devanagari", path: "/System/Library/Fonts/Kohinoor.ttc", scripts: "Hindi/Marathi", index: 0 },
            FontCandidate { id: "in-bengali", path: "/System/Library/Fonts/KohinoorBangla.ttc", scripts: "Bengali", index: 0 },
            FontCandidate { id: "in-tamil", path: "/System/Library/Fonts/Supplemental/Tamil Sangam MN.ttc", scripts: "Tamil", index: 0 },
            FontCandidate { id: "in-telugu", path: "/System/Library/Fonts/Supplemental/Telugu Sangam MN.ttc", scripts: "Telugu", index: 0 },
            FontCandidate { id: "in-kannada", path: "/System/Library/Fonts/Supplemental/Kannada Sangam MN.ttc", scripts: "Kannada", index: 0 },
            // 最後の砦：ほぼ全スクリプトを含むメガフォント
            FontCandidate { id: "pan-unicode", path: "/System/Library/Fonts/Supplemental/Arial Unicode.ttf", scripts: "pan-Unicode fallback", index: 0 },
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
                    println!("⚠️ フォント未検出（{}）: {}", cand.scripts, cand.path);
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

    /// egui にフォントを登録し、フォールバックチェーンを組み立てる。
    /// `self` を消費してバイト列をムーブする（大容量のため clone しない）
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
    fn report_mentions_loaded_scripts() {
        let mgr = FontManager {
            fonts: vec![(
                FontCandidate { id: "x", path: "/p", scripts: "Japanese", index: 0 },
                vec![0u8; 100],
            )],
            missing: vec!["y"],
        };
        let r = mgr.report();
        assert!(r.contains("Japanese"), "{r}");
        assert!(r.contains('y'), "{r}");
    }

    /// 実機のシステムフォントが読めること（macOSのみ）
    #[cfg(target_os = "macos")]
    #[test]
    fn loads_system_fonts_on_macos() {
        let mgr = FontManager::load();
        assert!(mgr.len() > 0, "no system fonts loaded");
        let scripts: Vec<String> = mgr.loaded().iter().map(|f| f.scripts.clone()).collect();
        assert!(
            scripts.iter().any(|s| s == "Japanese" || s == "pan-Unicode fallback"),
            "no CJK coverage: {scripts:?}"
        );
    }
}

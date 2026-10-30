//! 双方向テキストの表示整形
//!
//! epaint 0.27 のテキスト描画は1文字ずつ左から右へ配置するだけで、
//! アラビア語の字形結合も双方向並べ替えも行わない。
//! 論理順のまま渡すとアラビア語は分離・逆順に、ヘブライ語は逆順に表示される。
//! 描画直前に論理順→視覚順へ変換する（`get_display(reshape(text))` 相当）。
//!
//! 注意: 整形結果は表示専用。ログ・履歴・API 送信には論理順の原文を使うこと。

use std::borrow::Cow;
use std::sync::OnceLock;

use arabic_reshaper::ArabicReshaper;
use unicode_bidi::BidiInfo;

/// 結合字形の計算を共有する reshaper（設定パースを使い回す）
fn reshaper() -> &'static ArabicReshaper<'static> {
    static INSTANCE: OnceLock<ArabicReshaper<'static>> = OnceLock::new();
    INSTANCE.get_or_init(ArabicReshaper::new)
}

/// RTLスクリプト（アラビア語・ヘブライ語）の文字を含むか。
/// 含まないテキストは整形不要なので、そのまま返すための高速判定。
pub fn needs_shaping(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c,
        '\u{0590}'..='\u{05FF}'   // Hebrew
        | '\u{0600}'..='\u{06FF}' // Arabic
        | '\u{0750}'..='\u{077F}' // Arabic Supplement
        | '\u{08A0}'..='\u{08FF}' // Arabic Extended-A
        | '\u{FB50}'..='\u{FDFF}' // Arabic Presentation Forms-A
        | '\u{FE70}'..='\u{FEFF}' // Arabic Presentation Forms-B
        )
    })
}

/// 論理順テキストを表示用の視覚順に変換する。
/// RTL文字を含まない場合は入力をそのまま返す。
pub fn shape_for_display(text: &str) -> Cow<'_, str> {
    if !needs_shaping(text) {
        return Cow::Borrowed(text);
    }
    if !text.contains('\n') {
        return Cow::Owned(reorder_line(&reshaper().reshape(text)));
    }
    // 改行をまたいで合字しないよう、行ごとに整形する
    let mut out = String::with_capacity(text.len() + 16);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&reorder_line(&reshaper().reshape(line)));
    }
    Cow::Owned(out)
}

/// 結合形の1行を視覚順に並べ替える
fn reorder_line(reshaped_line: &str) -> String {
    let bidi = BidiInfo::new(reshaped_line, None);
    match bidi.paragraphs.first() {
        Some(para) => bidi.reorder_line(para, para.range.clone()).into_owned(),
        None => reshaped_line.to_owned(),
    }
}

/// 論理順の1行を整形する（結合＋視覚順化）
fn shape_logical_line(line: &str) -> String {
    reorder_line(&reshaper().reshape(line))
}

/// 段落を表示幅に収まる行に割り、行ごとに視覚順化する。
///
/// 視覚順化した全文を egui に渡すと、egui の LTR 折返しで行順が逆になる
/// （論理的先頭が最終行に落ちる）。論理順のまま単語単位で割り、
/// 各行を独立に整形することで正しい行順を保つ。
/// RTL 文字を含まない場合は入力をそのまま返す（egui の折返しに任せる）。
/// `max_width` が正でない場合は折返さず全文を1行として整形する。
pub fn shape_paragraph_for_display<'a>(
    text: &'a str,
    max_width: f32,
    measure: &dyn Fn(&str) -> f32,
) -> Cow<'a, str> {
    if !needs_shaping(text) {
        return Cow::Borrowed(text);
    }
    if max_width <= 0.0 {
        return Cow::Owned(shape_for_display(text).into_owned());
    }
    let mut out = String::with_capacity(text.len() + 16);
    for (i, para) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&wrap_paragraph(para, max_width, measure));
    }
    Cow::Owned(out)
}

/// 論理順の段落を貪欲に折り返し、各行を整形する。行順は論理順のまま。
/// 1単語で幅を超える場合ははみ出しを許して1行にする
fn wrap_paragraph(para: &str, max_width: f32, measure: &dyn Fn(&str) -> f32) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_shaped = String::new();
    for w in para.split_whitespace() {
        let cand = if current.is_empty() {
            w.to_string()
        } else {
            format!("{current} {w}")
        };
        let shaped = shape_logical_line(&cand);
        if current.is_empty() || measure(&shaped) <= max_width {
            current = cand;
            current_shaped = shaped;
        } else {
            lines.push(std::mem::take(&mut current_shaped));
            current = w.to_string();
            current_shaped = shape_logical_line(&current);
        }
    }
    if !current_shaped.is_empty() {
        lines.push(current_shaped);
    }
    lines.join("\n")
}

/// 字幕の表示上限の既定（文字数）。認識テキストは累積型のため、
/// そのまま出すと古い文が残り続ける。上限を超えたら末尾に丸める
pub const DEFAULT_TAIL_CHARS: usize = 160;
/// 表示上限の調整範囲（文字数）
pub const TAIL_CHARS_MIN: usize = 40;
pub const TAIL_CHARS_MAX: usize = 400;
/// 丸め後に残す最小文字数。下回る区切りは使わない
const MIN_TAIL_CHARS: usize = 20;

/// 25言語の文区切り。タイ語など句読点のない言語は文字数切りが担う
fn is_sentence_end(c: char) -> bool {
    matches!(
        c,
        '。' | '．' | '！' | '？' | '.' | '!' | '?' | '…' | '।' | '؟' | '\n'
    )
}

/// 字幕表示用に末尾へ丸める。収まればそのまま借りて返す。
/// 文区切りを優先し、なければ文字数で切る。
/// 区切り直後の閉じ括弧・空白は含めない
pub fn tail_for_display(text: &str, max_chars: usize) -> &str {
    let max_chars = max_chars.max(1);
    let total: usize = text.chars().count();
    if total <= max_chars {
        return text;
    }
    let skip = total - max_chars;
    let mut start = text.len();
    for (i, (b, _)) in text.char_indices().enumerate() {
        if i == skip {
            start = b;
            break;
        }
    }
    let mut best = start;
    for (b, c) in text[start..].char_indices() {
        if !is_sentence_end(c) {
            continue;
        }
        let mut after = start + b + c.len_utf8();
        // 閉じ括弧類は区切り側に付ける
        while let Some(next) = text[after..].chars().next() {
            if matches!(next, '」' | '』' | '）' | ')' | '"' | '”' | '’') {
                after += next.len_utf8();
            } else {
                break;
            }
        }
        if text[after..].chars().count() >= MIN_TAIL_CHARS {
            best = after;
            break;
        }
    }
    let tail = text[best..].trim_start();
    if tail.is_empty() { &text[start..] } else { tail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_short_text_unchanged() {
        assert_eq!(tail_for_display("", DEFAULT_TAIL_CHARS), "");
        assert_eq!(tail_for_display("こんにちは", DEFAULT_TAIL_CHARS), "こんにちは");
        let exactly: String = "あ".repeat(DEFAULT_TAIL_CHARS);
        assert_eq!(tail_for_display(&exactly, DEFAULT_TAIL_CHARS), exactly);
    }

    #[test]
    fn tail_snaps_to_sentence_boundary() {
        let text = format!("{}. {}", "x".repeat(100), "y".repeat(100));
        assert_eq!(tail_for_display(&text, DEFAULT_TAIL_CHARS), "y".repeat(100));
    }

    #[test]
    fn tail_japanese_and_danda() {
        let ja = format!("{}。{}", "あ".repeat(100), "い".repeat(100));
        assert_eq!(tail_for_display(&ja, DEFAULT_TAIL_CHARS), "い".repeat(100));
        let hi = format!("{}।{}", "अ".repeat(100), "आ".repeat(100));
        assert_eq!(tail_for_display(&hi, DEFAULT_TAIL_CHARS), "आ".repeat(100));
    }

    #[test]
    fn tail_short_remainder_uses_hard_cut() {
        let text = format!("{}.{}", "y".repeat(150), "z".repeat(10));
        let tail = tail_for_display(&text, DEFAULT_TAIL_CHARS);
        assert_eq!(tail.chars().count(), DEFAULT_TAIL_CHARS);
        assert!(tail.starts_with('y'));
    }

    #[test]
    fn tail_hard_cut_without_boundary() {
        let long = "a".repeat(200);
        let tail = tail_for_display(&long, DEFAULT_TAIL_CHARS);
        assert_eq!(tail.chars().count(), DEFAULT_TAIL_CHARS);
    }

    #[test]
    fn tail_custom_max_is_honored() {
        let long = "a".repeat(100);
        assert_eq!(tail_for_display(&long, 40).chars().count(), 40);
        assert_eq!(tail_for_display("abc", 0).chars().count(), 1);
    }

    #[test]
    fn latin_and_cjk_passthrough() {
        assert_eq!(shape_for_display("Hello, 世界！"), "Hello, 世界！");
        assert_eq!(shape_for_display(""), "");
    }

    #[test]
    fn arabic_word_is_shaped_and_reordered() {
        // سلام → 結合形 [seen-initial, lam-alef-ligature-final, meem-isolated] を視覚順へ
        assert_eq!(shape_for_display("سلام"), "\u{FEE1}\u{FEFC}\u{FEB3}");
    }

    #[test]
    fn language_name_arabic() {
        // 言語表示名「العربية」(alef, lam, ain, ra, beh, yeh, teh-marbuta) の整形結果。
        // alef・ra が右結合のみのため3区切りに分かれ、視覚順は反転する
        assert_eq!(
            shape_for_display("العربية"),
            "\u{FE94}\u{FEF4}\u{FE91}\u{FEAE}\u{FECC}\u{FEDF}\u{FE8D}"
        );
    }

    #[test]
    fn lam_alef_becomes_ligature() {
        assert_eq!(shape_for_display("لا"), "\u{FEFB}");
    }

    #[test]
    fn hebrew_is_reordered() {
        // עברית → 視覚順は文字反転
        assert_eq!(shape_for_display("עברית"), "תירבע");
    }

    #[test]
    fn mixed_ltr_base_keeps_latin_order() {
        // 'a' が先頭＝LTR基調。RTLランのみ視覚順化（単独behは孤立形のまま）
        assert_eq!(shape_for_display("a ب"), "a \u{FE8F}");
    }

    #[test]
    fn digits_stay_ltr_inside_rtl() {
        // RTL段落：behが右、欧字数字は左にまとまって正順
        assert_eq!(shape_for_display("ب 3"), "3 \u{FE8F}");
    }

    #[test]
    fn multiline_shaped_per_line() {
        assert_eq!(shape_for_display("a\nب"), "a\n\u{FE8F}");
    }

    /// テスト用の幅（1文字10pt換算）
    fn count10(s: &str) -> f32 {
        s.chars().count() as f32 * 10.0
    }

    #[test]
    fn ltr_paragraph_borrows_unchanged() {
        assert!(matches!(
            shape_paragraph_for_display("hello world foo bar", 5.0, &count10),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn short_rtl_stays_single_line() {
        assert_eq!(
            shape_paragraph_for_display("سلام", 1000.0, &count10),
            shape_for_display("سلام")
        );
    }

    #[test]
    fn long_rtl_wraps_in_logical_order() {
        // 2語ずつ収まる幅：行順が論理順のまま2行になる
        let line1 = shape_for_display("السلام عليكم");
        let width = count10(&line1) + 5.0;
        let shaped = shape_paragraph_for_display("السلام عليكم ورحمة الله", width, &count10);
        let expect = format!("{line1}\n{}", shape_for_display("ورحمة الله"));
        assert_eq!(shaped, expect);
    }

    #[test]
    fn explicit_breaks_and_zero_width() {
        let s = shape_paragraph_for_display("سلام\nلا", 1000.0, &count10);
        assert_eq!(s, format!("{}\n{}", shape_for_display("سلام"), shape_for_display("لا")));
        assert_eq!(
            shape_paragraph_for_display("سلام عليكم", 0.0, &count10),
            shape_for_display("سلام عليكم")
        );
    }
}

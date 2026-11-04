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
use std::time::Duration;

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

/// 字幕に表示する最大文数。認識テキストは累積型のため、
/// 文字数だけでは短い文が何文も残り続ける。文数でも絞る
pub const MAX_DISPLAY_SENTENCES: usize = 2;

/// 文末位置（文末記号の直後のバイト位置）を列挙する。
/// `.` `!` `?` は直後が空白か末尾のときだけ文末にする（小数・略語の誤爆避け）
fn sentence_end_positions(text: &str) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((b, c)) = chars.next() {
        let terminal = matches!(c, '。' | '．' | '！' | '？' | '؟' | '।' | '…' | '\n')
            || matches!(c, '.' | '!' | '?')
                && chars.peek().is_none_or(|(_, n)| n.is_whitespace());
        if terminal {
            ends.push(b + c.len_utf8());
        }
    }
    ends
}

/// 文末記号で文に割る。前後の空白を除き、空文は除く。
/// カバレッジ照合用。区切り規則は表示側と同一
pub fn split_sentences(text: &str) -> Vec<&str> {
    let ends = sentence_end_positions(text);
    let mut out = Vec::new();
    let mut start = 0;
    for e in ends {
        let s = text[start..e].trim();
        if !s.is_empty() {
            out.push(s);
        }
        start = e;
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// 末尾N文に絞る。文末記号のない言語はそのまま返す。
/// 末尾の未完文（文末記号なし）は1文に数える
pub fn tail_sentences(text: &str, max_sentences: usize) -> &str {
    let max_sentences = max_sentences.max(1);
    let ends = sentence_end_positions(text);
    let rest = &text[ends.last().copied().unwrap_or(0)..];
    let count = ends.len() + usize::from(!rest.trim().is_empty());
    if count <= max_sentences {
        return text;
    }
    let cut = ends
        .get(count - max_sentences - 1)
        .copied()
        .unwrap_or(0);
    let mut after = cut;
    // 閉じ括弧類は区切り側に付ける
    while let Some(next) = text[after..].chars().next() {
        if matches!(next, '」' | '』' | '）' | ')' | '"' | '”' | '’') {
            after += next.len_utf8();
        } else {
            break;
        }
    }
    let tail = text[after..].trim_start();
    if tail.is_empty() { text } else { tail }
}

/// 字幕表示用に末尾へ丸める。文字数切りと文数切り（末尾2文）の両方をかける。
/// 文区切りを優先し、なければ文字数で切る。
/// 区切り直後の閉じ括弧・空白は含めない
pub fn tail_for_display(text: &str, max_chars: usize) -> &str {
    let max_chars = max_chars.max(1);
    let total: usize = text.chars().count();
    if total <= max_chars {
        return tail_sentences(text, MAX_DISPLAY_SENTENCES);
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
    let tail = if tail.is_empty() { &text[start..] } else { tail };
    tail_sentences(tail, MAX_DISPLAY_SENTENCES)
}

/// 字幕の読み速度（1秒あたり文字数）。Netflix Timed Text Style Guide 成人番組の上限
/// - 欧文: 20字/秒（English (USA) ガイド）
/// - 日中韓: 4字/秒（Japanese ガイド、全角文字。1字の情報量が多いため低い）
const READING_CPS_LATIN: f64 = 20.0;
const READING_CPS_CJK: f64 = 4.0;
/// 文の最短表示。公開ガイドの最小値（Netflix 5/6秒、DCMP 40フレーム≒1.33秒、
/// FCC 最低1秒）に、発話の合間も文を残す余裕を足した4秒にした。
/// 2秒では合間が空くたび窓が真っ白になり、読みさしで消えて見えた
pub(crate) const SUBTITLE_MIN_DISPLAY: Duration = Duration::from_secs(4);
/// 文の最長表示。BBC ガイドの約6秒（Netflix 上限7秒の内側）。従来の固定値と同じ
pub(crate) const SUBTITLE_MAX_DISPLAY: Duration = Duration::from_secs(6);

/// CJK 文字（漢字・かな・ハングル）か
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30ff}' // ひらがな・カタカナ
        | '\u{3400}'..='\u{4dbf}' // CJK拡張A
        | '\u{4e00}'..='\u{9fff}' // CJK統合漢字
        | '\u{f900}'..='\u{faff}' // CJK互換漢字
        | '\u{20000}'..='\u{2ffff}' // 拡張B～F
        | '\u{ac00}'..='\u{d7a3}' // ハングル音節
        | '\u{1100}'..='\u{11ff}' | '\u{3130}'..='\u{318f}' // ハングル字母・互換字母
        | '\u{ff66}'..='\u{ff9d}' // 半角カタカナ
    )
}

/// 読める中身（文字・数字）を含むか。"..." や "…" だけの文は表示しない。
/// モデルの出力する ASCII の "..." は追補記号（U+2026）と紛らわしいため、
/// どの文字コードの点々も一律に落とす
pub(crate) fn has_words(sent: &str) -> bool {
    sent.chars().any(|c| c.is_alphanumeric())
}

/// 文の表示寿命。文字数÷読み速度を最短～最長で挟む。
/// CJK が半数超なら日中韓の速度、それ以外（欧文・アラビア語など）は欧文の速度で読む
pub(crate) fn sentence_lifetime(sent: &str) -> Duration {
    let total = sent.chars().count();
    if total == 0 {
        return SUBTITLE_MIN_DISPLAY;
    }
    let cjk = sent.chars().filter(|c| is_cjk(*c)).count();
    let cps = if cjk * 2 > total {
        READING_CPS_CJK
    } else {
        READING_CPS_LATIN
    };
    Duration::from_secs_f64((total as f64 / cps).clamp(
        SUBTITLE_MIN_DISPLAY.as_secs_f64(),
        SUBTITLE_MAX_DISPLAY.as_secs_f64(),
    ))
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
    fn tail_caps_accumulation_at_two_sentences() {
        // 160字に収まる5文でも末尾2文だけ残す
        let text = "Hello, I'm Lume. This is LumeSub. It's a real-time translation subtitle app. My words will turn into English. Watch.";
        assert_eq!(
            tail_for_display(text, DEFAULT_TAIL_CHARS),
            "My words will turn into English. Watch."
        );
    }

    #[test]
    fn tail_caps_japanese_and_keeps_partial() {
        let text = "こんにちは。私はルメです。これはルメサブです。";
        assert_eq!(
            tail_for_display(text, DEFAULT_TAIL_CHARS),
            "私はルメです。これはルメサブです。"
        );
        // 文末なしの途中片は1文に数える
        assert_eq!(
            tail_for_display("one. two. three …", DEFAULT_TAIL_CHARS),
            "two. three …"
        );
        assert_eq!(
            tail_for_display("one. two …", DEFAULT_TAIL_CHARS),
            "one. two …"
        );
    }

    #[test]
    fn tail_sentence_split_ignores_decimals() {
        // 小数点は文末にしない
        assert_eq!(
            tail_for_display("See 3.5kg. Buy it. Now.", DEFAULT_TAIL_CHARS),
            "Buy it. Now."
        );
    }

    #[test]
    fn tail_sentence_cut_keeps_closers() {
        assert_eq!(
            tail_for_display("あ。」い。」う。", DEFAULT_TAIL_CHARS),
            "い。」う。"
        );
    }

    #[test]
    fn split_sentences_cuts_at_terminals() {
        // 半角 ! は直後が空白・末尾のときだけ区切り（表示側と同一規則）
        assert_eq!(
            split_sentences("こんにちは!私はルメです。これはルメサブです。"),
            vec!["こんにちは!私はルメです。", "これはルメサブです。"]
        );
        assert_eq!(
            split_sentences("One. Two! Three?"),
            vec!["One.", "Two!", "Three?"]
        );
        // 小数点は割らない、末尾の未完文も拾う
        assert_eq!(split_sentences("See 3.5kg. Buy"), vec!["See 3.5kg.", "Buy"]);
        // 空白だけの区間は除く
        assert_eq!(split_sentences("One.  Two."), vec!["One.", "Two."]);
        assert!(split_sentences("   ").is_empty());
    }

    #[test]
    fn tail_without_sentence_ends_unchanged() {
        // 文末記号のない言語は文字数切りだけが効く
        assert_eq!(tail_for_display("まだ途中", DEFAULT_TAIL_CHARS), "まだ途中");
    }

    #[test]
    fn sentence_lifetime_scales_with_reading_speed() {
        use std::time::Duration;
        // 短文は最短表示に寄る
        assert_eq!(sentence_lifetime("はい。"), Duration::from_secs(4));
        assert_eq!(sentence_lifetime("Hi."), Duration::from_secs(4));
        // 欧文は20字/秒、日文は4字/秒で伸びる
        assert_eq!(
            sentence_lifetime(&"a".repeat(100)),
            Duration::from_millis(5000)
        );
        assert_eq!(
            sentence_lifetime(&"あ".repeat(20)),
            Duration::from_millis(5000)
        );
        // 長文は最長表示で頭打ち
        assert_eq!(sentence_lifetime(&"a".repeat(200)), Duration::from_secs(6));
        assert_eq!(sentence_lifetime(&"あ".repeat(40)), Duration::from_secs(6));
    }

    #[test]
    fn has_words_rejects_dots_only() {
        assert!(!has_words(""));
        assert!(!has_words("..."));
        assert!(!has_words("…"));
        assert!(!has_words("。。。"));
        assert!(!has_words("?!"));
        assert!(has_words("Hello..."));
        assert!(has_words("こんにちは"));
        assert!(has_words("مرحبا"));
        assert!(has_words("123"));
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

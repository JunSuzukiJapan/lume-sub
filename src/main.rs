mod asr;
mod audio;
mod control;
mod devlog;
mod fonts;
mod geometry;
mod hover;
mod i18n;
mod language;
mod screen;
mod store;
mod text;
mod translate;
mod tune;

use eframe::egui;
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use asr::{ASR_MODEL_DEFAULT, AsrConfig, AsrEvent};
use control::{CaptureState, PipelineController, SpawnContext, buttons_enabled};
use i18n::{UiLocale, UiStrings};
use language::{Language, Script, SharedLanguageConfig, SourceLanguage, shared};
use screen::DiagLog;
use store::{ApiConfig, SharedStore, Store};
use text::shape_for_display;
use translate::{TRANSLATE_MODEL_DEFAULT, TranslationSession};

/// 字幕1件分の表示要求。古い文の遅延到着を捨てるための順序情報を持つ
#[derive(Debug, Clone)]
pub(crate) struct SubtitleUpdate {
    pub text: String,
    pub turn: i32,
    pub seq: u64,
    pub generation: u64,
    pub spawned_at: Instant,
}

impl SubtitleUpdate {
    pub fn new(text: String, turn: i32, seq: u64, generation: u64) -> Self {
        Self {
            text,
            turn,
            seq,
            generation,
            spawned_at: Instant::now(),
        }
    }

    pub fn at(text: String, turn: i32, seq: u64, generation: u64, spawned_at: Instant) -> Self {
        Self {
            text,
            turn,
            seq,
            generation,
            spawned_at,
        }
    }
}

/// 字幕窓ごとの表示ガード。1文ごとにいつ表示したかを覚え、古い文は捨てる
#[derive(Debug, Clone, Default)]
struct DisplayGuard {
    last: Option<(u64, i32, u64, Instant)>,
}

impl DisplayGuard {
    fn new() -> Self {
        Self { last: None }
    }

    /// 世代切替（言語変更・新セッション）で古い記憶を捨てる
    fn reset(&mut self, generation: u64) {
        self.last = Some((generation, i32::MIN, 0, Instant::now()));
    }

    /// 新しければ真（採用）。古い世代・古いターン・同ターンの古い順序は偽（破棄）。
    /// ストリーミングの追補（同一トリガーの同順序・新時刻）は受け付ける
    fn accept(&mut self, update: &SubtitleUpdate) -> bool {
        if let Some((last_gen, turn, seq, at)) = self.last {
            if update.generation < last_gen {
                return false;
            }
            if update.generation == last_gen {
                if update.turn < turn {
                    return false;
                }
                if update.turn == turn
                    && (update.seq < seq || (update.seq == seq && update.spawned_at < at))
                {
                    return false;
                }
            }
        }
        self.last = Some((
            update.generation,
            update.turn,
            update.seq,
            update.spawned_at,
        ));
        true
    }
}

/// 途中翻訳の追補記号。文末にこれが付いていれば未確定の追補、なければ確定文
const STREAMING_MARKER: char = '…';

/// 追補（未確定）の文言か。送信側は確定文に記号を付けずに送る規約
fn is_streaming_text(text: &str) -> bool {
    text.ends_with(STREAMING_MARKER)
}

/// 追補の表示文。まだ1語もなければ送らない（中身のない "…" を出さない）
fn progress_text(acc: &str) -> Option<String> {
    if acc.trim().is_empty() {
        None
    } else {
        Some(format!("{acc} …"))
    }
}

/// 翻訳窓の表示バッファ。直近3ターン分を保持する
///
/// 新ターンの開始で旧訳を消さず、旧ターンの確定訳が遅れて届いても
/// 2つ前まで救済する。表示は確定文を優先して2行に選ぶ。
/// 文の取りこぼしを防ぐための救済つき順序管理
#[derive(Debug, Clone, Default)]
struct TranslationTail {
    old: Option<SubtitleUpdate>,
    prev: Option<SubtitleUpdate>,
    cur: Option<SubtitleUpdate>,
    ledger: SentenceLedger,
}

impl TranslationTail {
    fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    /// 同一枠内の新旧判定。seq が新しければ採用、同 seq は時刻で判定する
    fn slot_accept(slot: &Option<SubtitleUpdate>, update: &SubtitleUpdate) -> bool {
        match slot {
            None => true,
            Some(last) => {
                if update.seq > last.seq {
                    // 追補は表示文より短くならない範囲でのみ採用する。
                    // 確定要求など新要求の出始め（短い追補）が古い長い追補を
                    // 消して「長い→短い→長い」と瞬くのを防ぐ。確定文はつねに採用
                    !is_streaming_text(&update.text)
                        || update.text.chars().count() >= last.text.chars().count()
                } else {
                    update.seq == last.seq && update.spawned_at >= last.spawned_at
                }
            }
        }
    }

    /// 順序つき翻訳を受け付ける。採用したら真
    /// - 新しいターン: 旧枠へずらして採用
    /// - 現行・前・前々ターン: 新しければ採用（2つ前までの遅延確定の救済）
    /// - それより古いターン: 破棄
    fn push(&mut self, update: &SubtitleUpdate) -> bool {
        if self.ledger.is_new_generation(update.generation) {
            self.old = None;
            self.prev = None;
            self.cur = None;
        }
        let accepted = match self.cur.as_ref().map(|u| u.turn) {
            None => {
                self.cur = Some(update.clone());
                true
            }
            Some(t) if update.turn > t => {
                self.old = self.prev.take();
                self.prev = self.cur.take();
                self.cur = Some(update.clone());
                true
            }
            Some(t) if update.turn == t => {
                // 確定文を持つ枠に同じターン番号で別内容の確定文が来たら、
                // プロバイダが turnId を進めなかった別発話とみなして新枠へ
                // ずらす（上書きすると前文が寿命前に消える）。確定はターン内
                // で終端のため、正規の確定→確定の別文置換は存在しない。
                // 同文の再送は上書き（期限延長）のままにする
                let (diff_completed, seq_newer) = match self.cur.as_ref() {
                    Some(cur) => {
                        let diff = !is_streaming_text(&cur.text)
                            && !is_streaming_text(&update.text)
                            && cur.text != update.text;
                        (diff, update.seq >= cur.seq)
                    }
                    None => (false, false),
                };
                if diff_completed && seq_newer {
                    self.old = self.prev.take();
                    self.prev = self.cur.take();
                    self.cur = Some(update.clone());
                    true
                } else if diff_completed {
                    false
                } else if Self::slot_accept(&self.cur, update) {
                    self.cur = Some(update.clone());
                    true
                } else {
                    false
                }
            }
            // 現行より古い。同ターンの枠があれば救済、それ以外は破棄
            Some(_) => {
                let prev_turn = self.prev.as_ref().map(|u| u.turn);
                let old_turn = self.old.as_ref().map(|u| u.turn);
                if prev_turn.is_none()
                    || (prev_turn == Some(update.turn)
                        && Self::slot_accept(&self.prev, update))
                {
                    self.prev = Some(update.clone());
                    true
                } else if old_turn == Some(update.turn) && Self::slot_accept(&self.old, update) {
                    self.old = Some(update.clone());
                    true
                } else {
                    false
                }
            }
        };
        if accepted {
            self.ledger
                .note(update.generation, &update.text, update.spawned_at);
        }
        accepted
    }

    /// 枠文に寿命内の文が残っているか
    fn text_is_fresh(&self, text: &str, now: Instant) -> bool {
        text::split_sentences(text)
            .iter()
            .any(|s| self.ledger.is_fresh(s, now))
    }

    /// 寿命内・中身ありの枠文を古い順に
    fn fresh_texts(&self, now: Instant) -> Vec<&str> {
        [&self.old, &self.prev, &self.cur]
            .into_iter()
            .flatten()
            .map(|u| u.text.as_str())
            .filter(|t| !t.is_empty())
            .filter(|t| text::has_words(t))
            .filter(|t| self.text_is_fresh(t, now))
            .collect()
    }

    /// 確定文だけの表示文（上段用）。新しい2枠分。追補は含めない
    fn render_finalized(&self, now: Instant) -> String {
        let completed: Vec<&str> = self
            .fresh_texts(now)
            .into_iter()
            .filter(|t| !is_streaming_text(t))
            .collect();
        let start = completed.len().saturating_sub(2);
        self.ledger
            .filter_fresh(&completed[start..].join("\n"), now)
    }

    /// 追補の表示文（下段用）。最新枠が追補のときだけ出す
    fn render_streaming(&self, now: Instant) -> String {
        match self.fresh_texts(now).last() {
            Some(t) if is_streaming_text(t) => self.ledger.filter_fresh(t, now),
            _ => String::new(),
        }
    }
}

/// 言語設定の世代。UIで言語を変えるたびに進み、旧世代の翻訳結果を捨てる
type SharedGeneration = Arc<AtomicU64>;

/// ASRターンの追跡。開始・確定の到着が前後しても単調に進み、戻らない
#[derive(Debug, Clone, Default)]
struct TurnTracker {
    current: i32,
}

impl TurnTracker {
    fn new() -> Self {
        Self { current: 0 }
    }

    /// 新ターンの開始。実際に進んだら真（窓の消去はそのときだけ行う）
    fn note_speech_start(&mut self, turn_id: i32) -> bool {
        if turn_id > self.current {
            self.current = turn_id;
            true
        } else {
            false
        }
    }

    /// ターン確定。開始が欠けても追いつく。古い確定では戻らない
    fn note_complete(&mut self, turn_id: i32) {
        self.current = self.current.max(turn_id);
    }

    fn current(&self) -> i32 {
        self.current
    }

    fn reset(&mut self) {
        self.current = 0;
    }
}

/// ターンごとの観測集計。開発ログに1行サマリを出すための記録。
/// 表示の可否はGUI側の管轄のため、ここでは要求・完了までを数える
/// （表示・破棄の突き合わせは --coverage で行う）
#[derive(Debug, Clone, Default)]
struct TurnStats {
    start_at: Option<Instant>,
    first_partial_at: Option<Instant>,
    partials: u32,
    complete: bool,
    xlate_spawned: u32,
    xlate_ok: u32,
    xlate_err: u32,
}

impl TurnStats {
    /// 開発ログ用の1行サマリ。在空分は「残」に数える
    fn summary(&self, turn: i32) -> String {
        let first = match (self.start_at, self.first_partial_at) {
            (Some(s), Some(f)) => format!("{}ms", f.duration_since(s).as_millis()),
            _ => "-".to_string(),
        };
        let pending = self
            .xlate_spawned
            .saturating_sub(self.xlate_ok + self.xlate_err);
        format!(
            "📊 ターン#{turn}: partials={} 初回{} 確定{} 翻訳要求{}(完了{}/失敗{}/残{})",
            self.partials,
            first,
            if self.complete { "あり" } else { "なし" },
            self.xlate_spawned,
            self.xlate_ok,
            self.xlate_err,
            pending,
        )
    }
}

/// 発話区間の認識見張り。音声区間に対応する認識があったかを残す
#[derive(Debug, Clone, Default)]
struct UtteranceWatch {
    start_ms: u128,
    recognized: bool,
    complete: bool,
    turns: Vec<i32>,
}

impl UtteranceWatch {
    fn mark_partial(&mut self, turn: i32) {
        self.recognized = true;
        if !self.turns.contains(&turn) {
            self.turns.push(turn);
        }
    }

    fn mark_complete(&mut self, turn: i32) {
        self.mark_partial(turn);
        self.complete = true;
    }

    /// 人間向けの判定行
    fn verdict(&self, end_ms: u128) -> String {
        let start = self.start_ms as f64 / 1000.0;
        let end = end_ms as f64 / 1000.0;
        if self.recognized {
            let turns = self
                .turns
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "🔍 音声 {start:.1}–{end:.1}s: 認識あり (turn={turns} 確定{})",
                if self.complete { "あり" } else { "なし" },
            )
        } else {
            format!("🔍 音声 {start:.1}–{end:.1}s: 認識なし")
        }
    }
}

/// 集計の吐き出し。upto 未満のターンをサマリ出力して捨てる
fn flush_turn_stats(map: &mut BTreeMap<i32, TurnStats>, upto: i32) {
    let old: Vec<i32> = map.keys().copied().filter(|t| *t < upto).collect();
    for t in old {
        if let Some(s) = map.remove(&t) {
            dev_log!("{}", s.summary(t));
        }
    }
}

pub(crate) enum AppEvent {
    /// 即時表示（待機文・音声状態など順序管理の外）
    UpdateSource(String),
    /// 順序つき原文。古いターンの遅延は捨てる
    UpdateSourceSeq(SubtitleUpdate),
    /// 即時表示（新セッションの消去など）
    UpdateTranslation(String),
    /// 順序つき翻訳。古い文の遅延は捨てる
    UpdateTranslationSeq(SubtitleUpdate),
}

/// 高速モードの途中翻訳の間引き間隔。呼び出し回数を抑える
const PARTIAL_TRANSLATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// 低速モードの追補翻訳を投げる文字数の伸び
const PREVIEW_GROWTH_CHARS: usize = 10;
/// 冒頭訳を投げる最小文字数。1字の断片訳を避ける
const LEADING_MIN_CHARS: usize = 2;
/// 文ごとの寿命管理。文の長さに応じた寿命が過ぎた文を落とす。
/// 世代が変わったら中身を捨てる。原文窓と翻訳窓で共用する
#[derive(Debug, Clone, Default)]
struct SentenceLedger {
    generation: Option<u64>,
    /// 文とその表示期限。追補で同じ文が来たら期限を延ばす
    seen: Vec<(String, Instant)>,
    latest: Option<Instant>,
}

impl SentenceLedger {
    fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    /// 未見の世代か
    fn is_new_generation(&self, generation: u64) -> bool {
        self.generation != Some(generation)
    }

    /// 文ごとの表示期限を記録する。期限切れの古い文は捨てる
    fn note(&mut self, generation: u64, text: &str, at: Instant) {
        if self.is_new_generation(generation) {
            self.generation = Some(generation);
            self.seen.clear();
            self.latest = None;
        }
        self.latest = self.latest.max(Some(at));
        for sent in text::split_sentences(text) {
            self.seen.retain(|(s, _)| s != sent);
            self.seen
                .push((sent.to_string(), at + text::sentence_lifetime(sent)));
        }
        if let Some(latest) = self.latest {
            self.seen.retain(|(_, exp)| *exp > latest);
        }
    }

    /// 寿命内か。台帳にない文は掃除済み＝寿命切れのため落とす
    fn is_fresh(&self, sent: &str, now: Instant) -> bool {
        match self.seen.iter().find(|(s, _)| s == sent) {
            Some((_, exp)) => now < *exp,
            None => false,
        }
    }

    /// 寿命切れ・中身なしの文を落とした表示文。1文1行でつなぐ
    fn filter_fresh(&self, joined: &str, now: Instant) -> String {
        if joined.is_empty() {
            return String::new();
        }
        text::split_sentences(joined)
            .into_iter()
            .filter(|s| text::has_words(s) && self.is_fresh(s, now))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 高速モードの途中訳の追跡。間引き判定と確定時の使い回しを持つ。
/// 時刻は引数で受けてテスト可能にする
struct PartialTracker {
    last_at: Option<Instant>,
    last_text: String,
    last_translation: String,
    /// 翻訳済みテキスト内の文末記号数。ターン確定で0に戻る
    done_marks: usize,
    /// 今ターンで途中翻訳を投げたか。ターン確定で false に戻る
    turn_previewed: bool,
    /// 前回送信時の文字数。ターン確定で0に戻る
    spawned_chars: usize,
}

/// 文末記号の数。`.!?` は直後が空白か末尾のときだけ数える（小数・略語の誤爆避け）
fn sentence_marks(text: &str) -> usize {
    let mut count = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let terminal = matches!(c, '。' | '．' | '！' | '？' | '؟' | '।')
            || matches!(c, '.' | '!' | '?')
                && chars.peek().is_none_or(|n| n.is_whitespace());
        if terminal {
            count += 1;
        }
    }
    count
}

impl PartialTracker {
    fn new() -> Self {
        Self {
            last_at: None,
            last_text: String::new(),
            last_translation: String::new(),
            done_marks: 0,
            turn_previewed: false,
            spawned_chars: 0,
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
        self.note_spawned(text.clone(), now);
        self.note_completed(text, translation);
    }

    /// 翻訳要求の送信時点。間引きはここで効かせ、使い回しは完了まで待つ
    fn note_spawned(&mut self, text: String, now: Instant) {
        self.last_at = Some(now);
        self.done_marks = sentence_marks(&text);
        self.spawned_chars = text.chars().count();
        self.last_text = text;
        self.last_translation.clear();
        self.turn_previewed = true;
    }

    /// 翻訳結果の到着。最新の送信と一致したときだけ使い回し用に覚える
    fn note_completed(&mut self, text: String, translation: String) {
        if text == self.last_text {
            self.last_translation = translation;
        }
    }

    /// 途中文に未翻訳の文末があれば真。モード・間引きによらず即時翻訳する
    fn sentence_ready(&self, text: &str) -> bool {
        sentence_marks(text) > self.done_marks
    }

    /// ターン確定。次ターンの文末カウントを0から始め、冒頭訳に備える
    fn note_turn_end(&mut self) {
        self.done_marks = 0;
        self.turn_previewed = false;
        self.spawned_chars = 0;
    }

    /// 今ターンまだ途中翻訳を投げていない文言なら真。3字以上で即訳して
    /// 翻訳窓の空白を短くする（低速モードでも1回だけ投げる）
    /// 実動作は [`Self::preview_trigger`] 経由のためテスト専用
    #[cfg(test)]
    fn leading_ready(&self, text: &str) -> bool {
        self.leading_ready_cfg(text, LEADING_MIN_CHARS)
    }

    /// 閾値指定つきの冒頭判定。記録再生（tune）で閾値を振るときに使う
    fn leading_ready_cfg(&self, text: &str, leading_min: usize) -> bool {
        !self.turn_previewed && text.chars().count() >= leading_min
    }

    /// 前回送信から一定文字数以上増えたら真。句読点のない途中文でも
    /// 翻訳を止めないための低速モード用
    /// 実動作は [`Self::preview_trigger`] 経由のためテスト専用
    #[cfg(test)]
    fn growth_ready(&self, text: &str) -> bool {
        self.growth_ready_cfg(text, PREVIEW_GROWTH_CHARS)
    }

    /// 閾値指定つきの追補判定。記録再生（tune）で閾値を振るときに使う
    fn growth_ready_cfg(&self, text: &str, growth_chars: usize) -> bool {
        text != self.last_text
            && text.chars().count() >= self.spawned_chars.saturating_add(growth_chars)
    }

    /// 途中翻訳の発火判定。文末→高速間引き→冒頭→低速追補の順
    fn preview_trigger(&self, text: &str, fast: bool, now: Instant) -> Option<&'static str> {
        self.preview_trigger_cfg(text, fast, now, PREVIEW_GROWTH_CHARS, LEADING_MIN_CHARS)
    }

    /// 閾値指定つきの発火判定。記録再生（tune）で閾値を振るときに使う
    fn preview_trigger_cfg(
        &self,
        text: &str,
        fast: bool,
        now: Instant,
        growth_chars: usize,
        leading_min: usize,
    ) -> Option<&'static str> {
        if self.sentence_ready(text) {
            Some("文末")
        } else if fast && self.should_translate(text, now) {
            Some("途中")
        } else if self.leading_ready_cfg(text, leading_min) {
            Some("冒頭")
        } else if !fast && self.growth_ready_cfg(text, growth_chars) {
            Some("追補")
        } else {
            None
        }
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

/// 終了時の確定保存用に窓幾何を選ぶ。ドラッグ中（実窓が古い）は
/// None にしてメモリ側のドラッグ値を据え置く
fn exit_geometry_to_persist(
    dirty: bool,
    last: Option<geometry::WindowGeometry>,
) -> Option<geometry::WindowGeometry> {
    if dirty { None } else { last }
}

/// 字幕窓の非表示要求。両方隠しは拒否してログを出す
fn hide_subtitle(store: &SharedStore, hide_source: bool) {
    let mut s = store.lock().expect("store lock poisoned");
    let (source, translation) = if hide_source {
        (false, s.show_translation())
    } else {
        (s.show_source(), false)
    };
    if !s.set_shows(source, translation) {
        dev_log!("⚠️ 両方の字幕を非表示にはできません");
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
                dev_log!("🖥️ 追従: 移動します {outer:?} → {pos:?}（メイン画面 {main:?}）");
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

/// 字幕1区画分の表示指定。翻訳窓は確定（上段）＋追補（下段）の2区画
struct PaintPart<'a> {
    text: &'a str,
    cache: &'a mut (String, u32, String, String),
    color: egui::Color32,
    strong: bool,
}

/// 整形済み字幕1区画を描く
fn paint_shaped_text(
    ui: &mut egui::Ui,
    text: &str,
    shaped_cache: &mut (String, u32, String, String),
    font: egui::FontId,
    color: egui::Color32,
    strong: bool,
) {
    // RTL は論理順のまま実フォント幅で割り、行ごとに整形する（行順保持）
    let max_w = ui.available_width() - 4.0;
    let width_key = (max_w * 2.0).round().max(0.0) as u32;
    let font_key = font_cache_key(&font);
    let visual =
        if shaped_cache.0 == text && shaped_cache.1 == width_key && shaped_cache.2 == font_key {
            shaped_cache.3.clone()
        } else {
            let shaped = ui.fonts(|fonts| {
                let measure = |s: &str| {
                    fonts
                        .layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::WHITE)
                        .size()
                        .x
                };
                text::shape_paragraph_for_display(text, max_w, &measure).into_owned()
            });
            *shaped_cache = (text.to_string(), width_key, font_key, shaped.clone());
            shaped
        };
    let rich = egui::RichText::new(visual).font(font).color(color);
    let rich = if strong { rich.strong() } else { rich };
    ui.add(egui::Label::new(rich).wrap(true));
}

/// 字幕1窓分を描く。バツが押されたら true を返す。
/// もう片方の窓が非表示中のときは `close_enabled` を false にしてバツを無効化する
#[allow(clippy::too_many_arguments)]
fn paint_subtitle_window(
    ui: &mut egui::Ui,
    parts: &mut [PaintPart<'_>],
    font: egui::FontId,
    interactive: bool,
    drag_hint: &str,
    tag: &str,
    close_enabled: bool,
) -> bool {
    ui.vertical_centered(|ui| {
        ui.add_space(20.0);
        let mut shown = 0;
        for part in parts.iter_mut() {
            if part.text.is_empty() {
                continue;
            }
            if shown > 0 {
                ui.separator();
            }
            shown += 1;
            paint_shaped_text(
                ui,
                part.text,
                part.cache,
                font.clone(),
                part.color,
                part.strong,
            );
        }
    });
    if interactive {
        hover::paint_frame(ui, drag_hint, tag, close_enabled)
    } else {
        false
    }
}

/// 整形キャッシュ用の字体キー。字体・サイズが変われば再整形する
fn font_cache_key(font: &egui::FontId) -> String {
    let name = match &font.family {
        egui::FontFamily::Proportional => "p".to_string(),
        egui::FontFamily::Monospace => "m".to_string(),
        egui::FontFamily::Name(n) => n.to_string(),
    };
    format!("{name}|{}", font.size)
}

/// フォント選択肢の表示名。空文字＝既定
fn font_choice_label(loaded: &[(String, String)], id: &str, default_label: &str) -> String {
    if id.is_empty() {
        default_label.to_string()
    } else {
        loaded
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, s)| s.clone())
            .unwrap_or_else(|| id.to_string())
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
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "--tune") {
        tune::run(&args[2..]);
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--coverage") {
        tune::coverage(&args[2..]);
        return Ok(());
    }
    // rustls の暗号プロバイダを明示的に選択する。
    // 依存の features だけでは自動判定できず TLS 初回使用時に panic するため
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 計測ログの時刻原点。パイプラインとUIで共有する
    let t0 = Instant::now();

    let store = store::shared(Store::load());
    devlog::set_enabled(store.lock().expect("store lock poisoned").dev_logs());
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
    dev_log!(
        "🗝️ API Key: {}",
        if key_saved {
            "保存済み設定を使用"
        } else if api_key.is_some() {
            "環境変数を使用"
        } else {
            "未設定（ASR・翻訳なし）"
        }
    );
    dev_log!("🌐 API Base URL: {base_url}");
    dev_log!(
        "🗣️ 表示言語: {} ({})",
        ui_locale.display_name(),
        ui_locale.code()
    );
    dev_log!(
        "🖥️ メイン画面追従: {}",
        if follow_main { "ON" } else { "OFF" }
    );
    if saved.is_some() {
        dev_log!("🪟 字幕位置を復元します");
    }
    if saved_control.is_some() {
        dev_log!("🪟 コントロール位置を復元します");
    }
    // 保存済みキーがなければ初回ダイアログを出す。入力欄には有効値を入れる
    let settings_open = !key_saved;
    let settings_draft = effective;
    let lang_config = store.lock().expect("store lock poisoned").language_config();
    dev_log!("🈯 言語: {}", lang_config.describe());
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
            dev_log!("🔌 ASR接続先: {ws_url}（model={asr_model}）");
            Some(AsrConfig {
                ws_url,
                api_key: key,
                model: asr_model,
            })
        }
        (Some(_), Err(e)) => {
            dev_log!("⚠️ {e}（ASR・翻訳なし）");
            None
        }
        (None, _) => {
            dev_log!("⚠️ APIキー未設定のためASR・翻訳は無効です（レベル表示のみ）");
            None
        }
    };
    let asr_enabled = asr_config.is_some();

    // 言語設定の世代。UIで変えるたびに進む
    let generation: SharedGeneration = Arc::new(AtomicU64::new(0));
    // ASRイベント → 字幕は即時、翻訳は並列に投げて遅延到着は捨てる
    if let Some(cfg) = asr_config.clone() {
        let gui_tx2 = gui_tx.clone();
        let client = reqwest::Client::new();
        let langs = languages.clone();
        let task_store = store.clone();
        let base = base_url.clone();
        let model = translate_model.clone();
        let task_generation = generation.clone();
        let task_t0 = t0;
        rt.spawn(async move {
            let session = Arc::new(Mutex::new(TranslationSession::default()));
            let partials = Arc::new(Mutex::new(PartialTracker::new()));
            let stats = Arc::new(Mutex::new(BTreeMap::<i32, TurnStats>::new()));
            let mut utterance: Option<UtteranceWatch> = None;
            let mut turns = TurnTracker::new();
            let mut next_seq: u64 = 0;
            let mut local_gen = task_generation.load(Ordering::Relaxed);
            while let Some(event) = asr_rx.recv().await {
                // 言語が変わったら途中状態を捨てて新世代で始める
                let cur_gen = task_generation.load(Ordering::Relaxed);
                if cur_gen != local_gen {
                    local_gen = cur_gen;
                    if let Ok(mut p) = partials.lock() {
                        p.reset();
                    }
                    if let Ok(mut s) = session.lock() {
                        s.reset();
                    }
                    if let Ok(mut m) = stats.lock() {
                        flush_turn_stats(&mut m, i32::MAX);
                    }
                    turns.reset();
                }
                match event {
                    AsrEvent::Session(id) => {
                        dev_log!(
                            "⏱ {}",
                            serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "session"})
                        );
                        // 新セッションはターン番号が振り直されるため世代を進める
                        local_gen = task_generation.fetch_add(1, Ordering::Relaxed) + 1;
                        if let Ok(mut s) = session.lock() {
                            s.reset();
                        }
                        dev_log!("✅ ASRセッション開始: {id}");
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
                        if let Ok(mut p) = partials.lock() {
                            p.reset();
                        }
                        if let Ok(mut m) = stats.lock() {
                            flush_turn_stats(&mut m, i32::MAX);
                        }
                        turns.reset();
                    }
                    AsrEvent::SpeechStart(turn_id) => {
                        dev_log!(
                            "⏱ {}",
                            serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "start", "turn": turn_id})
                        );
                        // 新しい発話が始まったら旧文を即破棄する。遅延した古い
                        // 開始では消さない（有効な新文を隠さないため）
                        if turns.note_speech_start(turn_id) {
                            dev_log!("🎙️ 発話開始#{turn_id}（旧文を破棄）");
                            // 新ターンは文末カウントを0から始める
                            if let Ok(mut p) = partials.lock() {
                                p.note_turn_end();
                            }
                            if let Ok(mut m) = stats.lock() {
                                flush_turn_stats(&mut m, turn_id);
                                m.entry(turn_id).or_default().start_at = Some(Instant::now());
                            }
                            let seq = next_seq;
                            next_seq += 1;
                            let _ = gui_tx2.try_send(AppEvent::UpdateSourceSeq(
                                SubtitleUpdate::new(String::new(), turn_id, seq, local_gen),
                            ));
                            // 翻訳窓は消さない。旧ターンの確定訳が遅れて届いても
                            // TranslationTail が前ターン枠に救済し、新訳が来るまで旧訳を残す
                        }
                    }
                    AsrEvent::Partial(text) => {
                        let seq = next_seq;
                        next_seq += 1;
                        let turn = turns.current();
                        dev_log!(
                            "⏱ {}",
                            serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "partial", "seq": seq, "text": text})
                        );
                        if let Some(u) = utterance.as_mut() {
                            u.mark_partial(turn);
                        }
                        if let Ok(mut m) = stats.lock() {
                            let entry = m.entry(turn).or_default();
                            entry.partials += 1;
                            if entry.first_partial_at.is_none() {
                                entry.first_partial_at = Some(Instant::now());
                            }
                        }
                        let _ = gui_tx2.try_send(AppEvent::UpdateSourceSeq(
                            SubtitleUpdate::new(format!("{text} …"), turn, seq, local_gen),
                        ));
                        // 文末が出たら両モードで即時翻訳。高速モードは文の途中も間引いて訳す。
                        // 低速モードは冒頭1回＋文字数の伸びで追補する
                        let fast = task_store
                            .lock()
                            .map(|s| s.fast_translation())
                            .unwrap_or(false);
                        let trigger = partials
                            .lock()
                            .map(|p| p.preview_trigger(&text, fast, Instant::now()));
                        let trigger = trigger.unwrap_or(None);
                        if let Some(kind) = trigger {
                            dev_log!(
                                "⏱ {}",
                                serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "trigger", "kind": kind, "turn": turn, "seq": seq, "chars": text.chars().count()})
                            );
                            let pair =
                                langs.lock().map(|c| c.clone()).unwrap_or_default();
                            if pair.needs_translation() {
                                let body = session.lock().ok().map(|mut s| {
                                    s.build_request_json(
                                        &model,
                                        &pair.source,
                                        &pair.target,
                                        &text,
                                    )
                                });
                                if let Some(body) = body {
                                    if let Ok(mut p) = partials.lock() {
                                        p.note_spawned(text.clone(), Instant::now());
                                    }
                                    if let Ok(mut m) = stats.lock() {
                                        m.entry(turn).or_default().xlate_spawned += 1;
                                    }
                                    let task_client = client.clone();
                                    let task_base = base.clone();
                                    let task_key = cfg.api_key.clone();
                                    let task_gui = gui_tx2.clone();
                                    let task_partials = partials.clone();
                                    let task_stats = stats.clone();
                                    let task_gen = task_generation.clone();
                                    let kind = kind.to_string();
                                    tokio::spawn(async move {
                                        let progress_gui = task_gui.clone();
                                        let progress_gen = task_gen.clone();
                                        let req_start = Instant::now();
                                        let mut first_ms: Option<u128> = None;
                                        match TranslationSession::post_chat_stream(
                                            &task_client,
                                            &task_base,
                                            &task_key,
                                            body,
                                            |acc, _final| {
                                                if first_ms.is_none() {
                                                    first_ms =
                                                        Some(req_start.elapsed().as_millis());
                                                }
                                                // 旧世代の追補は捨てる
                                                if progress_gen.load(Ordering::Relaxed) != local_gen
                                                {
                                                    return;
                                                }
                                                if let Some(text) = progress_text(acc) {
                                                    let _ = progress_gui.try_send(
                                                        AppEvent::UpdateTranslationSeq(
                                                            SubtitleUpdate::at(
                                                                text,
                                                                turn,
                                                                seq,
                                                                local_gen,
                                                                Instant::now(),
                                                            ),
                                                        ),
                                                    );
                                                }
                                            },
                                        )
                                        .await
                                        {
                                            Ok(tr) => {
                                                dev_log!(
                                                    "⏱ {}",
                                                    serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "xlate", "turn": turn, "seq": seq, "first_ms": first_ms.unwrap_or(req_start.elapsed().as_millis()), "total_ms": req_start.elapsed().as_millis(), "out": tr.chars().count()})
                                                );
                                                // 旧世代の結果は捨てる
                                                if task_gen.load(Ordering::Relaxed) != local_gen {
                                                    return;
                                                }
                                                dev_log!("🌐 {kind}訳: {tr}");
                                                if let Ok(mut p) = task_partials.lock() {
                                                    p.note_completed(text, tr.clone());
                                                }
                                                if let Ok(mut m) = task_stats.lock()
                                                    && let Some(s) = m.get_mut(&turn)
                                                {
                                                    s.xlate_ok += 1;
                                                }
                                                // 最終文は追補で表示済みのため送らない
                                            }
                                            Err(e) => {
                                                dev_log!(
                                                    "⏱ {}",
                                                    serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "xlate_err", "turn": turn, "seq": seq})
                                                );
                                                dev_log!("⚠️ 途中翻訳失敗: {e}");
                                                if let Ok(mut m) = task_stats.lock()
                                                    && let Some(s) = m.get_mut(&turn)
                                                {
                                                    s.xlate_err += 1;
                                                }
                                            }
                                        }
                                    });
                                }
                            }
                        }
                    }
                    AsrEvent::Complete { turn_id, transcript } => {
                        let seq = next_seq;
                        next_seq += 1;
                        dev_log!(
                            "⏱ {}",
                            serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "complete", "turn": turn_id, "seq": seq, "text": transcript})
                        );
                        dev_log!("📝 確定#{turn_id}: {transcript}");
                        if let Some(u) = utterance.as_mut() {
                            u.mark_complete(turn_id);
                        }
                        let _ = gui_tx2.try_send(AppEvent::UpdateSourceSeq(
                            SubtitleUpdate::new(transcript.clone(), turn_id, seq, local_gen),
                        ));
                        let pair =
                            langs.lock().map(|c| c.clone()).unwrap_or_default();
                        // 使い回し判定はターン終了記録より先に行う。
                        // 重なりで古い確定が遅れてきたときは新ターンの数を消さない
                        let reused =
                            partials.lock().ok().and_then(|p| p.reuse(&transcript));
                        if let Ok(mut m) = stats.lock() {
                            m.entry(turn_id).or_default().complete = true;
                        }
                        turns.note_complete(turn_id);
                        if turn_id == turns.current()
                            && let Ok(mut p) = partials.lock()
                        {
                            p.note_turn_end();
                        }
                        if !pair.needs_translation() {
                            dev_log!("⏭️ 同一言語のため翻訳スキップ#{turn_id}");
                            dev_log!(
                                "⏱ {}",
                                serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "skip", "turn": turn_id, "seq": seq})
                            );
                            let _ = gui_tx2.try_send(AppEvent::UpdateTranslationSeq(
                                SubtitleUpdate::new(transcript.clone(), turn_id, seq, local_gen),
                            ));
                            continue;
                        }
                        // 高速モードの途中訳と一致すれば使い回す
                        if let Some(reuse) = reused {
                            dev_log!("⚡ 途中訳を使い回し#{turn_id}");
                            if let Ok(mut s) = session.lock() {
                                s.push_history(transcript.clone(), reuse.clone());
                            }
                            let _ = gui_tx2.try_send(AppEvent::UpdateTranslationSeq(
                                SubtitleUpdate::new(reuse, turn_id, seq, local_gen),
                            ));
                            continue;
                        }
                        let body = session.lock().ok().map(|mut s| {
                            s.build_request_json(
                                &model,
                                &pair.source,
                                &pair.target,
                                &transcript,
                            )
                        });
                        if let Some(body) = body {
                            if let Ok(mut m) = stats.lock() {
                                m.entry(turn_id).or_default().xlate_spawned += 1;
                            }
                            let task_client = client.clone();
                            let task_base = base.clone();
                            let task_key = cfg.api_key.clone();
                            let task_gui = gui_tx2.clone();
                            let task_session = session.clone();
                            let task_stats = stats.clone();
                            let task_gen = task_generation.clone();
                            tokio::spawn(async move {
                                let progress_gui = task_gui.clone();
                                let progress_gen = task_gen.clone();
                                let req_start = Instant::now();
                                let mut first_ms: Option<u128> = None;
                                match TranslationSession::post_chat_stream(
                                    &task_client,
                                    &task_base,
                                    &task_key,
                                    body,
                                    |acc, is_final| {
                                        if first_ms.is_none() {
                                            first_ms = Some(req_start.elapsed().as_millis());
                                        }
                                        // 旧世代の追補は捨てる。最終文は呼び出し元で送る
                                        if is_final
                                            || progress_gen.load(Ordering::Relaxed) != local_gen
                                        {
                                            return;
                                        }
                                        if let Some(text) = progress_text(acc) {
                                            let _ = progress_gui.try_send(
                                                AppEvent::UpdateTranslationSeq(
                                                    SubtitleUpdate::at(
                                                        text,
                                                        turn_id,
                                                        seq,
                                                        local_gen,
                                                        Instant::now(),
                                                    ),
                                                ),
                                            );
                                        }
                                    },
                                )
                                .await
                                {
                                    Ok(translated) => {
                                        dev_log!(
                                            "⏱ {}",
                                            serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "xlate", "turn": turn_id, "seq": seq, "first_ms": first_ms.unwrap_or(req_start.elapsed().as_millis()), "total_ms": req_start.elapsed().as_millis(), "out": translated.chars().count()})
                                        );
                                        if task_gen.load(Ordering::Relaxed) != local_gen {
                                            return;
                                        }
                                        dev_log!("🌐 翻訳#{turn_id}: {translated}");
                                        if let Ok(mut s) = task_session.lock() {
                                            s.push_history(transcript, translated.clone());
                                        }
                                        if let Ok(mut m) = task_stats.lock()
                                            && let Some(s) = m.get_mut(&turn_id)
                                        {
                                            s.xlate_ok += 1;
                                        }
                                        let _ = task_gui.try_send(
                                            AppEvent::UpdateTranslationSeq(SubtitleUpdate::at(
                                                translated,
                                                turn_id,
                                                seq,
                                                local_gen,
                                                Instant::now(),
                                            )),
                                        );
                                    }
                                    Err(e) => {
                                        dev_log!(
                                            "⏱ {}",
                                            serde_json::json!({"t": task_t0.elapsed().as_millis(), "ev": "xlate_err", "turn": turn_id, "seq": seq})
                                        );
                                        dev_log!("⚠️ 翻訳失敗#{turn_id}: {e}");
                                        if let Ok(mut m) = task_stats.lock()
                                            && let Some(s) = m.get_mut(&turn_id)
                                        {
                                            s.xlate_err += 1;
                                        }
                                    }
                                }
                            });
                        }
                    }
                    AsrEvent::SpeechActivity { speaking } => {
                        let now_ms = task_t0.elapsed().as_millis();
                        dev_log!(
                            "⏱ {}",
                            serde_json::json!({"t": now_ms, "ev": "speech", "state": if speaking { "start" } else { "end" }})
                        );
                        if speaking {
                            // 閉じ忘れ（再接続またぎ）はここで区切る
                            if let Some(prev) = utterance.take() {
                                dev_log!("{}", prev.verdict(now_ms));
                            }
                            utterance = Some(UtteranceWatch {
                                start_ms: now_ms,
                                ..Default::default()
                            });
                        } else if let Some(done) = utterance.take() {
                            dev_log!("{}", done.verdict(now_ms));
                        }
                    }
                    AsrEvent::Error(e) => dev_log!("⚠️ ASRエラー: {e}"),
                }
            }
            if let Ok(mut m) = stats.lock() {
                flush_turn_stats(&mut m, i32::MAX);
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
            dev_log!("{}", font_manager.report());
            let loaded_fonts = font_manager.loaded_styles();
            font_manager.install(&cc.egui_ctx);
            Box::new(LumeSubApp::new(
                cc.egui_ctx.clone(),
                gui_rx,
                tokio_handle,
                initial_text,
                controller,
                languages,
                generation.clone(),
                store.clone(),
                restored,
                saved_control,
                control_restored,
                settings_open,
                settings_draft,
                loaded_fonts,
                t0,
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
    /// 翻訳窓の上段（確定文）と下段（翻訳中の追補）
    translation_final: String,
    translation_streaming: String,
    receiver: Arc<Mutex<mpsc::Receiver<AppEvent>>>,
    controller: PipelineController,
    langs: SharedLanguageConfig,
    generation: SharedGeneration,
    source_guard: DisplayGuard,
    source_ledger: SentenceLedger,
    /// 寿命管理の対象となる直近の原文（順序つき更新の原文）。即時文では None
    source_live: Option<String>,
    translation_tail: TranslationTail,
    last_sentence_refresh: Instant,
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
    /// 終了時の確定保存用に毎フレーム覚える現在幾何。
    /// 実窓が古いドラッグ中は更新しない（保存値が正）
    last_source_geom: Option<geometry::WindowGeometry>,
    last_translation_geom: Option<geometry::WindowGeometry>,
    last_control_geom: Option<geometry::WindowGeometry>,
    placed_control_initial: bool,
    settings_open: bool,
    settings_draft: ApiConfig,
    show_source_draft: bool,
    show_translation_draft: bool,
    /// 整形済み字幕の覚え書き（論理文・幅・字体・視覚文）。変化時のみ再計算する
    source_cache: (String, u32, String, String),
    translation_cache: (String, u32, String, String),
    translation_live_cache: (String, u32, String, String),
    /// 読込済みフォントの選択肢（ID・書体名・スクリプト）。言語別に
    /// 絞り込んで字幕の字体切替に使う
    loaded_fonts: Vec<(String, String, Script)>,
    /// 計測ログの時刻原点。パイプラインと共有する
    t0: Instant,
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
        generation: SharedGeneration,
        store: SharedStore,
        restored: bool,
        control_saved: Option<geometry::WindowGeometry>,
        control_restored: bool,
        settings_open: bool,
        settings_draft: ApiConfig,
        loaded_fonts: Vec<(String, String, Script)>,
        t0: Instant,
    ) -> Self {
        let (tx_internal, rx_internal) = mpsc::channel(100);
        let (show_source_draft, show_translation_draft) = {
            let s = store.lock().expect("store lock poisoned");
            (s.show_source(), s.show_translation())
        };
        let app = Self {
            source_text: initial_text,
            translation_final: String::new(),
            translation_streaming: String::new(),
            receiver: Arc::new(Mutex::new(rx_internal)),
            controller,
            langs,
            generation,
            source_guard: DisplayGuard::new(),
            source_ledger: SentenceLedger::new(),
            source_live: None,
            translation_tail: TranslationTail::new(),
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
            last_source_geom: None,
            last_translation_geom: None,
            last_control_geom: None,
            placed_control_initial: control_restored,
            settings_open,
            settings_draft,
            show_source_draft,
            show_translation_draft,
            source_cache: (String::new(), u32::MAX, String::new(), String::new()),
            translation_cache: (String::new(), u32::MAX, String::new(), String::new()),
            translation_live_cache: (String::new(), u32::MAX, String::new(), String::new()),
            loaded_fonts,
            t0,
            last_sentence_refresh: Instant::now(),
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

    /// 終了時点の窓位置・サイズをファイルへ確定保存する。
    /// Quit・バツ・on_exit の共通処理
    fn save_geometries_now(&self) {
        if let Ok(mut s) = self.store.lock() {
            s.persist_geometries(
                exit_geometry_to_persist(self.fs_drag.is_dirty(), self.last_source_geom),
                exit_geometry_to_persist(
                    self.fs_drag_translation.is_dirty(),
                    self.last_translation_geom,
                ),
                self.last_control_geom,
            );
        }
    }

    /// 寿命切れの文を両窓から落とす。500msに1回だけ再計算し、変化があれば真
    fn refresh_sentence_expiry(
        &mut self,
        now: Instant,
        source_max: usize,
        translation_max: usize,
    ) -> bool {
        if now.duration_since(self.last_sentence_refresh) < Duration::from_millis(500) {
            return false;
        }
        self.last_sentence_refresh = now;
        let mut changed = false;
        if let Some(live) = self.source_live.as_ref() {
            let rendered =
                text::tail_for_display(&self.source_ledger.filter_fresh(live, now), source_max)
                    .to_string();
            if rendered != self.source_text {
                self.source_text = rendered;
                dev_log!("🕐 寿命切れの原文を消去");
                changed = true;
            }
        }
        let rendered_final = text::tail_for_display(
            &self.translation_tail.render_finalized(now),
            translation_max,
        )
        .to_string();
        if rendered_final != self.translation_final {
            self.translation_final = rendered_final;
            dev_log!("🕐 寿命切れの訳を消去");
            changed = true;
        }
        let rendered_live = text::tail_for_display(
            &self.translation_tail.render_streaming(now),
            translation_max,
        )
        .to_string();
        if rendered_live != self.translation_streaming {
            self.translation_streaming = rendered_live;
            changed = true;
        }
        changed
    }

    /// アプリ終了（Quitボタン・コントロール窓のバツ共通）
    fn quit(&self, ctx: &egui::Context) {
        // 終了前に窓位置・サイズをかならず保存する（on_exit の前に確定させる）
        self.save_geometries_now();
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
        let font = {
            let s = self.store.lock().expect("store lock poisoned");
            let choices = fonts::choices_for(
                &self.loaded_fonts,
                Some(s.language_config().target.script()),
            );
            fonts::subtitle_font_id(s.translation_font(), &choices, s.translation_font_size())
        };
        egui::CentralPanel::default().show(ctx, |ui| {
            // 上段＝確定文（白・太字）、下段＝翻訳中（灰色）。空の区画は出さない
            let mut parts = [
                PaintPart {
                    text: &self.translation_final,
                    cache: &mut self.translation_cache,
                    color: egui::Color32::WHITE,
                    strong: true,
                },
                PaintPart {
                    text: &self.translation_streaming,
                    cache: &mut self.translation_live_cache,
                    color: egui::Color32::from_gray(175),
                    strong: false,
                },
            ];
            if paint_subtitle_window(
                ui,
                &mut parts,
                font.clone(),
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
            dev_log!("⚠️ 両方の字幕を非表示にはできません（表示設定は変更しません）");
        }
        self.settings_open = false;
        dev_log!("⚙️ 設定を保存しました（API設定は次回起動から有効）");
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

        // 入出力言語の切替。変えたら Store に即保存し、字幕を即切り替える
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
                Some((before, langs.clone()))
            } else {
                None
            }
        } else {
            ui.colored_label(egui::Color32::RED, sh(t.lang_read_failed));
            None
        };
        if let Some((before, cfg)) = changed {
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_languages(cfg.source, cfg.target);
            // 旧世代の遅延訳を捨て、窓を即切り替える
            let next_gen = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
            self.source_guard.reset(next_gen);
            self.translation_tail.reset();
            if before.source != cfg.source {
                // 入力が変わったら両窓を空にしてASRを繋ぎ直す（bias即反映）
                self.source_ledger.reset();
                self.source_live = None;
                self.source_text.clear();
                self.translation_final.clear();
                self.translation_streaming.clear();
                self.source_cache = (String::new(), u32::MAX, String::new(), String::new());
                self.translation_cache =
                    (String::new(), u32::MAX, String::new(), String::new());
                self.translation_live_cache =
                    (String::new(), u32::MAX, String::new(), String::new());
                let status = self.controller.status();
                if status == CaptureState::Running || status == CaptureState::Paused {
                    let was_paused = status == CaptureState::Paused;
                    self.controller.stop();
                    self.controller.start();
                    if was_paused {
                        self.controller.pause();
                    }
                    dev_log!("🈯 入力言語の変更を即反映しました");
                }
            } else {
                // 出力のみは翻訳窓だけ空にする（原文は有効なまま）
                self.translation_final.clear();
                self.translation_streaming.clear();
                self.translation_cache =
                    (String::new(), u32::MAX, String::new(), String::new());
                self.translation_live_cache =
                    (String::new(), u32::MAX, String::new(), String::new());
            }
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
            dev_log!(
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
            dev_log!("🌐 高速翻訳: {}", if fast { "ON" } else { "OFF" });
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
        // 開発ログのオン・オフ。変えたら即保存・即反映する
        let before = self.store.lock().expect("store lock poisoned").dev_logs();
        let mut dev = before;
        ui.checkbox(&mut dev, sh(t.dev_logs));
        if dev != before {
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_dev_logs(dev);
            devlog::set_enabled(dev);
            dev_log!("🛠️ 開発ログ: {}", if dev { "ON" } else { "OFF" });
        }
        // 原文の字体。変えたら即保存する
        let before = self
            .store
            .lock()
            .expect("store lock poisoned")
            .source_font()
            .to_string();
        let mut current = before.clone();
        // 原文言語の書体に絞る（Auto＝全書体）
        let source_choices = {
            let s = self.store.lock().expect("store lock poisoned");
            fonts::choices_for(
                &self.loaded_fonts,
                s.language_config().source.script_filter(),
            )
        };
        egui::ComboBox::from_label(sh(t.source_font))
            .selected_text(font_choice_label(&source_choices, &current, t.font_default))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut current, String::new(), sh(t.font_default));
                for (id, label) in &source_choices {
                    ui.selectable_value(&mut current, id.clone(), label.as_str());
                }
            });
        if current != before {
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_source_font(current);
        }
        // 原文のサイズ。変えたら即保存する
        let before = self
            .store
            .lock()
            .expect("store lock poisoned")
            .source_font_size();
        let mut size = before;
        ui.horizontal(|ui| {
            ui.label(sh(t.source_font_size));
            ui.add(egui::Slider::new(
                &mut size,
                fonts::FONT_SIZE_MIN..=fonts::FONT_SIZE_MAX,
            ));
        });
        if size != before {
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_source_font_size(size);
        }
        // 翻訳の字体。変えたら即保存する
        let before = self
            .store
            .lock()
            .expect("store lock poisoned")
            .translation_font()
            .to_string();
        let mut current = before.clone();
        // 翻訳言語の書体に絞る
        let translation_choices = {
            let s = self.store.lock().expect("store lock poisoned");
            fonts::choices_for(
                &self.loaded_fonts,
                Some(s.language_config().target.script()),
            )
        };
        egui::ComboBox::from_label(sh(t.translation_font))
            .selected_text(font_choice_label(
                &translation_choices,
                &current,
                t.font_default,
            ))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut current, String::new(), sh(t.font_default));
                for (id, label) in &translation_choices {
                    ui.selectable_value(&mut current, id.clone(), label.as_str());
                }
            });
        if current != before {
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_translation_font(current);
        }
        // 翻訳のサイズ。変えたら即保存する
        let before = self
            .store
            .lock()
            .expect("store lock poisoned")
            .translation_font_size();
        let mut size = before;
        ui.horizontal(|ui| {
            ui.label(sh(t.translation_font_size));
            ui.add(egui::Slider::new(
                &mut size,
                fonts::FONT_SIZE_MIN..=fonts::FONT_SIZE_MAX,
            ));
        });
        if size != before {
            self.store
                .lock()
                .expect("store lock poisoned")
                .set_translation_font_size(size);
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
        // ROOT のバツ・Cmd+Q 等でも窓位置・サイズをかならず保存する。
        // この後 on_exit でも保存するが、二重保存は無害
        if ctx.input(|i| i.viewport().close_requested()) {
            self.save_geometries_now();
        }
        if let Ok(mut rx) = self.receiver.try_lock() {
            // 累積文は末尾に丸めて表示する（古い文が残り続けないよう）
            let (source_max, translation_max) = {
                let s = self.store.lock().expect("store lock poisoned");
                (s.source_tail_chars(), s.translation_tail_chars())
            };
            while let Ok(event) = rx.try_recv() {
                match event {
                    AppEvent::UpdateSource(text) => {
                        // 新セッションの待機文は無条件で出し、古い順序記憶を捨てる
                        let current = self.generation.load(Ordering::Relaxed);
                        self.source_guard.reset(current);
                        self.source_ledger.reset();
                        self.source_live = None;
                        self.translation_tail.reset();
                        self.source_text =
                            text::tail_for_display(&text, source_max).to_string();
                    }
                    AppEvent::UpdateSourceSeq(update) => {
                        if update.generation != self.generation.load(Ordering::Relaxed) {
                            dev_log!(
                                "🗑️ 旧世代の原文を破棄: turn={} seq={}",
                                update.turn,
                                update.seq
                            );
                            continue;
                        }
                        if self.source_guard.accept(&update) {
                            self.source_ledger.note(
                                update.generation,
                                &update.text,
                                update.spawned_at,
                            );
                            self.source_live = Some(update.text.clone());
                            self.source_text = text::tail_for_display(
                                &self
                                    .source_ledger
                                    .filter_fresh(&update.text, Instant::now()),
                                source_max,
                            )
                            .to_string();
                            dev_log!(
                                "⏱ {}",
                                serde_json::json!({"t": self.t0.elapsed().as_millis(), "ev": "show", "which": "src", "turn": update.turn, "seq": update.seq, "chars": update.text.chars().count()})
                            );
                        } else {
                            dev_log!(
                                "🗑️ 古い原文を破棄: turn={} seq={}",
                                update.turn,
                                update.seq
                            );
                            dev_log!(
                                "⏱ {}",
                                serde_json::json!({"t": self.t0.elapsed().as_millis(), "ev": "drop", "which": "src", "turn": update.turn, "seq": update.seq})
                            );
                        }
                    }
                    AppEvent::UpdateTranslation(text) => {
                        self.translation_tail.reset();
                        self.translation_final =
                            text::tail_for_display(&text, translation_max).to_string();
                        self.translation_streaming.clear();
                    }
                    AppEvent::UpdateTranslationSeq(update) => {
                        if update.generation != self.generation.load(Ordering::Relaxed) {
                            dev_log!(
                                "🗑️ 旧世代の訳を破棄: turn={} seq={}",
                                update.turn,
                                update.seq
                            );
                            continue;
                        }
                        if self.translation_tail.push(&update) {
                            let now = Instant::now();
                            self.translation_final = text::tail_for_display(
                                &self.translation_tail.render_finalized(now),
                                translation_max,
                            )
                            .to_string();
                            self.translation_streaming = text::tail_for_display(
                                &self.translation_tail.render_streaming(now),
                                translation_max,
                            )
                            .to_string();
                            dev_log!(
                                "⏱ {}",
                                serde_json::json!({"t": self.t0.elapsed().as_millis(), "ev": "show", "which": "tr", "turn": update.turn, "seq": update.seq, "chars": update.text.chars().count()})
                            );
                        } else {
                            dev_log!(
                                "🗑️ 古い訳を破棄: turn={} seq={}",
                                update.turn,
                                update.seq
                            );
                            dev_log!(
                                "⏱ {}",
                                serde_json::json!({"t": self.t0.elapsed().as_millis(), "ev": "drop", "which": "tr", "turn": update.turn, "seq": update.seq})
                            );
                        }
                    }
                }
            }
        }

        // 寿命切れの文を落とす（表示更新がなくても古文が消える）
        {
            let (source_max, translation_max) = {
                let s = self.store.lock().expect("store lock poisoned");
                (s.source_tail_chars(), s.translation_tail_chars())
            };
            self.refresh_sentence_expiry(Instant::now(), source_max, translation_max);
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
                dev_log!("🖥️ 原文窓を常時最前面にしました（全Space＋レベル1000）");
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
                let panel_font_size = self
                    .store
                    .lock()
                    .expect("store lock poisoned")
                    .source_font_size();
                let panel_font = self
                    .store
                    .lock()
                    .expect("store lock poisoned")
                    .source_font()
                    .to_string();
                let shown = screen::sync_fullscreen_panel(
                    &mut self.fs_panel,
                    screen::window_occlusion_visible(SOURCE_VIEWPORT_TITLE),
                    mirror,
                    flip_h,
                    &self.source_text,
                    panel_font_size,
                    &panel_font,
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
                            self.store
                                .lock()
                                .expect("store lock poisoned")
                                .set_source_geometry(g);
                            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                                rect.min,
                            ));
                            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(rect.size()));
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
                fonts::DEFAULT_SUBTITLE_SIZE,
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
        // 原文窓の移動・リサイズをメモリに追跡する（ファイル保存は終了時のみ）。
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
            self.last_source_geom = Some(*current);
            self.store
                .lock()
                .expect("store lock poisoned")
                .track_source(current);
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
        let source_font = {
            let s = self.store.lock().expect("store lock poisoned");
            let choices = fonts::choices_for(
                &self.loaded_fonts,
                s.language_config().source.script_filter(),
            );
            fonts::subtitle_font_id(s.source_font(), &choices, s.source_font_size())
        };
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut parts = [PaintPart {
                text: &self.source_text,
                cache: &mut self.source_cache,
                color: egui::Color32::WHITE,
                strong: true,
            }];
            if paint_subtitle_window(
                ui,
                &mut parts,
                source_font.clone(),
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
            self.last_control_geom = Some(*current);
            self.store
                .lock()
                .expect("store lock poisoned")
                .track_control(current);
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
                self.last_translation_geom = Some(*current);
                self.store
                    .lock()
                    .expect("store lock poisoned")
                    .track_translation(current);
            }
            let translation_outer = translation_current.map(rect_from_geometry);
            // 翻訳窓も常時最前面に。窓が出るまで毎フレーム試す
            if !self.translation_fullscreen_fixed {
                if screen::ensure_window_over_fullscreen(TRANSLATION_VIEWPORT_TITLE) {
                    self.translation_fullscreen_fixed = true;
                    dev_log!("🖥️ 翻訳窓を常時最前面にしました（全Space＋レベル1000）");
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
                let panel_font_size = self
                    .store
                    .lock()
                    .expect("store lock poisoned")
                    .translation_font_size();
                let panel_font = self
                    .store
                    .lock()
                    .expect("store lock poisoned")
                    .translation_font()
                    .to_string();
                let mirror_text = if self.translation_streaming.is_empty() {
                    self.translation_final.clone()
                } else if self.translation_final.is_empty() {
                    self.translation_streaming.clone()
                } else {
                    format!(
                        "{}\n{}",
                        self.translation_final, self.translation_streaming
                    )
                };
                let shown = screen::sync_fullscreen_panel(
                    &mut self.fs_panel_translation,
                    screen::window_occlusion_visible(TRANSLATION_VIEWPORT_TITLE),
                    mirror,
                    flip_h,
                    &mirror_text,
                    panel_font_size,
                    &panel_font,
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
                            self.store
                                .lock()
                                .expect("store lock poisoned")
                                .set_translation_geometry(g);
                            ctx.send_viewport_cmd_to(
                                translation_id,
                                egui::ViewportCommand::OuterPosition(rect.min),
                            );
                            ctx.send_viewport_cmd_to(
                                translation_id,
                                egui::ViewportCommand::InnerSize(rect.size()),
                            );
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
                fonts::DEFAULT_SUBTITLE_SIZE,
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

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 終了時点の窓位置・サイズを確定保存する（Quit 経路の保存漏れへの安全網。
        // 既に保存済みでも同じ内容の上書きで無害）
        self.save_geometries_now();
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

    #[test]
    fn sentence_marks_counts_end_marks() {
        assert_eq!(sentence_marks("Hello. World"), 1);
        assert_eq!(sentence_marks("Hello. World."), 2);
        assert_eq!(sentence_marks("こんにちは。元気？"), 2);
        assert_eq!(sentence_marks("看视频，开会。"), 1);
        assert_eq!(sentence_marks("مرحبا؟"), 1);
        assert_eq!(sentence_marks("नमस्ते।"), 1);
        // 小数は文末にしない
        assert_eq!(sentence_marks("3.5kg"), 0);
        assert_eq!(sentence_marks("まだ途中"), 0);
        assert_eq!(sentence_marks(""), 0);
    }

    #[test]
    fn sentence_ready_fires_on_new_sentence_end() {
        let mut p = PartialTracker::new();
        let t0 = Instant::now();
        // 文末なし→発火しない
        assert!(!p.sentence_ready("hello"));
        // 新しい文末→間引き内でも発火する
        assert!(p.sentence_ready("hello. world"));
        p.note_translated("hello. world".into(), "こんにちは。世界".into(), t0);
        assert!(!p.sentence_ready("hello. world"));
        // 2文目が終わったら再発火
        assert!(p.sentence_ready("hello. world. next"));
        p.note_translated(
            "hello. world. next".into(),
            "こんにちは。世界。次".into(),
            t0,
        );
        assert!(!p.sentence_ready("hello. world. next"));
        // ターン確定で次ターンは0から数え直す
        p.note_turn_end();
        assert!(!p.sentence_ready("new turn"));
        assert!(p.sentence_ready("new turn."));
    }

    #[test]
    fn leading_preview_fires_once_per_turn() {
        let mut p = PartialTracker::new();
        let t0 = Instant::now();
        // ターン冒頭の途中文は即訳する
        assert!(p.leading_ready("hello"));
        assert!(!p.leading_ready(""));
        p.note_spawned("hello".into(), t0);
        // 投げたら次は発火しない（文末・間引きに任せる）
        assert!(!p.leading_ready("hello world"));
        // ターン確定で次の冒頭に備える
        p.note_turn_end();
        assert!(p.leading_ready("next turn"));
    }

    #[test]
    fn sentence_preview_still_fires_after_leading() {
        let mut p = PartialTracker::new();
        let t0 = Instant::now();
        p.note_spawned("hello".into(), t0);
        // 冒頭訳のあとも新しい文末では訳す
        assert!(p.sentence_ready("hello. world"));
        assert!(!p.leading_ready("hello. world"));
    }

    #[test]
    fn leading_preview_waits_for_min_chars() {
        let p = PartialTracker::new();
        // 1字の断片では投げない。2字から投げる
        assert!(!p.leading_ready("あ"));
        assert!(p.leading_ready("ab"));
        assert!(p.leading_ready("abc"));
    }

    #[test]
    fn growth_preview_fires_on_text_growth() {
        let mut p = PartialTracker::new();
        let t0 = Instant::now();
        p.note_spawned("hello".into(), t0);
        // 少しの伸びでは発火しない
        assert!(!p.growth_ready("hello world"));
        // 10字以上の伸びで発火する
        let grown = format!("hello world{}", "x".repeat(10));
        assert!(p.growth_ready(&grown));
        // 送信後は同文で発火しない
        p.note_spawned(grown.clone(), t0);
        assert!(!p.growth_ready(&grown));
        // ターン確定で0から数え直す
        p.note_turn_end();
        assert!(p.growth_ready(&"y".repeat(10)));
        assert!(!p.growth_ready(&"y".repeat(9)));
    }

    #[test]
    fn slow_mode_keeps_translating_unpunctuated_partials() {
        // 句読点のない累積部分文でも翻訳が止まらない（動画の再現）
        let mut p = PartialTracker::new();
        let t0 = Instant::now();
        let mut fires = 0;
        for len in 1..=100 {
            let text: String = "あ".repeat(len);
            if p.preview_trigger(&text, false, t0).is_some() {
                fires += 1;
                p.note_spawned(text, t0);
            }
        }
        // 冒頭1回＋10字ごとの追補9回
        assert_eq!(fires, 10);
    }

    #[test]
    fn preview_trigger_matches_configured_defaults() {
        // 実動作と記録再生が同じ判定を使うことの担保
        let p = PartialTracker::new();
        let t0 = Instant::now();
        for text in [
            "",
            "a",
            "ab",
            "abc",
            "hello.",
            "hello. world. more text here yes",
        ] {
            for fast in [false, true] {
                assert_eq!(
                    p.preview_trigger(text, fast, t0),
                    p.preview_trigger_cfg(
                        text,
                        fast,
                        t0,
                        PREVIEW_GROWTH_CHARS,
                        LEADING_MIN_CHARS
                    ),
                    "{text:?} fast={fast}"
                );
            }
        }
    }

    #[test]
    fn partial_tracker_async_spawn_then_complete() {
        use std::time::Duration;
        let mut p = PartialTracker::new();
        let t0 = Instant::now();
        // 起動時は訳せる
        assert!(p.should_translate("hello.", t0));
        // 送信時点では間引きが効くが、使い回しはまだできない
        p.note_spawned("hello.".into(), t0);
        assert!(!p.should_translate("hello. world", t0));
        assert_eq!(p.reuse("hello."), None);
        // 完了で使い回せる
        p.note_completed("hello.".into(), "こんにちは。".into());
        assert_eq!(p.reuse("hello."), Some("こんにちは。".to_string()));
        // 新しい送信で古い訳は無効になる
        let t1 = t0 + Duration::from_secs(2);
        assert!(p.should_translate("hello. world.", t1));
        p.note_spawned("hello. world.".into(), t1);
        assert_eq!(p.reuse("hello. world."), None);
        // 古い完了は新しい送信を上書きしない
        p.note_completed("hello.".into(), "古い".into());
        assert_eq!(p.reuse("hello. world."), None);
        p.note_completed("hello. world.".into(), "こんにちは。世界。".into());
        assert_eq!(
            p.reuse("hello. world."),
            Some("こんにちは。世界。".to_string())
        );
    }

    #[test]
    fn display_guard_drops_stale_sentence() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut g = DisplayGuard::new();
        // 初回は受け付ける
        let first = SubtitleUpdate::at("uno".into(), 1, 1, 0, t0);
        assert!(g.accept(&first));
        // 同順序・同時刻の複製は受け付ける（ストリーミング追補と区別しない。
        // 同一トリガーの文言は増える一方なので、再描画しても害がない）
        let dup_old = SubtitleUpdate::at("uno".into(), 1, 1, 0, t0);
        assert!(g.accept(&dup_old));
        // 同じターンの新しい文は受け付ける
        let second = SubtitleUpdate::at("uno dos".into(), 1, 2, 0, t0 + Duration::from_millis(10));
        assert!(g.accept(&second));
        // 新しいターンは受け付ける
        let next_turn = SubtitleUpdate::at("next".into(), 2, 3, 0, t0 + Duration::from_millis(20));
        assert!(g.accept(&next_turn));
        // 古いターンの遅延到着は捨てる（seq が新しくても）
        let late_old = SubtitleUpdate::at("uno late".into(), 1, 99, 0, t0 + Duration::from_millis(30));
        assert!(!g.accept(&late_old));
        // 古い世代は捨てる
        let old_gen = SubtitleUpdate::at("old".into(), 5, 100, 0, t0 + Duration::from_millis(40));
        let mut g2 = DisplayGuard::new();
        g2.reset(1);
        assert!(!g2.accept(&old_gen));
        // 新しい世代は受け付ける（ターン番号が若くても）
        let new_gen = SubtitleUpdate::at("new".into(), 1, 101, 1, t0 + Duration::from_millis(50));
        assert!(g2.accept(&new_gen));
    }

    #[test]
    fn display_guard_accepts_streaming_progress() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut g = DisplayGuard::new();
        // 同一トリガーの追補（同順序・新時刻）は受け付ける
        let first = SubtitleUpdate::at("He".into(), 1, 5, 0, t0);
        assert!(g.accept(&first));
        let progress =
            SubtitleUpdate::at("Hello".into(), 1, 5, 0, t0 + Duration::from_millis(150));
        assert!(g.accept(&progress));
        // 同順序でも古い時刻の到着は捨てる
        let late = SubtitleUpdate::at("Hel".into(), 1, 5, 0, t0 + Duration::from_millis(100));
        assert!(!g.accept(&late));
    }

    /// 上段（確定）・下段（追補）の表示文
    fn rendered(tail: &TranslationTail, now: Instant) -> (String, String) {
        (tail.render_finalized(now), tail.render_streaming(now))
    }

    #[test]
    fn translation_tail_carries_previous_turn_final() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        // 現行ターンの追補と確定
        assert!(tail.push(&SubtitleUpdate::at(
            "Lume …".into(),
            1,
            1,
            0,
            t0
        )));
        assert!(tail.push(&SubtitleUpdate::at(
            "Lume captures audio.".into(),
            1,
            2,
            0,
            t0 + Duration::from_millis(10)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_secs(1)),
            ("Lume captures audio.".to_string(), String::new())
        );
        // 新ターンの追補が来ても確定文だけ出す
        assert!(tail.push(&SubtitleUpdate::at(
            "The …".into(),
            2,
            3,
            0,
            t0 + Duration::from_millis(20)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_secs(1)),
            ("Lume captures audio.".to_string(), "The …".to_string())
        );
        // 旧ターンの遅延確定は前ターン枠に救済する（捨てない）
        assert!(tail.push(&SubtitleUpdate::at(
            "Lume captures audio. Watch.".into(),
            1,
            4,
            0,
            t0 + Duration::from_millis(30)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_secs(1)),
            (
                "Lume captures audio.\nWatch.".to_string(),
                "The …".to_string()
            )
        );
        // 新ターンの追補が来ても確定文だけ出す
        assert!(tail.push(&SubtitleUpdate::at(
            "You …".into(),
            3,
            5,
            0,
            t0 + Duration::from_millis(40)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_secs(1)),
            (
                "Lume captures audio.\nWatch.".to_string(),
                "You …".to_string()
            )
        );
        // さらに新ターンが来たら3つ前は落ちる
        assert!(tail.push(&SubtitleUpdate::at(
            "T4 …".into(),
            4,
            6,
            0,
            t0 + Duration::from_millis(50)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_secs(1)),
            (String::new(), "T4 …".to_string())
        );
        // 3つ前の遅延到着は捨てる
        assert!(!tail.push(&SubtitleUpdate::at(
            "Lume late".into(),
            1,
            99,
            0,
            t0 + Duration::from_millis(60)
        )));
    }

    /// 動画23:55のt13再現：前ターン確定の寿命内に次ターン確定が来たら両方出す
    #[test]
    fn translation_tail_keeps_final_across_next_turn_complete() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        // Bターンの追補（プレビュー由来）
        assert!(tail.push(&SubtitleUpdate::at("Hello …".into(), 2, 10, 0, t0)));
        assert!(tail.push(&SubtitleUpdate::at(
            "Hello, I'm L …".into(),
            2,
            10,
            0,
            t0 + Duration::from_millis(1700)
        )));
        // Bターンの確定（complete要求の最終文）
        assert!(tail.push(&SubtitleUpdate::at(
            "This is LumeSub.".into(),
            2,
            11,
            0,
            t0 + Duration::from_millis(1800)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(2000)),
            ("This is LumeSub.".to_string(), String::new())
        );
        // Cターンの確定（モデル由来の "..." 付き確定文）。B確定の寿命内
        assert!(tail.push(&SubtitleUpdate::at(
            "Real-time translation subtitles ...".into(),
            3,
            12,
            0,
            t0 + Duration::from_millis(4100)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(4500)),
            (
                "This is LumeSub.\nReal-time translation subtitles ...".to_string(),
                String::new()
            )
        );
    }

    /// 同一ターン内で確定文が別文に置き換わったら別発話として両方残す。
    /// プロバイダが turnId を進めない場合の救済（動画23:55のt13相当）
    #[test]
    fn translation_tail_splits_same_turn_completed_collision() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        assert!(tail.push(&SubtitleUpdate::at("This is LumeSub.".into(), 2, 11, 0, t0)));
        // 同じターン番号で別内容の確定文が来たら上書きせず新枠へずらす
        assert!(tail.push(&SubtitleUpdate::at(
            "Real-time translation subtitles ...".into(),
            2,
            12,
            0,
            t0 + Duration::from_millis(2300)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(2700)),
            (
                "This is LumeSub.\nReal-time translation subtitles ...".to_string(),
                String::new()
            )
        );
        // 同文の再送は上書き（期限の延長）のまま
        assert!(tail.push(&SubtitleUpdate::at(
            "Real-time translation subtitles ...".into(),
            2,
            13,
            0,
            t0 + Duration::from_millis(2800)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(2900)),
            (
                "This is LumeSub.\nReal-time translation subtitles ...".to_string(),
                String::new()
            )
        );
    }

    #[test]
    fn translation_tail_orders_within_slot() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        assert!(tail.push(&SubtitleUpdate::at("B".into(), 2, 3, 0, t0)));
        // 同ターンの古い順序は捨てる
        assert!(!tail.push(&SubtitleUpdate::at(
            "A".into(),
            2,
            2,
            0,
            t0 + Duration::from_millis(10)
        )));
        // 同順序の追補は新時刻だけ受け付ける
        assert!(tail.push(&SubtitleUpdate::at(
            "B …".into(),
            2,
            3,
            0,
            t0 + Duration::from_millis(20)
        )));
        assert!(!tail.push(&SubtitleUpdate::at(
            "B …".into(),
            2,
            3,
            0,
            t0 + Duration::from_millis(10)
        )));
        // 確定文がなければ最新の追補を下段に出す
        assert!(tail.push(&SubtitleUpdate::at(
            "C …".into(),
            3,
            4,
            0,
            t0 + Duration::from_millis(30)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_secs(1)),
            (String::new(), "C …".to_string())
        );
        // 前ターンの確定が届いたら上段に出す
        assert!(tail.push(&SubtitleUpdate::at(
            "B done.".into(),
            2,
            5,
            0,
            t0 + Duration::from_millis(40)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_secs(1)),
            ("B done.".to_string(), "C …".to_string())
        );
    }

    #[test]
    fn translation_tail_resets_on_generation() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        assert!(tail.push(&SubtitleUpdate::at(
            "old".into(),
            7,
            1,
            0,
            t0
        )));
        // 世代が変わったら旧枠を捨て、若いターン番号から始める
        assert!(tail.push(&SubtitleUpdate::at(
            "new …".into(),
            1,
            2,
            1,
            t0 + Duration::from_millis(10)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_secs(1)),
            (String::new(), "new …".to_string())
        );
    }

    #[test]
    fn translation_tail_expires_old_sentences() {
        use std::time::Duration;
        let t0 = Instant::now();
        // "Old done." の寿命4秒（最短表示）。期限 t0+4秒、"New …" は t0+5秒
        let mut tail = TranslationTail::new();
        assert!(tail.push(&SubtitleUpdate::at("Old done.".into(), 1, 1, 0, t0)));
        assert!(tail.push(&SubtitleUpdate::at(
            "New …".into(),
            2,
            2,
            0,
            t0 + Duration::from_secs(1)
        )));
        // 確定文は上段、追補は下段に出る
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(2500)),
            ("Old done.".to_string(), "New …".to_string())
        );
        // 旧文だけ寿命切れで落ちる
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(4500)),
            (String::new(), "New …".to_string())
        );
        // 両方切れたら空になる
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(5500)),
            (String::new(), String::new())
        );
    }

    #[test]
    fn translation_tail_refreshes_streaming_sentences() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        // 累積文に含まれる文は追補のたび寿命が延びる
        // （"First." の寿命4秒に対し、追補で期限が t0+4秒 → t0+5秒へ延びる）
        assert!(tail.push(&SubtitleUpdate::at(
            "First. Sec …".into(),
            1,
            1,
            0,
            t0
        )));
        assert!(tail.push(&SubtitleUpdate::at(
            "First. Second …".into(),
            1,
            2,
            0,
            t0 + Duration::from_secs(1)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(4500)),
            (String::new(), "First.\nSecond …".to_string())
        );
    }

    #[test]
    fn translation_tail_splits_finalized_and_streaming() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        // 確定文がなければ追補を下段に出す
        assert!(tail.push(&SubtitleUpdate::at("L1 …".into(), 1, 1, 0, t0)));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(100)),
            (String::new(), "L1 …".to_string())
        );
        // 確定文は上段、追補は下段に分ける
        assert!(tail.push(&SubtitleUpdate::at(
            "L2.".into(),
            2,
            2,
            0,
            t0 + Duration::from_millis(200)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(300)),
            ("L2.".to_string(), String::new())
        );
        assert!(tail.push(&SubtitleUpdate::at(
            "L1 final.".into(),
            1,
            3,
            0,
            t0 + Duration::from_millis(400)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(500)),
            ("L1 final.\nL2.".to_string(), String::new())
        );
    }

    #[test]
    fn translation_tail_rescues_two_back_late_final() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        // 追補だけ3ターン進んでも2つ前の確定は救済し、上下段に分けて出す
        for (i, text) in ["T1 …", "T2 …", "T3 …"].into_iter().enumerate() {
            assert!(tail.push(&SubtitleUpdate::at(
                text.into(),
                i as i32 + 1,
                i as u64 + 1,
                0,
                t0 + Duration::from_millis(i as u64 * 10)
            )));
        }
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(100)),
            (String::new(), "T3 …".to_string())
        );
        assert!(tail.push(&SubtitleUpdate::at(
            "T1 done.".into(),
            1,
            9,
            0,
            t0 + Duration::from_millis(200)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(300)),
            ("T1 done.".to_string(), "T3 …".to_string())
        );
    }

    #[test]
    fn translation_tail_rejects_shorter_streaming_regression() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        // 同ターンの追補は表示文より短くならない（長い→短い→長いの瞬き防止）
        assert!(tail.push(&SubtitleUpdate::at(
            "Long streaming text …".into(),
            1,
            1,
            0,
            t0
        )));
        // 新要求の出始め（短い追補）は採らない
        assert!(!tail.push(&SubtitleUpdate::at(
            "Short …".into(),
            1,
            2,
            0,
            t0 + Duration::from_millis(100)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(200)),
            (String::new(), "Long streaming text …".to_string())
        );
        // 追いついたら採用する
        assert!(tail.push(&SubtitleUpdate::at(
            "Long streaming text grows …".into(),
            1,
            3,
            0,
            t0 + Duration::from_millis(300)
        )));
        // 短い確定文はつねに採用する
        assert!(tail.push(&SubtitleUpdate::at(
            "Final.".into(),
            1,
            4,
            0,
            t0 + Duration::from_millis(400)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(500)),
            ("Final.".to_string(), String::new())
        );
    }

    #[test]
    fn progress_text_skips_empty_accumulation() {
        assert_eq!(progress_text(""), None);
        assert_eq!(progress_text("  "), None);
        assert_eq!(progress_text("Hello"), Some("Hello …".to_string()));
    }

    #[test]
    fn translation_tail_skips_expired_anchor() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        assert!(tail.push(&SubtitleUpdate::at("Old done.".into(), 1, 1, 0, t0)));
        assert!(tail.push(&SubtitleUpdate::at(
            "T2 …".into(),
            2,
            2,
            0,
            t0 + Duration::from_secs(1)
        )));
        assert!(tail.push(&SubtitleUpdate::at(
            "T3 …".into(),
            3,
            3,
            0,
            t0 + Duration::from_secs(2)
        )));
        // 確定文が寿命内は上段に残し、追補は下段に出す
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(2500)),
            ("Old done.".to_string(), "T3 …".to_string())
        );
        // 寿命切れの確定文は選ばず、新しい追補を出す
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(4500)),
            (String::new(), "T3 …".to_string())
        );
    }

    #[test]
    fn translation_tail_hides_dots_only_sentences() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut tail = TranslationTail::new();
        // 点々だけの確定文は表示しない（どの文字コードでも）
        assert!(tail.push(&SubtitleUpdate::at("...".into(), 1, 1, 0, t0)));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(100)),
            (String::new(), String::new())
        );
        // 中身のある文は出す。点々は枠を塞がない
        assert!(tail.push(&SubtitleUpdate::at(
            "Real.".into(),
            2,
            2,
            0,
            t0 + Duration::from_millis(200)
        )));
        assert!(tail.push(&SubtitleUpdate::at(
            "⋯".into(),
            3,
            3,
            0,
            t0 + Duration::from_millis(300)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(400)),
            ("Real.".to_string(), String::new())
        );
        // 混ざった点々断片だけ落とす
        assert!(tail.push(&SubtitleUpdate::at(
            "Mixed. ...".into(),
            4,
            4,
            0,
            t0 + Duration::from_millis(500)
        )));
        assert_eq!(
            rendered(&tail, t0 + Duration::from_millis(600)),
            ("Real.\nMixed.".to_string(), String::new())
        );
    }

    #[test]
    fn translation_tail_pruned_sentence_stays_expired() {
        use std::time::Duration;
        let t0 = Instant::now();
        let life = text::sentence_lifetime("Ancient history here.");
        let mut tail = TranslationTail::new();
        assert!(tail.push(&SubtitleUpdate::at(
            "Ancient history here.".into(),
            1,
            1,
            0,
            t0
        )));
        // 寿命を超えた後の受け付けで台帳から掃除される
        assert!(tail.push(&SubtitleUpdate::at(
            "New stuff …".into(),
            2,
            2,
            0,
            t0 + life + Duration::from_secs(1)
        )));
        // 掃除済みの旧文は復活しない
        assert_eq!(
            rendered(&tail, t0 + life + Duration::from_secs(2)),
            (String::new(), "New stuff …".to_string())
        );
    }

    #[test]
    fn sentence_ledger_resets_on_generation() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut l = SentenceLedger::new();
        l.note(0, "Old.", t0);
        assert_eq!(l.filter_fresh("Old.", t0 + Duration::from_secs(1)), "Old.");
        // 世代が変わったら旧文を捨てる
        l.note(1, "New.", t0 + Duration::from_secs(2));
        assert_eq!(
            l.filter_fresh("Old. New.", t0 + Duration::from_secs(3)),
            "New."
        );
        l.reset();
        assert_eq!(l.filter_fresh("New.", t0 + Duration::from_secs(3)), "");
    }

    #[test]
    fn refresh_sentence_expiry_drops_stale_text() {
        use std::time::Duration;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = test_app(rt.handle().clone());
        let old = Instant::now() - text::SUBTITLE_MAX_DISPLAY - Duration::from_secs(1);
        app.translation_tail
            .push(&SubtitleUpdate::at("Old done.".into(), 1, 1, 0, old));
        app.translation_final = "Old done.".to_string();
        app.translation_streaming = "stale …".to_string();
        app.source_ledger.note(0, "古い原文。", old);
        app.source_live = Some("古い原文。".to_string());
        app.source_text = "古い原文。".to_string();
        app.last_sentence_refresh = Instant::now() - Duration::from_secs(1);
        // 寿命切れで両窓とも空になる
        assert!(app.refresh_sentence_expiry(Instant::now(), 160, 160));
        assert_eq!(app.translation_final, "");
        assert_eq!(app.translation_streaming, "");
        assert_eq!(app.source_text, "");
        // 500ms以内は再計算しない
        assert!(!app.refresh_sentence_expiry(Instant::now(), 160, 160));
    }

    #[test]
    fn refresh_sentence_expiry_keeps_immediate_and_fresh_text() {
        use std::time::Duration;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = test_app(rt.handle().clone());
        // 即時文（live なし）は寿命管理の対象外
        app.source_text = "🎧 Listening…".to_string();
        assert!(app.source_live.is_none());
        // 新しい訳は残る
        let now = Instant::now();
        app.translation_tail
            .push(&SubtitleUpdate::at("Fresh.".into(), 1, 1, 0, now));
        app.translation_final = "Fresh.".to_string();
        app.last_sentence_refresh = now - Duration::from_secs(1);
        assert!(!app.refresh_sentence_expiry(now, 160, 160));
        assert_eq!(app.source_text, "🎧 Listening…");
        assert_eq!(app.translation_final, "Fresh.");
        assert_eq!(app.translation_streaming, "");
    }

    #[test]
    fn turn_stats_summary_counts_pending() {
        use std::time::Duration;
        let t0 = Instant::now();
        let s = TurnStats {
            start_at: Some(t0),
            first_partial_at: Some(t0 + Duration::from_millis(800)),
            partials: 12,
            complete: true,
            xlate_spawned: 3,
            xlate_ok: 1,
            xlate_err: 1,
        };
        assert_eq!(
            s.summary(7),
            "📊 ターン#7: partials=12 初回800ms 確定あり 翻訳要求3(完了1/失敗1/残1)"
        );
        // 開始・初回がなければ初回はハイフン
        let empty = TurnStats::default();
        assert_eq!(
            empty.summary(1),
            "📊 ターン#1: partials=0 初回- 確定なし 翻訳要求0(完了0/失敗0/残0)"
        );
    }

    #[test]
    fn utterance_verdict_reports_recognition() {
        let mut u = UtteranceWatch {
            start_ms: 3900,
            ..Default::default()
        };
        assert_eq!(u.verdict(14100), "🔍 音声 3.9–14.1s: 認識なし");
        u.mark_partial(1);
        u.mark_partial(1);
        u.mark_complete(2);
        assert_eq!(
            u.verdict(14100),
            "🔍 音声 3.9–14.1s: 認識あり (turn=1,2 確定あり)"
        );
    }

    #[test]
    fn flush_turn_stats_drops_only_older_turns() {
        let mut map = BTreeMap::from([(1, TurnStats::default()), (2, TurnStats::default())]);
        flush_turn_stats(&mut map, 2);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec![2]);
        flush_turn_stats(&mut map, i32::MAX);
        assert!(map.is_empty());
    }

    #[test]
    fn turn_tracker_advances_monotonically() {
        let mut t = TurnTracker::new();
        assert_eq!(t.current(), 0);
        // 新ターンは進む
        assert!(t.note_speech_start(1));
        assert_eq!(t.current(), 1);
        // 同じ開始の重複では進まない
        assert!(!t.note_speech_start(1));
        // 確定で遅れない
        t.note_complete(1);
        assert_eq!(t.current(), 1);
        // 次の開始で進む
        assert!(t.note_speech_start(2));
        // 遅延した古い確定・開始では戻らない
        t.note_complete(1);
        assert_eq!(t.current(), 2);
        assert!(!t.note_speech_start(1));
        assert_eq!(t.current(), 2);
        // 開始が欠けても確定で追いつく
        t.note_complete(5);
        assert_eq!(t.current(), 5);
        // リセットで0に戻る
        t.reset();
        assert_eq!(t.current(), 0);
    }

    #[test]
    fn font_choice_label_prefers_scripts_name() {
        let loaded = vec![("jp".to_string(), "Japanese".to_string())];
        assert_eq!(font_choice_label(&loaded, "", "Default"), "Default");
        assert_eq!(font_choice_label(&loaded, "jp", "Default"), "Japanese");
        assert_eq!(font_choice_label(&loaded, "nope", "Default"), "nope");
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
            Arc::new(AtomicU64::new(0)),
            store,
            false,
            None,
            false,
            false,
            ApiConfig::default(),
            vec![],
            Instant::now(),
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
        cache: &mut (String, u32, String, String),
        pos: egui::Pos2,
        enabled: bool,
        pressed: bool,
    ) -> bool {
        let mut clicked = false;
        let _ = ctx.run(click_input(pos, pressed), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut parts = [PaintPart {
                    text: "hello",
                    cache,
                    color: egui::Color32::WHITE,
                    strong: true,
                }];
                clicked = paint_subtitle_window(
                    ui,
                    &mut parts,
                    egui::FontId::proportional(28.0),
                    true,
                    "drag",
                    "test",
                    enabled,
                );
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
    fn exit_geometry_to_persist_skips_dirty_drag() {
        let g = geometry::WindowGeometry {
            x: 1.0,
            y: 2.0,
            w: 3.0,
            h: 4.0,
        };
        // ドラッグ中（実窓が古い）は渡さず、メモリ側のドラッグ値を据え置く
        assert_eq!(exit_geometry_to_persist(true, Some(g)), None);
        assert_eq!(exit_geometry_to_persist(false, Some(g)), Some(g));
        assert_eq!(exit_geometry_to_persist(false, None), None);
    }

    fn quit_scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lume-{}-{}.json", name, std::process::id()))
    }

    #[test]
    fn quit_saves_window_geometries_to_file() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = test_app(rt.handle().clone());
        let path = quit_scratch("quit-geom");
        let _ = std::fs::remove_file(&path);
        app.store = crate::store::shared(Store::load_from(Some(path.clone())));
        let g = geometry::WindowGeometry {
            x: 10.0,
            y: 20.0,
            w: 800.0,
            h: 150.0,
        };
        app.last_source_geom = Some(g);
        app.last_translation_geom = Some(g);
        app.last_control_geom = Some(g);
        assert!(!path.exists());
        // Quitボタンは quit() を呼ぶ。on_exit を待たずに保存する
        let ctx = egui::Context::default();
        let _ = ctx.run(raw_input_with(vec![]), |ctx| {
            app.quit(ctx);
        });
        let loaded = Store::load_from(Some(path.clone()));
        assert_eq!(loaded.source(), Some(g));
        assert_eq!(loaded.translation(), Some(g));
        assert_eq!(loaded.control(), Some(g));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn control_close_saves_window_geometries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = test_app(rt.handle().clone());
        let path = quit_scratch("close-geom");
        let _ = std::fs::remove_file(&path);
        app.store = crate::store::shared(Store::load_from(Some(path.clone())));
        let g = geometry::WindowGeometry {
            x: 30.0,
            y: 40.0,
            w: 320.0,
            h: 440.0,
        };
        app.last_source_geom = Some(g);
        app.last_translation_geom = Some(g);
        app.last_control_geom = Some(g);
        let ctx = egui::Context::default();
        // 通常フレームでは書かない
        let _ = ctx.run(raw_input_with(vec![]), |ctx| {
            app.control_frame(ctx);
        });
        assert!(!path.exists());
        // コントロール窓のバツで保存する
        let _ = ctx.run(
            raw_input_with(vec![egui::ViewportEvent::Close]),
            |ctx| {
                app.control_frame(ctx);
            },
        );
        let loaded = Store::load_from(Some(path.clone()));
        assert_eq!(loaded.source(), Some(g));
        assert_eq!(loaded.translation(), Some(g));
        assert_eq!(loaded.control(), Some(g));
        let _ = std::fs::remove_file(&path);
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
        let mut cache = (String::new(), u32::MAX, String::new(), String::new());
        let mut clicked = true;
        let _ = ctx.run(raw_input_with(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut parts = [PaintPart {
                    text: "hello",
                    cache: &mut cache,
                    color: egui::Color32::WHITE,
                    strong: true,
                }];
                clicked = paint_subtitle_window(
                    ui,
                    &mut parts,
                    egui::FontId::proportional(28.0),
                    false,
                    "drag",
                    "test",
                    true,
                );
            });
        });
        assert!(!clicked);
        assert_eq!(cache.0, "hello");
        assert!(!cache.3.is_empty());
    }

    fn paint_frame_for_test(
        ctx: &egui::Context,
        cache: &mut (String, u32, String, String),
        enabled: bool,
        locate: Option<&mut egui::Pos2>,
    ) {
        let _ = ctx.run(raw_input_with(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                if let Some(out) = locate {
                    let rect = ui.max_rect();
                    *out = egui::Pos2::new(rect.max.x - 17.0, rect.min.y + 12.0);
                }
                let mut parts = [PaintPart {
                    text: "hello",
                    cache,
                    color: egui::Color32::WHITE,
                    strong: true,
                }];
                paint_subtitle_window(
                    ui,
                    &mut parts,
                    egui::FontId::proportional(28.0),
                    true,
                    "drag",
                    "test",
                    enabled,
                );
            });
        });
    }

    #[test]
    fn disabled_close_ignores_clicks() {
        let ctx = egui::Context::default();
        let mut cache = (String::new(), u32::MAX, String::new(), String::new());
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

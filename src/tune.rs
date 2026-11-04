//! 記録再生による翻訳タイミングの調整
//!
//! 使い方:
//! 1. アプリの設定で開発ログをONにし、音声を流して標準出力を保存する
//! 2. `cargo run -- --tune timing.log` で計測値と閾値別の予測を比べる
//!
//! 開発ログの `⏱ ` 行（JSON）だけを読み、実物の [`PartialTracker`]・
//! [`TurnTracker`]・[`TranslationTail`] に仮想時刻で再生する。翻訳の所要は
//! 記録の中央値で模す（`--first-ms`・`--total-ms` で上書き可）。

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::text::split_sentences;
use crate::{
    LEADING_MIN_CHARS, PREVIEW_GROWTH_CHARS, PartialTracker, SubtitleUpdate, TranslationTail,
    TurnTracker,
};

/// 記録行の種別。`⏱ ` の後ろのJSONに対応する。
/// 未読の項目も記録形式として保持する
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "ev")]
enum Rec {
    #[serde(rename = "start")]
    Start { t: u64, turn: i32 },
    #[serde(rename = "partial")]
    Partial {
        t: u64,
        #[serde(default)]
        seq: u64,
        text: String,
    },
    #[serde(rename = "complete")]
    Complete {
        t: u64,
        turn: i32,
        #[serde(default)]
        seq: u64,
        text: String,
    },
    #[serde(rename = "trigger")]
    Trigger {
        t: u64,
        kind: String,
        turn: i32,
        #[serde(default)]
        seq: u64,
        chars: usize,
    },
    #[serde(rename = "skip")]
    Skip { t: u64, turn: i32, seq: u64 },
    #[serde(rename = "speech")]
    Speech { t: u64, state: String },
    #[serde(rename = "xlate")]
    Xlate {
        t: u64,
        turn: i32,
        seq: u64,
        first_ms: u64,
        total_ms: u64,
        out: usize,
    },
    #[serde(rename = "xlate_err")]
    XlateErr { t: u64, turn: i32, seq: u64 },
    #[serde(rename = "show")]
    Show {
        t: u64,
        which: String,
        turn: i32,
        seq: u64,
        chars: usize,
    },
    #[serde(rename = "drop")]
    Drop {
        t: u64,
        which: String,
        turn: i32,
        seq: u64,
    },
    #[serde(rename = "session")]
    Session { t: u64 },
}

impl Rec {
    fn t(&self) -> u64 {
        match self {
            Rec::Start { t, .. }
            | Rec::Partial { t, .. }
            | Rec::Complete { t, .. }
            | Rec::Trigger { t, .. }
            | Rec::Xlate { t, .. }
            | Rec::XlateErr { t, .. }
            | Rec::Show { t, .. }
            | Rec::Drop { t, .. }
            | Rec::Skip { t, .. }
            | Rec::Speech { t, .. }
            | Rec::Session { t } => *t,
        }
    }
}

/// ログ全文から記録行を抜き出す。読めない記録行は数えて捨てる
fn parse_log(text: &str) -> (Vec<Rec>, usize) {
    let mut recs = Vec::new();
    let mut skipped = 0;
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("⏱ ") else {
            continue;
        };
        match serde_json::from_str(payload) {
            Ok(r) => recs.push(r),
            Err(_) => skipped += 1,
        }
    }
    recs.sort_by_key(Rec::t);
    (recs, skipped)
}

/// 中央値。空なら None
fn median(xs: &[u64]) -> Option<u64> {
    let mut v = xs.to_vec();
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

/// 上側百分位（0.9＝p90）。空なら None
fn percentile(xs: &[u64], p: f64) -> Option<u64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    let i = ((v.len() as f64 * p).ceil() as usize)
        .saturating_sub(1)
        .min(v.len() - 1);
    Some(v[i])
}

/// 比較する閾値の組
struct TuneConfig {
    name: &'static str,
    growth: usize,
    leading: usize,
    fast: bool,
}

/// 再生結果の集計
#[derive(Default)]
struct ReplayResult {
    reactions_ms: Vec<u64>,
    silent_turns: usize,
    requests: usize,
    updates: usize,
    drops: usize,
}

/// 仮想時刻での翻訳到着
struct Arrival {
    t: u64,
    turn: i32,
    seq: u64,
    /// 途中訳の確定分。再利用判定用に覚える
    req_text: Option<String>,
}

/// 到着を1件さばく。表示・反応・破棄を数える
#[allow(clippy::too_many_arguments)]
fn apply_arrival(
    tail: &mut TranslationTail,
    partials: &mut PartialTracker,
    arr: &Arrival,
    at: Instant,
    turn_start: &BTreeMap<i32, u64>,
    reacted: &mut BTreeSet<i32>,
    out: &mut ReplayResult,
) {
    if let Some(req) = &arr.req_text {
        partials.note_completed(req.clone(), "x".to_string());
    }
    let u = SubtitleUpdate::at("x".to_string(), arr.turn, arr.seq, 0, at);
    if tail.push(&u) {
        out.updates += 1;
        if !reacted.contains(&arr.turn)
            && let Some(t0) = turn_start.get(&arr.turn)
        {
            reacted.insert(arr.turn);
            out.reactions_ms.push(arr.t.saturating_sub(*t0));
        }
    } else {
        out.drops += 1;
    }
}

/// 記録を仮想時刻で再生し、翻訳タイミングを予測する。
/// 判定ロジックは実物を使い、翻訳の所要だけ引数の値で模す
fn replay(recs: &[Rec], cfg: &TuneConfig, first_ms: u64, total_ms: u64) -> ReplayResult {
    let base = Instant::now();
    let vt = |t: u64| base + Duration::from_millis(t);
    let mut turns = TurnTracker::new();
    let mut partials = PartialTracker::new();
    let mut tail = TranslationTail::new();
    let mut next_seq: u64 = 0;
    let mut pending: BTreeMap<u64, Vec<Arrival>> = BTreeMap::new();
    let mut turn_start: BTreeMap<i32, u64> = BTreeMap::new();
    let mut reacted: BTreeSet<i32> = BTreeSet::new();
    let mut out = ReplayResult::default();

    // 仮想時刻 limit までの到着をさばく
    macro_rules! drain {
        ($limit:expr) => {
            while let Some((&t, _)) = pending.first_key_value() {
                if t > $limit {
                    break;
                }
                let (_, arrs) = pending.pop_first().expect("pending entry");
                for arr in arrs {
                    apply_arrival(
                        &mut tail,
                        &mut partials,
                        &arr,
                        vt(arr.t),
                        &turn_start,
                        &mut reacted,
                        &mut out,
                    );
                }
            }
        };
    }

    for rec in recs {
        // セッション切替は状態を捨てる（到着待ちも旧世代として破棄）
        if matches!(rec, Rec::Session { .. }) {
            turns = TurnTracker::new();
            partials = PartialTracker::new();
            tail = TranslationTail::new();
            pending.clear();
            continue;
        }
        drain!(rec.t());
        match rec {
            Rec::Start { t, turn } => {
                if turns.note_speech_start(*turn) {
                    partials.note_turn_end();
                    // 原文窓の消去分だけ seq を進める（翻訳窓は消さない）
                    next_seq += 1;
                    turn_start.insert(*turn, *t);
                }
            }
            Rec::Partial { t, text, .. } => {
                let turn = turns.current();
                if partials
                    .preview_trigger_cfg(text, cfg.fast, vt(*t), cfg.growth, cfg.leading)
                    .is_some()
                {
                    out.requests += 1;
                    partials.note_spawned(text.clone(), vt(*t));
                    let s = next_seq;
                    next_seq += 1;
                    pending.entry(t + first_ms).or_default().push(Arrival {
                        t: t + first_ms,
                        turn,
                        seq: s,
                        req_text: None,
                    });
                    pending.entry(t + total_ms).or_default().push(Arrival {
                        t: t + total_ms,
                        turn,
                        seq: s,
                        req_text: Some(text.clone()),
                    });
                }
            }
            Rec::Complete { t, turn, text, .. } => {
                turns.note_complete(*turn);
                let reused = partials.reuse(text).is_some();
                if *turn == turns.current() {
                    partials.note_turn_end();
                }
                let s = next_seq;
                next_seq += 1;
                if reused {
                    apply_arrival(
                        &mut tail,
                        &mut partials,
                        &Arrival {
                            t: *t,
                            turn: *turn,
                            seq: s,
                            req_text: None,
                        },
                        vt(*t),
                        &turn_start,
                        &mut reacted,
                        &mut out,
                    );
                } else {
                    out.requests += 1;
                    pending.entry(t + first_ms).or_default().push(Arrival {
                        t: t + first_ms,
                        turn: *turn,
                        seq: s,
                        req_text: None,
                    });
                    pending.entry(t + total_ms).or_default().push(Arrival {
                        t: t + total_ms,
                        turn: *turn,
                        seq: s,
                        req_text: None,
                    });
                }
            }
            _ => {}
        }
    }
    drain!(u64::MAX);
    // 反応のなかった有効ターンを数える
    out.silent_turns = turn_start.keys().filter(|t| !reacted.contains(t)).count();
    out
}

/// 記録の表示行から実測の切替反応を求める（予測の答え合わせ用）
fn actual_reactions(recs: &[Rec]) -> Vec<u64> {
    let mut starts: BTreeMap<i32, u64> = BTreeMap::new();
    for rec in recs {
        if let Rec::Start { t, turn } = rec {
            starts.entry(*turn).or_insert(*t);
        }
    }
    let mut reacted: BTreeSet<i32> = BTreeSet::new();
    let mut out = Vec::new();
    for rec in recs {
        if let Rec::Show { t, which, turn, .. } = rec
            && which == "tr"
            && !reacted.contains(turn)
            && let Some(t0) = starts.get(turn)
        {
            reacted.insert(*turn);
            out.push(t.saturating_sub(*t0));
        }
    }
    out
}

fn print_usage() {
    println!(
        "使い方: lume-sub --tune TIMING.LOG [--first-ms N] [--total-ms N]\n\
         \n\
         開発ログの ⏱ 記録行から翻訳タイミングを測り、閾値別の反応を予測する。\n\
         1. アプリの設定で開発ログをONにする\n\
         2. 音声を流した実行の標準出力を保存する（例: cargo run > timing.log）\n\
         3. このコマンドで計測値と予測表を見る\n\
         翻訳の所要は記録の中央値で模す。記録に翻訳がない場合は両方必須。\n\
         \n\
         lume-sub --tune --eval [--first-ms N] [--total-ms N]\n\
         合成条件（4系統の記録）で閾値を比べる。実録なしの既定値選び用。"
    );
}

/// 合成評価の所要の仮定値（ストリーミング想定）。--first-ms/--total-ms で上書き可
const EVAL_FIRST_MS: u64 = 700;
const EVAL_TOTAL_MS: u64 = 1200;

/// 合成評価用の1スクリプト
struct EvalScript {
    name: &'static str,
    recs: Vec<Rec>,
}

/// 1ターンの開始→部分文（step字刻み）→確定を作る
fn turn_script(t0: u64, turn: i32, full: &str, step_ms: u64, step_chars: usize) -> Vec<Rec> {
    let mut recs = vec![Rec::Start { t: t0, turn }];
    let chars: Vec<char> = full.chars().collect();
    let mut len = step_chars.min(chars.len());
    let mut t = t0;
    while len < chars.len() {
        t += step_ms;
        recs.push(Rec::Partial {
            t,
            seq: 0,
            text: chars[..len].iter().collect(),
        });
        len += step_chars;
    }
    t += step_ms;
    recs.push(Rec::Partial {
        t,
        seq: 0,
        text: full.to_string(),
    });
    recs.push(Rec::Complete {
        t: t + 400,
        turn,
        seq: 0,
        text: full.to_string(),
    });
    recs
}

/// 実録が無いときの既定の合成条件。動画で見た4系統
fn eval_scripts() -> Vec<EvalScript> {
    let mut scripts = Vec::new();
    // S1: 句読点なし英語（スペイン語区間のような振る舞い）
    let mut recs = vec![Rec::Session { t: 0 }];
    recs.extend(turn_script(
        100,
        1,
        "Hello world this is live subtitle flow without any punctuation",
        150,
        2,
    ));
    recs.extend(turn_script(
        7000,
        2,
        "Switching languages midstream and translation keeps following",
        150,
        2,
    ));
    scripts.push(EvalScript {
        name: "en-nopunct",
        recs,
    });
    // S2: 句読点つき日本語
    let mut recs = vec![Rec::Session { t: 0 }];
    recs.extend(turn_script(
        100,
        1,
        "こんにちは。私はルメです。リアルタイム翻訳字幕です。",
        150,
        2,
    ));
    recs.extend(turn_script(
        4500,
        2,
        "言語が変わっても。翻訳は追いかけます。",
        150,
        2,
    ));
    scripts.push(EvalScript {
        name: "ja-punct",
        recs,
    });
    // S3: 短い応答の連続（先頭閾値の効き目を見る）
    let mut recs = vec![Rec::Session { t: 0 }];
    let mut t = 100;
    for (i, word) in ["はい", "OK", "好的", "yes"].iter().enumerate() {
        recs.extend(turn_script(t, i as i32 + 1, word, 120, 1));
        t += 1200;
    }
    scripts.push(EvalScript { name: "short", recs });
    // S4: 長い中国語（追補の刻みを見る）
    let mut recs = vec![Rec::Session { t: 0 }];
    recs.extend(turn_script(
        100,
        1,
        &"看视频开会学中文每天坚持练习就会进步".repeat(6),
        150,
        3,
    ));
    scripts.push(EvalScript {
        name: "cjk-long",
        recs,
    });
    scripts
}

/// 合成条件での設定別合計
struct EvalRow {
    name: String,
    baseline: bool,
    growth: usize,
    leading: usize,
    fast: bool,
    reactions_ms: Vec<u64>,
    silent: usize,
    requests: usize,
    updates: usize,
    drops: usize,
}

/// 全スクリプト×全候補を再生して合計する
fn eval_totals(first_ms: u64, total_ms: u64) -> Vec<EvalRow> {
    let mut cfgs: Vec<(usize, usize, bool)> = Vec::new();
    for growth in [10, 20, 30] {
        for leading in [1, 2, 3] {
            cfgs.push((growth, leading, false));
        }
    }
    cfgs.push((PREVIEW_GROWTH_CHARS, LEADING_MIN_CHARS, true));
    let scripts = eval_scripts();
    let mut rows = Vec::new();
    for (growth, leading, fast) in cfgs {
        let cfg = TuneConfig {
            name: "",
            growth,
            leading,
            fast,
        };
        let baseline =
            growth == PREVIEW_GROWTH_CHARS && leading == LEADING_MIN_CHARS && !fast;
        let mut row = EvalRow {
            name: if fast {
                "高速".to_string()
            } else {
                format!("成長{growth}+先頭{leading}")
            },
            baseline,
            growth,
            leading,
            fast,
            reactions_ms: Vec::new(),
            silent: 0,
            requests: 0,
            updates: 0,
            drops: 0,
        };
        for script in &scripts {
            let r = replay(&script.recs, &cfg, first_ms, total_ms);
            row.reactions_ms.extend(r.reactions_ms);
            row.silent += r.silent_turns;
            row.requests += r.requests;
            row.updates += r.updates;
            row.drops += r.drops;
        }
        rows.push(row);
    }
    rows
}

/// 合成条件での比較表を出す
fn run_eval(first_ms: u64, total_ms: u64) {
    println!("合成評価（first={first_ms}ms total={total_ms}ms の仮定、4スクリプト合計）:");
    println!(
        "対象: {}",
        eval_scripts()
            .iter()
            .map(|s| s.name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("設定           反応平均  反応p90  無反応  要求  表示  破棄");
    let rows = eval_totals(first_ms, total_ms);
    let mean = |r: &EvalRow| {
        if r.reactions_ms.is_empty() {
            u64::MAX
        } else {
            r.reactions_ms.iter().sum::<u64>() / r.reactions_ms.len() as u64
        }
    };
    for r in &rows {
        println!(
            "{:<14} {:>6}ms {:>6}ms {:>6} {:>6} {:>6} {:>6}{}",
            r.name,
            if r.reactions_ms.is_empty() {
                0
            } else {
                mean(r)
            },
            percentile(&r.reactions_ms, 0.9).unwrap_or(0),
            r.silent,
            r.requests,
            r.updates,
            r.drops,
            if r.baseline { " ←現在値" } else { "" },
        );
    }
    if let Some(best) = rows.iter().min_by_key(|r| (r.silent, mean(r))) {
        if best.baseline {
            println!("判定: 現在値が最良");
        } else {
            println!(
                "判定: {} が最良（成長={}字・先頭={}字・高速={}）",
                best.name, best.growth, best.leading, best.fast
            );
        }
    }
}

/// 記録の計測と閾値別の予測を標準出力に並べる
pub(crate) fn run(args: &[String]) {
    let mut file: Option<&str> = None;
    let mut first_ms: Option<u64> = None;
    let mut total_ms: Option<u64> = None;
    let mut eval_mode = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--eval" => eval_mode = true,
            "--first-ms" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<u64>().ok()) {
                    Some(v) => first_ms = Some(v),
                    None => {
                        eprintln!("--first-ms には数値が必要");
                        print_usage();
                        return;
                    }
                }
            }
            "--total-ms" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<u64>().ok()) {
                    Some(v) => total_ms = Some(v),
                    None => {
                        eprintln!("--total-ms には数値が必要");
                        print_usage();
                        return;
                    }
                }
            }
            "-h" | "--help" => {
                print_usage();
                return;
            }
            s if s.starts_with('-') => {
                eprintln!("不明な引数: {s}");
                print_usage();
                return;
            }
            s => {
                if file.is_some() {
                    eprintln!("ファイルは1つだけ指定");
                    print_usage();
                    return;
                }
                file = Some(s);
            }
        }
        i += 1;
    }
    if eval_mode {
        if file.is_some() {
            eprintln!("ファイルと--evalは同時指定不可");
            print_usage();
            return;
        }
        run_eval(
            first_ms.unwrap_or(EVAL_FIRST_MS),
            total_ms.unwrap_or(EVAL_TOTAL_MS),
        );
        return;
    }
    let Some(file) = file else {
        print_usage();
        return;
    };
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("読めません（{file}）: {e}");
            return;
        }
    };
    let (recs, skipped) = parse_log(&text);
    let firsts: Vec<u64> = recs
        .iter()
        .filter_map(|r| match r {
            Rec::Xlate { first_ms, .. } => Some(*first_ms),
            _ => None,
        })
        .collect();
    let totals: Vec<u64> = recs
        .iter()
        .filter_map(|r| match r {
            Rec::Xlate { total_ms, .. } => Some(*total_ms),
            _ => None,
        })
        .collect();
    let errs = recs.iter().filter(|r| matches!(r, Rec::XlateErr { .. })).count();
    let live_triggers = recs.iter().filter(|r| matches!(r, Rec::Trigger { .. })).count();
    let live_drops = recs.iter().filter(|r| matches!(r, Rec::Drop { .. })).count();
    let first_ms = match (first_ms, median(&firsts)) {
        (Some(v), _) | (None, Some(v)) => v,
        (None, None) => {
            eprintln!("翻訳の記録がありません。--first-ms/--total-ms を指定してください");
            return;
        }
    };
    let total_ms = match (total_ms, median(&totals)) {
        (Some(v), _) | (None, Some(v)) => v,
        (None, None) => {
            eprintln!("翻訳の記録がありません。--first-ms/--total-ms を指定してください");
            return;
        }
    };
    println!("記録: {}行（{}行スキップ）", recs.len(), skipped);
    println!(
        "翻訳: {}件 first中央値={}ms p90={}ms / total中央値={}ms p90={}ms / 失敗{}件",
        firsts.len(),
        median(&firsts).unwrap_or(0),
        percentile(&firsts, 0.9).unwrap_or(0),
        median(&totals).unwrap_or(0),
        percentile(&totals, 0.9).unwrap_or(0),
        errs
    );
    let actual = actual_reactions(&recs);
    println!("実測: トリガ{live_triggers}件・表示破棄{live_drops}件");
    if !actual.is_empty() {
        println!(
            "実測の切替反応: {}ターン 平均={}ms p90={}ms",
            actual.len(),
            actual.iter().sum::<u64>() / actual.len() as u64,
            percentile(&actual, 0.9).unwrap_or(0)
        );
    }
    let configs = [
        TuneConfig {
            name: "現在値",
            growth: PREVIEW_GROWTH_CHARS,
            leading: LEADING_MIN_CHARS,
            fast: false,
        },
        TuneConfig {
            name: "成長5",
            growth: 5,
            leading: LEADING_MIN_CHARS,
            fast: false,
        },
        TuneConfig {
            name: "成長20",
            growth: 20,
            leading: LEADING_MIN_CHARS,
            fast: false,
        },
        TuneConfig {
            name: "先頭1",
            growth: PREVIEW_GROWTH_CHARS,
            leading: 1,
            fast: false,
        },
        TuneConfig {
            name: "成長5+先頭1",
            growth: 5,
            leading: 1,
            fast: false,
        },
        TuneConfig {
            name: "高速",
            growth: PREVIEW_GROWTH_CHARS,
            leading: LEADING_MIN_CHARS,
            fast: true,
        },
    ];
    println!("予測（first={first_ms}ms total={total_ms}ms として再生）:");
    println!("設定           反応平均  反応p90  無反応  要求  表示  破棄");
    let mut results: Vec<(&TuneConfig, ReplayResult)> = Vec::new();
    for cfg in &configs {
        let r = replay(&recs, cfg, first_ms, total_ms);
        let mean = if r.reactions_ms.is_empty() {
            0
        } else {
            r.reactions_ms.iter().sum::<u64>() / r.reactions_ms.len() as u64
        };
        println!(
            "{:<14} {:>6}ms {:>6}ms {:>6} {:>6} {:>6} {:>6}",
            cfg.name,
            mean,
            percentile(&r.reactions_ms, 0.9).unwrap_or(0),
            r.silent_turns,
            r.requests,
            r.updates,
            r.drops
        );
        results.push((cfg, r));
    }
    let mean = |r: &ReplayResult| {
        if r.reactions_ms.is_empty() {
            u64::MAX
        } else {
            r.reactions_ms.iter().sum::<u64>() / r.reactions_ms.len() as u64
        }
    };
    if let Some((best, _)) = results
        .iter()
        .min_by_key(|(_, r)| (r.silent_turns, mean(r)))
    {
        if best.name == "現在値" {
            println!("判定: 現在値が最良");
        } else {
            println!(
                "判定: {} が最良（成長={}字・先頭={}字・高速={}）",
                best.name, best.growth, best.leading, best.fast
            );
        }
    }
}

// ---------- カバレッジ ----------

/// 台本1言語分。demo/script.json の1要素（id・text だけ読む）
#[derive(Debug, Deserialize)]
struct ScriptSeg {
    id: String,
    text: String,
}

/// 結合文字（発音記号など）か。is_alphanumeric は一部を通すため明示的に除く
fn is_combining_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036f}'
        | '\u{1ab0}'..='\u{1aff}'
        | '\u{1dc0}'..='\u{1dff}'
        | '\u{20d0}'..='\u{20ff}'
        | '\u{fe20}'..='\u{fe2f}'
        | '\u{0591}'..='\u{05bd}' | '\u{05bf}' | '\u{05c1}'..='\u{05c2}'
        | '\u{05c4}'..='\u{05c5}' | '\u{05c7}'
        | '\u{0610}'..='\u{061a}' | '\u{064b}'..='\u{065f}' | '\u{0670}'
        | '\u{06d6}'..='\u{06dc}' | '\u{06df}'..='\u{06e4}'
        | '\u{06e7}'..='\u{06e8}' | '\u{06ea}'..='\u{06ed}')
}

/// 照合用の正規化。小文字化して英数字以外を落とす
/// （空白・句読点・アラビア語の発音記号などの差を吸収する）
fn normalize_sentence(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() && !is_combining_mark(*c))
        .collect()
}

/// 台本1文の照合結果
struct SentenceHit {
    seg: String,
    /// 言語内の文番号（1始まり）
    index: usize,
    text: String,
    /// 含んでいた確定の (セッション, ターン)。途中のみなら途中文の帰属
    turn: Option<(u32, i32)>,
    partial_only: bool,
}

/// 発話区間と認識の突き合わせ結果
struct UtteranceHit {
    index: usize,
    start_t: u64,
    end_t: u64,
    /// 開始で区切れた（再接続またぎ等）
    split: bool,
    turns: Vec<(u32, i32)>,
    complete: bool,
    first_partial_ms: Option<u64>,
}

/// 認識文と翻訳・表示の突き合わせ結果
struct SentenceRow {
    text: String,
    turn: (u32, i32),
    /// "確定" or "途中"
    kind: &'static str,
    translated: bool,
    shown: bool,
}

/// カバレッジの集計結果
struct CoverageReport {
    script_hits: Vec<SentenceHit>,
    turns: BTreeMap<(u32, i32), TurnCoverage>,
    utterances: Vec<UtteranceHit>,
    sentences: Vec<SentenceRow>,
}

/// 要求文の置き場キー＝(セッション, ターン, seq)
type ReqKey = (u32, i32, u64);

/// 確定の遅れ余裕。発話終了後に届く確定を同一区間に寄せる
const COMPLETE_LAG_MS: u64 = 2500;

/// ターン別の翻訳・表示の集計
#[derive(Default)]
struct TurnCoverage {
    partials: u32,
    complete: bool,
    start_t: Option<u64>,
    first_partial_t: Option<u64>,
    xlate_ok: u32,
    xlate_err: u32,
    shown_tr: u32,
    dropped_tr: u32,
}

/// 記録から発話・認識・翻訳・表示の突き合わせを求める。
/// 途中分のターン帰属は実物と同じ TurnTracker で付ける
fn cover(recs: &[Rec], script: &[ScriptSeg]) -> CoverageReport {
    let mut session: u32 = 0;
    let mut started = false;
    let mut turns = TurnTracker::new();
    let mut partials: Vec<((u32, i32), String)> = Vec::new();
    let mut completes: Vec<((u32, i32), String)> = Vec::new();
    let mut partials_t: Vec<(u64, (u32, i32))> = Vec::new();
    let mut completes_t: Vec<(u64, (u32, i32))> = Vec::new();
    let mut reqs: Vec<(ReqKey, String)> = Vec::new();
    let mut xlate_ok: BTreeSet<ReqKey> = BTreeSet::new();
    let mut skipped: BTreeSet<ReqKey> = BTreeSet::new();
    let mut shown: BTreeSet<ReqKey> = BTreeSet::new();
    let mut speech: Vec<(u64, bool)> = Vec::new();
    let mut complete_text: BTreeMap<(u32, i32), String> = BTreeMap::new();
    let mut last_partial: BTreeMap<(u32, i32), String> = BTreeMap::new();
    let mut cov: BTreeMap<(u32, i32), TurnCoverage> = BTreeMap::new();
    for rec in recs {
        match rec {
            Rec::Session { .. } => {
                // 初回はセッション0、2回目以降を再接続として数える
                if started {
                    session += 1;
                }
                started = true;
                turns = TurnTracker::new();
            }
            Rec::Start { t, turn } => {
                if turns.note_speech_start(*turn) {
                    cov.entry((session, *turn)).or_default().start_t = Some(*t);
                }
            }
            Rec::Partial { t, seq, text } => {
                let key = (session, turns.current());
                let c = cov.entry(key).or_default();
                c.partials += 1;
                if c.first_partial_t.is_none() {
                    c.first_partial_t = Some(*t);
                }
                partials.push((key, normalize_sentence(text)));
                partials_t.push((*t, key));
                reqs.push(((session, key.1, *seq), normalize_sentence(text)));
                last_partial.insert(key, text.clone());
            }
            Rec::Complete { t, turn, seq, text } => {
                turns.note_complete(*turn);
                cov.entry((session, *turn)).or_default().complete = true;
                completes.push(((session, *turn), normalize_sentence(text)));
                completes_t.push((*t, (session, *turn)));
                reqs.push(((session, *turn, *seq), normalize_sentence(text)));
                complete_text
                    .entry((session, *turn))
                    .or_insert_with(|| text.clone());
            }
            Rec::Xlate { turn, seq, .. } => {
                cov.entry((session, *turn)).or_default().xlate_ok += 1;
                xlate_ok.insert((session, *turn, *seq));
            }
            Rec::XlateErr { turn, .. } => {
                cov.entry((session, *turn)).or_default().xlate_err += 1;
            }
            Rec::Skip { turn, seq, .. } => {
                skipped.insert((session, *turn, *seq));
            }
            Rec::Show {
                which, turn, seq, ..
            } if which == "tr" => {
                cov.entry((session, *turn)).or_default().shown_tr += 1;
                shown.insert((session, *turn, *seq));
            }
            Rec::Drop { which, turn, .. } if which == "tr" => {
                cov.entry((session, *turn)).or_default().dropped_tr += 1;
            }
            Rec::Speech { t, state } => {
                speech.push((*t, state == "start"));
            }
            _ => {}
        }
    }
    // 台本の照合
    let mut hits = Vec::new();
    for seg in script {
        for (i, sent) in split_sentences(&seg.text).iter().enumerate() {
            let norm = normalize_sentence(sent);
            if norm.is_empty() {
                continue;
            }
            let found = completes.iter().find(|(_, c)| c.contains(norm.as_str()));
            let (turn, partial_only) = match found {
                Some((key, _)) => (Some(*key), false),
                None => match partials.iter().find(|(_, p)| p.contains(norm.as_str())) {
                    Some((key, _)) => (Some(*key), true),
                    None => (None, false),
                },
            };
            hits.push(SentenceHit {
                seg: seg.id.clone(),
                index: i + 1,
                text: sent.to_string(),
                turn,
                partial_only,
            });
        }
    }
    // 発話区間の認識突き合わせ。開始の重なり・閉じ忘れに寛容に組む
    let last_t = recs.iter().map(Rec::t).max().unwrap_or(0);
    let mut utterances = Vec::new();
    let mut open: Option<u64> = None;
    let flush = |start: u64, end: u64, split: bool, out: &mut Vec<UtteranceHit>| {
        // 区切り直しの終わりは人為的なので遅れ余裕を付けない
        let hi = if split {
            end
        } else {
            end.saturating_add(COMPLETE_LAG_MS)
        };
        let mut turns_hit: Vec<(u32, i32)> = Vec::new();
        let mut complete = false;
        let mut first: Option<u64> = None;
        for (t, key) in partials_t.iter().chain(completes_t.iter()) {
            if *t < start || *t > hi {
                continue;
            }
            if !turns_hit.contains(key) {
                turns_hit.push(*key);
            }
        }
        for (t, _) in completes_t.iter() {
            if *t >= start && *t <= hi {
                complete = true;
            }
        }
        for (t, _) in partials_t.iter() {
            if *t >= start && *t <= hi {
                first = Some(first.map_or(t - start, |f: u64| f.min(t - start)));
            }
        }
        turns_hit.sort();
        out.push(UtteranceHit {
            index: out.len() + 1,
            start_t: start,
            end_t: end,
            split,
            turns: turns_hit,
            complete,
            first_partial_ms: first,
        });
    };
    for (t, speaking) in speech {
        if speaking {
            if let Some(start) = open.take() {
                flush(start, t, true, &mut utterances);
            }
            open = Some(t);
        } else if let Some(start) = open.take() {
            flush(start, t, false, &mut utterances);
        }
    }
    if let Some(start) = open.take() {
        flush(start, last_t, false, &mut utterances);
    }
    // 認識文の翻訳・表示突き合わせ。確定のあるターンは確定から、
    // なければ最終途中から文を拾い、同一ターンの要求で連結する
    let mut turn_keys: Vec<(u32, i32)> = complete_text.keys().copied().collect();
    for key in last_partial.keys() {
        if !turn_keys.contains(key) {
            turn_keys.push(*key);
        }
    }
    turn_keys.sort();
    let mut sentences = Vec::new();
    for key in turn_keys {
        let (raw, kind) = match complete_text.get(&key) {
            Some(t) => (t.clone(), "確定"),
            None => match last_partial.get(&key) {
                Some(t) => (t.clone(), "途中"),
                None => continue,
            },
        };
        for sent in split_sentences(&raw) {
            let norm = normalize_sentence(sent);
            if norm.is_empty() {
                continue;
            }
            let in_turn = |k: &ReqKey| k.0 == key.0 && k.1 == key.1;
            let translated = reqs.iter().any(|(k, txt)| {
                in_turn(k)
                    && txt.contains(norm.as_str())
                    && (xlate_ok.contains(k) || skipped.contains(k))
            });
            let shown_now = reqs
                .iter()
                .any(|(k, txt)| in_turn(k) && txt.contains(norm.as_str()) && shown.contains(k));
            sentences.push(SentenceRow {
                text: sent.to_string(),
                turn: key,
                kind,
                translated,
                shown: shown_now,
            });
        }
    }
    CoverageReport {
        script_hits: hits,
        turns: cov,
        utterances,
        sentences,
    }
}

/// 表示用に文を丸める
fn short(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        text.to_string()
    } else {
        chars[..max_chars].iter().collect::<String>() + "…"
    }
}

fn turn_label(key: (u32, i32)) -> String {
    if key.0 == 0 {
        format!("turn={}", key.1)
    } else {
        format!("turn={}(再接続{})", key.1, key.0)
    }
}

fn print_coverage(report: &CoverageReport, with_script: bool) {
    println!("発話→認識（音声{}区間）:", report.utterances.len());
    for u in &report.utterances {
        let mark = if u.turns.is_empty() { "×" } else { "○" };
        let turns = u
            .turns
            .iter()
            .map(|k| turn_label(*k))
            .collect::<Vec<_>>()
            .join(",");
        let first = u
            .first_partial_ms
            .map(|ms| format!(" 初回{:.1}s", ms as f64 / 1000.0))
            .unwrap_or_default();
        let split = if u.split { "（区切り直し）" } else { "" };
        println!(
            "#{} {:.1}–{:.1}s {} {}{}{}",
            u.index,
            u.start_t as f64 / 1000.0,
            u.end_t as f64 / 1000.0,
            mark,
            if turns.is_empty() {
                "-".to_string()
            } else {
                format!("{turns} 確定{}", if u.complete { "あり" } else { "なし" })
            },
            first,
            split,
        );
    }
    let heard = report
        .utterances
        .iter()
        .filter(|u| !u.turns.is_empty())
        .count();
    println!("音声の認識: {heard}/{}区間", report.utterances.len());
    println!("原文→翻訳・表示（認識{}文）:", report.sentences.len());
    for s in &report.sentences {
        println!(
            "[{} {}] {}訳 {}表 {}",
            turn_label(s.turn),
            s.kind,
            if s.translated { "○" } else { "×" },
            if s.shown { "○" } else { "×" },
            short(&s.text, 40),
        );
    }
    let tr = report.sentences.iter().filter(|s| s.translated).count();
    let sh = report.sentences.iter().filter(|s| s.shown).count();
    println!(
        "翻訳: {tr}/{}文、表示: {sh}/{}文",
        report.sentences.len(),
        report.sentences.len()
    );
    if with_script {
        println!("認識カバレッジ（台本{}文）:", report.script_hits.len());
        for h in &report.script_hits {
            let mark = match (h.turn, h.partial_only) {
                (Some(_), false) => "○",
                (Some(_), true) => "△",
                (None, _) => "×",
            };
            let turn = h.turn.map(turn_label).unwrap_or_else(|| "-".to_string());
            println!(
                "[{} #{}] {} {} {}",
                h.seg,
                h.index,
                mark,
                turn,
                short(&h.text, 40)
            );
        }
        let ok = report
            .script_hits
            .iter()
            .filter(|h| h.turn.is_some() && !h.partial_only)
            .count();
        let tri = report
            .script_hits
            .iter()
            .filter(|h| h.turn.is_some() && h.partial_only)
            .count();
        println!(
            "認識: {ok}/{}文（途中のみ{tri}文）",
            report.script_hits.len()
        );
    }
    println!("翻訳・表示（ターン別）:");
    for (key, c) in &report.turns {
        let first = match (c.start_t, c.first_partial_t) {
            (Some(a), Some(b)) => format!("{}ms", b.saturating_sub(a)),
            _ => "-".to_string(),
        };
        println!(
            "{} partials={} complete={} xlate ok={}/err={} shown={} dropped={} 初回{}",
            turn_label(*key),
            c.partials,
            if c.complete { "○" } else { "×" },
            c.xlate_ok,
            c.xlate_err,
            c.shown_tr,
            c.dropped_tr,
            first,
        );
    }
    let missing: Vec<&SentenceHit> = report
        .script_hits
        .iter()
        .filter(|h| h.turn.is_none())
        .collect();
    if !missing.is_empty() {
        println!("未認識:");
        for h in missing {
            println!("  [{} #{}] {}", h.seg, h.index, h.text);
        }
    }
    for u in report.utterances.iter().filter(|u| u.turns.is_empty()) {
        println!(
            "注意 音声#{:.1}–{:.1}s: 音声あり・認識なし",
            u.start_t as f64 / 1000.0,
            u.end_t as f64 / 1000.0
        );
    }
    for s in report.sentences.iter().filter(|s| !s.translated) {
        println!(
            "注意 {}: 未翻訳「{}」",
            turn_label(s.turn),
            short(&s.text, 30)
        );
    }
    for s in report.sentences.iter().filter(|s| s.translated && !s.shown) {
        println!(
            "注意 {}: 翻訳あり・未表示「{}」",
            turn_label(s.turn),
            short(&s.text, 30)
        );
    }
    for (key, c) in &report.turns {
        if c.dropped_tr > 0 {
            println!("注意 {}: 破棄あり（救済されず）", turn_label(*key));
        }
        if c.xlate_ok > 0 && c.shown_tr == 0 {
            println!("注意 {}: 翻訳あり・未表示", turn_label(*key));
        }
        if c.complete && c.xlate_ok == 0 && c.xlate_err == 0 && c.shown_tr == 0 {
            println!("注意 {}: 確定あり・翻訳なし", turn_label(*key));
        }
    }
}

/// 台本と記録を照合してカバレッジを出す。
/// `cargo run -- --coverage TIMING.LOG [SCRIPT.JSON]`
pub(crate) fn coverage(args: &[String]) {
    if args.is_empty() || args.len() > 2 {
        println!(
            "使い方: lume-sub --coverage TIMING.LOG [SCRIPT.JSON]\n\
             \n\
             すべての音声が原文として受け付けられ、すべての原文が翻訳・表示\n\
             されたかを照合する。台本（demo/script.json 形式）があれば\n\
             台本の各文の認識も照合する。○＝あり △＝途中のみ ×＝なし。"
        );
        return;
    }
    let log = match std::fs::read_to_string(&args[0]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("記録が読めない: {}", args[0]);
            eprintln!("{e}");
            return;
        }
    };
    let mut script = Vec::new();
    if args.len() == 2 {
        let script_text = match std::fs::read_to_string(&args[1]) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("台本が読めない: {}", args[1]);
                eprintln!("{e}");
                return;
            }
        };
        script = match serde_json::from_str(&script_text) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("台本のJSONが読めない: {e}");
                return;
            }
        };
    }
    let (recs, skipped) = parse_log(&log);
    if skipped > 0 {
        println!("（読めない記録行を{skipped}行捨てた）");
    }
    let report = cover(&recs, &script);
    print_coverage(&report, args.len() == 2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timing_lines_and_skips_noise() {
        let log = "普通の開発ログ\n\
             ⏱ {\"t\":10,\"ev\":\"start\",\"turn\":1}\n\
             ⏱ {\"t\":20,\"ev\":\"partial\",\"text\":\"あ\\\"い\"}\n\
             ⏱ 壊れた行\n\
             ⏱ {\"t\":30,\"ev\":\"nope\"}\n";
        let (recs, skipped) = parse_log(log);
        assert_eq!(recs.len(), 2);
        assert_eq!(skipped, 2);
        assert!(matches!(recs[0], Rec::Start { t: 10, turn: 1 }));
        match &recs[1] {
            // seq のない旧記録は 0 扱いで読める
            Rec::Partial {
                t: 20,
                seq: 0,
                text,
            } => assert_eq!(text, "あ\"い"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn median_and_percentile() {
        assert_eq!(median(&[],), None);
        assert_eq!(median(&[5],), Some(5));
        assert_eq!(median(&[1, 2, 3],), Some(2));
        assert_eq!(median(&[3, 1, 2],), Some(2));
        assert_eq!(percentile(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.9), Some(9));
        assert_eq!(percentile(&[], 0.9), None);
    }

    /// 合成の2ターン記録。句読点なし・各ターンは開始→部分文→確定
    fn script() -> Vec<Rec> {
        let mut recs = vec![Rec::Session { t: 0 }, Rec::Start { t: 10, turn: 1 }];
        for (i, len) in (1..=10).enumerate() {
            recs.push(Rec::Partial {
                t: 100 + i as u64 * 100,
                seq: 0,
                text: "あ".repeat(len),
            });
        }
        recs.push(Rec::Complete {
            t: 1200,
            turn: 1,
            seq: 0,
            text: "あ".repeat(10),
        });
        recs.push(Rec::Start { t: 2000, turn: 2 });
        for (i, len) in (1..=6).enumerate() {
            recs.push(Rec::Partial {
                t: 2100 + i as u64 * 100,
                seq: 0,
                text: "い".repeat(len),
            });
        }
        recs.push(Rec::Complete {
            t: 2800,
            turn: 2,
            seq: 0,
            text: "い".repeat(6),
        });
        recs
    }

    #[test]
    fn replay_counts_requests_and_reactions() {
        let cfg = TuneConfig {
            name: "t",
            growth: 20,
            leading: 3,
            fast: false,
        };
        let r = replay(&script(), &cfg, 200, 600);
        // 各ターン冒頭1回＋確定1回の要求、反応は開始→初回表示
        assert_eq!(r.requests, 4);
        assert_eq!(r.reactions_ms, vec![490, 500]);
        assert_eq!(r.updates, 8);
        assert_eq!(r.drops, 0);
        assert_eq!(r.silent_turns, 0);
    }

    #[test]
    fn replay_rescues_late_previous_turn_arrival() {
        let cfg = TuneConfig {
            name: "t",
            growth: 20,
            leading: 3,
            fast: false,
        };
        // ターン1の確定分の到着がターン2の初回表示より遅い重なり
        let recs = vec![
            Rec::Session { t: 0 },
            Rec::Start { t: 10, turn: 1 },
            Rec::Partial {
                t: 100,
                seq: 0,
                text: "あああ".into(),
            },
            Rec::Complete {
                t: 200,
                turn: 1,
                seq: 0,
                text: "あああ".into(),
            },
            Rec::Start { t: 300, turn: 2 },
            Rec::Partial {
                t: 400,
                seq: 0,
                text: "いいい".into(),
            },
        ];
        let r = replay(&recs, &cfg, 200, 600);
        assert_eq!(r.requests, 3);
        assert_eq!(r.reactions_ms, vec![290, 300]);
        // 旧ターンの確定分の遅延到着は前ターン枠に救済される。破棄は確定に
        // 追い越された途中分の1件だけで、旧方式（消去＋破棄）の2件より少ない
        assert_eq!(r.updates, 5);
        assert_eq!(r.drops, 1);
        assert_eq!(r.silent_turns, 0);
    }

    #[test]
    fn smaller_growth_fires_more_requests() {
        let small = TuneConfig {
            name: "s",
            growth: 5,
            leading: 3,
            fast: false,
        };
        let large = TuneConfig {
            name: "l",
            growth: 40,
            leading: 3,
            fast: false,
        };
        let a = replay(&script(), &small, 200, 600);
        let b = replay(&script(), &large, 200, 600);
        assert!(a.requests > b.requests, "{} vs {}", a.requests, b.requests);
    }

    #[test]
    fn eval_scripts_are_ordered_and_complete() {
        let scripts = eval_scripts();
        assert_eq!(scripts.len(), 4);
        for s in &scripts {
            assert!(s.recs.iter().any(|r| matches!(r, Rec::Start { .. })), "{}", s.name);
            assert!(
                s.recs.iter().any(|r| matches!(r, Rec::Complete { .. })),
                "{}",
                s.name
            );
            let mut last = 0;
            for r in &s.recs {
                assert!(r.t() >= last, "{}: {}", s.name, r.t());
                last = r.t();
            }
        }
    }

    #[test]
    fn eval_totals_are_deterministic() {
        let a = eval_totals(700, 1200);
        let b = eval_totals(700, 1200);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.reactions_ms, y.reactions_ms, "{}", x.name);
            assert_eq!(x.requests, y.requests, "{}", x.name);
            assert_eq!(x.silent, y.silent, "{}", x.name);
        }
    }

    #[test]
    fn lower_leading_floor_reacts_earlier_on_short_turns() {
        let short = eval_scripts().into_iter().find(|s| s.name == "short").unwrap();
        let replay_with = |leading| {
            replay(
                &short.recs,
                &TuneConfig {
                    name: "",
                    growth: 20,
                    leading,
                    fast: false,
                },
                200,
                600,
            )
        };
        let mean = |r: &ReplayResult| r.reactions_ms.iter().sum::<u64>() / r.reactions_ms.len() as u64;
        // 2字応答は先頭1でターン中に反応し、先頭3では確定待ちになる
        assert!(mean(&replay_with(1)) < mean(&replay_with(3)));
    }

    #[test]
    fn actual_reactions_uses_first_tr_show_per_turn() {
        let recs = vec![
            Rec::Start { t: 100, turn: 1 },
            Rec::Show {
                t: 150,
                which: "src".to_string(),
                turn: 1,
                seq: 1,
                chars: 3,
            },
            Rec::Show {
                t: 400,
                which: "tr".to_string(),
                turn: 1,
                seq: 2,
                chars: 5,
            },
            Rec::Show {
                t: 900,
                which: "tr".to_string(),
                turn: 1,
                seq: 3,
                chars: 9,
            },
        ];
        assert_eq!(actual_reactions(&recs), vec![300]);
    }

    #[test]
    fn normalize_sentence_drops_case_and_marks() {
        assert_eq!(normalize_sentence("Hello, World!"), "helloworld");
        assert_eq!(normalize_sentence("こんにちは。私は"), "こんにちは私は");
        // 結合文字（アラビア語の発音記号など）は落とす
        assert_eq!(normalize_sentence("áb"), "ab");
        assert_eq!(normalize_sentence("سَب"), "سب");
    }

    #[test]
    fn cover_matches_script_and_counts_turns() {
        let script = vec![
            ScriptSeg {
                id: "ja".to_string(),
                text: "こんにちは。私はルメです。".to_string(),
            },
            ScriptSeg {
                id: "es".to_string(),
                text: "Hola. Adios.".to_string(),
            },
        ];
        let recs = vec![
            Rec::Session { t: 0 },
            Rec::Speech {
                t: 0,
                state: "start".to_string(),
            },
            Rec::Start { t: 100, turn: 1 },
            Rec::Partial {
                t: 300,
                seq: 1,
                text: "こんにちは".to_string(),
            },
            Rec::Partial {
                t: 500,
                seq: 2,
                text: "こんにちは。私はルメです".to_string(),
            },
            Rec::Complete {
                t: 900,
                turn: 1,
                seq: 3,
                text: "こんにちは。私はルメです。".to_string(),
            },
            Rec::Speech {
                t: 1500,
                state: "end".to_string(),
            },
            Rec::Xlate {
                t: 1200,
                turn: 1,
                seq: 3,
                first_ms: 200,
                total_ms: 300,
                out: 10,
            },
            Rec::Show {
                t: 1210,
                which: "tr".to_string(),
                turn: 1,
                seq: 3,
                chars: 10,
            },
            Rec::Speech {
                t: 5000,
                state: "start".to_string(),
            },
            Rec::Start { t: 5000, turn: 2 },
            Rec::Partial {
                t: 5300,
                seq: 4,
                text: "Hola".to_string(),
            },
            Rec::Speech {
                t: 5900,
                state: "end".to_string(),
            },
            Rec::XlateErr {
                t: 5500,
                turn: 2,
                seq: 4,
            },
            Rec::Drop {
                t: 5600,
                which: "tr".to_string(),
                turn: 2,
                seq: 4,
            },
        ];
        let report = cover(&recs, &script);
        let hits = &report.script_hits;
        // 台本4文の照合
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].turn, Some((0, 1)));
        assert!(!hits[0].partial_only);
        assert_eq!(hits[1].turn, Some((0, 1)));
        // Hola は途中のみ、Adios は未認識
        assert_eq!(hits[2].turn, Some((0, 2)));
        assert!(hits[2].partial_only);
        assert_eq!(hits[3].turn, None);
        // ターン別集計
        let t1 = &report.turns[&(0, 1)];
        assert_eq!(t1.partials, 2);
        assert!(t1.complete);
        assert_eq!(t1.xlate_ok, 1);
        assert_eq!(t1.shown_tr, 1);
        assert_eq!(t1.first_partial_t, Some(300));
        let t2 = &report.turns[&(0, 2)];
        assert_eq!(t2.partials, 1);
        assert!(!t2.complete);
        assert_eq!(t2.xlate_err, 1);
        assert_eq!(t2.dropped_tr, 1);
        // 発話区間の突き合わせ
        assert_eq!(report.utterances.len(), 2);
        let u1 = &report.utterances[0];
        assert_eq!((u1.start_t, u1.end_t), (0, 1500));
        assert_eq!(u1.turns, vec![(0, 1)]);
        assert!(u1.complete);
        assert_eq!(u1.first_partial_ms, Some(300));
        let u2 = &report.utterances[1];
        assert_eq!(u2.turns, vec![(0, 2)]);
        assert!(!u2.complete);
        // 文単位の翻訳・表示突き合わせ
        assert_eq!(report.sentences.len(), 3);
        assert!(report.sentences[0].translated && report.sentences[0].shown);
        assert!(report.sentences[1].translated && report.sentences[1].shown);
        assert_eq!(report.sentences[2].turn, (0, 2));
        assert!(!report.sentences[2].translated && !report.sentences[2].shown);
    }

    #[test]
    fn cover_splits_reconnected_sessions() {
        let script = vec![ScriptSeg {
            id: "ja".to_string(),
            text: "あ。".to_string(),
        }];
        let recs = vec![
            Rec::Session { t: 0 },
            Rec::Start { t: 100, turn: 1 },
            Rec::Complete {
                t: 200,
                turn: 1,
                seq: 1,
                text: "あ。".to_string(),
            },
            // 再接続でターン番号が振り直される
            Rec::Session { t: 300 },
            Rec::Start { t: 400, turn: 1 },
            Rec::Partial {
                t: 500,
                seq: 2,
                text: "あ".to_string(),
            },
        ];
        let report = cover(&recs, &script);
        assert_eq!(report.script_hits[0].turn, Some((0, 1)));
        assert!(report.turns.contains_key(&(0, 1)));
        assert!(report.turns.contains_key(&(1, 1)));
        assert_eq!(report.turns[&(1, 1)].partials, 1);
        assert!(!report.turns[&(1, 1)].complete);
    }

    #[test]
    fn cover_links_sentences_through_seq() {
        let script = vec![];
        let xlate = |t: u64, turn: i32, seq: u64| Rec::Xlate {
            t,
            turn,
            seq,
            first_ms: 10,
            total_ms: 20,
            out: 5,
        };
        let show = |t: u64, turn: i32, seq: u64| Rec::Show {
            t,
            which: "tr".to_string(),
            turn,
            seq,
            chars: 5,
        };
        let recs = vec![
            Rec::Session { t: 0 },
            // 使い回し：確定の要求はなく途中訳が表示される
            Rec::Start { t: 10, turn: 1 },
            Rec::Partial {
                t: 100,
                seq: 1,
                text: "Hello world".to_string(),
            },
            Rec::Complete {
                t: 200,
                turn: 1,
                seq: 2,
                text: "Hello world.".to_string(),
            },
            xlate(300, 1, 1),
            show(310, 1, 2),
            // 同言語スキップ：要求なしで表示される
            Rec::Start { t: 400, turn: 2 },
            Rec::Complete {
                t: 500,
                turn: 2,
                seq: 3,
                text: "Same.".to_string(),
            },
            Rec::Skip {
                t: 510,
                turn: 2,
                seq: 3,
            },
            show(520, 2, 3),
            // 翻訳あり・未表示
            Rec::Start { t: 600, turn: 3 },
            Rec::Complete {
                t: 700,
                turn: 3,
                seq: 4,
                text: "Lost.".to_string(),
            },
            xlate(800, 3, 4),
            // 未翻訳
            Rec::Start { t: 900, turn: 4 },
            Rec::Complete {
                t: 1000,
                turn: 4,
                seq: 5,
                text: "Untranslated.".to_string(),
            },
        ];
        let report = cover(&recs, &script);
        assert_eq!(report.sentences.len(), 4);
        let row = |i: usize| &report.sentences[i];
        assert!(row(0).translated && row(0).shown);
        assert!(row(1).translated && row(1).shown);
        assert!(row(2).translated && !row(2).shown);
        assert!(!row(3).translated && !row(3).shown);
    }

    #[test]
    fn cover_pairs_utterances_tolerantly() {
        let script = vec![];
        let speech = |t: u64, start: bool| Rec::Speech {
            t,
            state: if start {
                "start".to_string()
            } else {
                "end".to_string()
            },
        };
        let recs = vec![
            Rec::Session { t: 0 },
            speech(100, true),
            Rec::Partial {
                t: 300,
                seq: 1,
                text: "hi".to_string(),
            },
            speech(1000, false),
            // 閉じ忘れは次の開始で区切る
            speech(2000, true),
            speech(3000, true),
            Rec::Complete {
                t: 4500,
                turn: 1,
                seq: 2,
                text: "late.".to_string(),
            },
            speech(4000, false),
            // 対応のない終了は無視する
            speech(5000, false),
        ];
        let report = cover(&recs, &script);
        assert_eq!(report.utterances.len(), 3);
        assert_eq!(report.utterances[0].turns, vec![(0, 0)]);
        assert!(!report.utterances[0].split);
        assert!(report.utterances[1].turns.is_empty());
        assert!(report.utterances[1].split);
        // 終了後の確定は余裕で同一区間に寄る
        assert!(report.utterances[2].complete);
    }

    #[test]
    fn parses_speech_and_skip_lines() {
        let log = "⏱ {\"t\":10,\"ev\":\"speech\",\"state\":\"start\"}\n\
             ⏱ {\"t\":20,\"ev\":\"skip\",\"turn\":1,\"seq\":2}\n";
        let (recs, skipped) = parse_log(log);
        assert_eq!(skipped, 0);
        assert!(matches!(recs[0], Rec::Speech { t: 10, .. }));
        assert!(matches!(
            recs[1],
            Rec::Skip {
                turn: 1,
                seq: 2,
                ..
            }
        ));
    }

    /// 実ログ形式→解析→突き合わせの一気通貫。fixture は main.rs の出す
    /// 全11形状（session/start/partial/trigger/complete/skip/speech/
    /// xlate/xlate_err/show/drop）＋🔍ノイズ行を含む。排出側の形状を
    /// 変えたら fixture も更新すること（skipped==0 が検知する）
    #[test]
    fn coverage_end_to_end_from_log_text() {
        let (recs, skipped) = parse_log(include_str!("testdata/coverage_sample.log"));
        assert_eq!(skipped, 0);
        assert!(recs.iter().any(|r| matches!(r, Rec::Trigger { .. })));
        assert!(recs.iter().any(|r| matches!(r, Rec::Skip { .. })));
        assert!(recs.iter().any(|r| matches!(r, Rec::Speech { .. })));

        // 台本なし：発話→認識と原文→翻訳・表示
        let report = cover(&recs, &[]);
        assert_eq!(report.utterances.len(), 4);
        let u1 = &report.utterances[0];
        assert_eq!((u1.start_t, u1.end_t), (100, 2000));
        assert_eq!(u1.turns, vec![(0, 1)]);
        assert!(u1.complete);
        assert_eq!(u1.first_partial_ms, Some(200));
        assert_eq!(report.utterances[1].turns, vec![(0, 2)]);
        assert!(!report.utterances[1].complete);
        assert_eq!(report.utterances[2].turns, vec![(0, 3)]);
        assert!(report.utterances[2].complete);
        // 再接続後は世代1として数える
        assert_eq!(report.utterances[3].turns, vec![(1, 1)]);
        assert!(report.utterances.iter().all(|u| !u.turns.is_empty()));

        assert_eq!(report.sentences.len(), 4);
        let row = |i: usize| &report.sentences[i];
        assert!(row(0).translated && row(0).shown);
        assert!(!row(1).translated && !row(1).shown);
        // 同言語スキップは翻訳済み扱い
        assert!(row(2).translated && row(2).shown);
        assert!(row(3).translated && row(3).shown);

        let t = |s: u32, t: i32| &report.turns[&(s, t)];
        assert_eq!(t(0, 1).partials, 2);
        assert!(t(0, 1).complete);
        assert_eq!((t(0, 1).xlate_ok, t(0, 1).shown_tr), (1, 1));
        assert_eq!((t(0, 2).xlate_err, t(0, 2).dropped_tr), (1, 1));
        assert!(t(1, 1).complete);

        // 台本あり：台本照合が加わる
        let script: Vec<ScriptSeg> =
            serde_json::from_str(include_str!("testdata/coverage_sample_script.json")).unwrap();
        let report = cover(&recs, &script);
        assert_eq!(report.script_hits.len(), 2);
        assert_eq!(report.script_hits[0].turn, Some((0, 1)));
        assert!(!report.script_hits[0].partial_only);
        assert_eq!(report.script_hits[1].turn, None);
    }
}

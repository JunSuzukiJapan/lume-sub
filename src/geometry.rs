//! 字幕ウィンドウの位置・サイズの永続化
//!
//! 移動・リサイズの結果を JSON で保存し、次回起動時に復元する。
//! 保存がない初回はモニター中央下部に配置する。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 字幕ウィンドウの既定サイズ（pt）
pub const DEFAULT_W: f32 = 800.0;
pub const DEFAULT_H: f32 = 150.0;
/// リサイズの下限（pt）
pub const MIN_W: f32 = 240.0;
pub const MIN_H: f32 = 80.0;
/// 初期配置の画面下からの余白（pt）
pub const MARGIN_BOTTOM: f32 = 60.0;
/// 保存の間引き間隔。ドラッグ中の連続書き込みを抑える
const SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// 保存する幾何情報（モニター座標系・pt）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// 保存先。`~/.config/lume-sub/window.json`（`XDG_CONFIG_HOME` 尊重）
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lume-sub").join("window.json"))
}

/// 保存済み幾何を読む。存在しない・壊れている場合は None
pub fn load(path: &std::path::Path) -> Option<WindowGeometry> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            println!("⚠️ 字幕位置ファイルを読めません（{}）: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(g) => Some(g),
        Err(e) => {
            println!("⚠️ 字幕位置ファイルが壊れています（{}）: {e}", path.display());
            None
        }
    }
}

/// 幾何を保存する。親ディレクトリがなければ作る
pub fn save(path: &std::path::Path, geom: &WindowGeometry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(geom).expect("geometry serialization");
    std::fs::write(path, text)
}

/// モニター中央下部の配置を計算する
pub fn bottom_center(monitor: egui::Vec2, size: egui::Vec2, margin_bottom: f32) -> egui::Pos2 {
    egui::Pos2::new(
        ((monitor.x - size.x).max(0.0)) / 2.0,
        (monitor.y - size.y - margin_bottom).max(0.0),
    )
}

/// 前回保存から変わり、間引き間隔が過ぎたら保存すべきか
fn should_save(last: &Option<WindowGeometry>, current: &WindowGeometry, elapsed: Duration) -> bool {
    elapsed >= SAVE_INTERVAL && last.is_none_or(|l| l != *current)
}

/// 変化時のみ間引き保存する。パスなし（保存先不明）のときは何もしない
pub struct GeometrySaver {
    path: Option<PathBuf>,
    last: Option<WindowGeometry>,
    last_save: Instant,
}

impl GeometrySaver {
    pub fn new(path: Option<PathBuf>) -> Self {
        if path.is_none() {
            println!("⚠️ 保存先が決まらないため字幕位置は保存されません");
        }
        Self {
            path,
            last: None,
            last_save: Instant::now(),
        }
    }

    pub fn tick(&mut self, ctx: &egui::Context) {
        let Some(path) = &self.path else { return };
        let info = ctx.input(|i| {
            (
                i.viewport().outer_rect.map(|r| r.min),
                i.viewport().inner_rect.map(|r| r.size()),
            )
        });
        let (Some(pos), Some(size)) = info else { return };
        let current = WindowGeometry {
            x: pos.x,
            y: pos.y,
            w: size.x,
            h: size.y,
        };
        if should_save(&self.last, &current, self.last_save.elapsed()) {
            if let Err(e) = save(path, &current) {
                println!("⚠️ 字幕位置の保存に失敗: {e}");
            }
            self.last = Some(current);
            self.last_save = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lume-sub-geom-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn roundtrip_save_load() {
        let path = scratch("roundtrip");
        let geom = WindowGeometry {
            x: 320.5,
            y: 900.0,
            w: 800.0,
            h: 150.0,
        };
        save(&path, &geom).unwrap();
        assert_eq!(load(&path), Some(geom));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_or_corrupt_loads_none() {
        assert_eq!(load(&scratch("definitely-missing")), None);
        let path = scratch("corrupt");
        std::fs::write(&path, "{oops").unwrap();
        assert_eq!(load(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bottom_center_math() {
        let pos = bottom_center(egui::vec2(1920.0, 1080.0), egui::vec2(800.0, 150.0), 60.0);
        assert_eq!(pos, egui::Pos2::new(560.0, 870.0));
        // 画面より大きい窓ははみ出さず原点側に寄せる
        let pos = bottom_center(egui::vec2(100.0, 100.0), egui::vec2(800.0, 150.0), 60.0);
        assert_eq!(pos, egui::Pos2::new(0.0, 0.0));
    }

    #[test]
    fn saves_only_on_change_after_interval() {
        let a = WindowGeometry {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 150.0,
        };
        let b = WindowGeometry { x: 10.0, ..a };
        assert!(should_save(&None, &a, SAVE_INTERVAL));
        assert!(!should_save(&None, &a, Duration::ZERO));
        assert!(!should_save(&Some(a), &a, SAVE_INTERVAL * 10));
        assert!(should_save(&Some(a), &b, SAVE_INTERVAL));
    }
}

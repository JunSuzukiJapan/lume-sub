//! ウィンドウの位置・サイズの型と初期配置の計算
//!
//! 保存自体は [`crate::store::Store`] が担う。このモジュールは幾何型と
//! 初期配置（字幕は中央下部、コントロールは右上）の計算だけを持つ。

use serde::{Deserialize, Serialize};

/// 字幕ウィンドウの既定サイズ（pt）
pub const DEFAULT_W: f32 = 800.0;
pub const DEFAULT_H: f32 = 150.0;
/// リサイズの下限（pt）
pub const MIN_W: f32 = 240.0;
pub const MIN_H: f32 = 80.0;
/// 初期配置の画面下からの余白（pt）
pub const MARGIN_BOTTOM: f32 = 60.0;
/// コントロールウィンドウの既定サイズ（pt）
pub const CONTROL_DEFAULT_W: f32 = 320.0;
pub const CONTROL_DEFAULT_H: f32 = 440.0;
/// コントロール初期配置の画面上・右からの余白（pt）
pub const MARGIN_TOP: f32 = 24.0;
pub const MARGIN_RIGHT: f32 = 24.0;
/// 翻訳窓の初期配置で原文窓との間に空ける隙間（pt）
pub const TRANSLATION_GAP: f32 = 8.0;

/// 保存する幾何情報（モニター座標系・pt）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// モニター中央下部の配置を計算する
pub fn bottom_center(monitor: egui::Vec2, size: egui::Vec2, margin_bottom: f32) -> egui::Pos2 {
    egui::Pos2::new(
        ((monitor.x - size.x).max(0.0)) / 2.0,
        (monitor.y - size.y - margin_bottom).max(0.0),
    )
}

/// モニター右上の配置を計算する
pub fn top_right(
    monitor: egui::Vec2,
    size: egui::Vec2,
    margin_top: f32,
    margin_right: f32,
) -> egui::Pos2 {
    egui::Pos2::new(
        (monitor.x - size.x - margin_right).max(0.0),
        margin_top.max(0.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_center_math() {
        let pos = bottom_center(egui::vec2(1920.0, 1080.0), egui::vec2(800.0, 150.0), 60.0);
        assert_eq!(pos, egui::Pos2::new(560.0, 870.0));
        // 画面より大きい窓ははみ出さず原点側に寄せる
        let pos = bottom_center(egui::vec2(100.0, 100.0), egui::vec2(800.0, 150.0), 60.0);
        assert_eq!(pos, egui::Pos2::new(0.0, 0.0));
    }

    #[test]
    fn top_right_math() {
        let pos = top_right(
            egui::vec2(1920.0, 1080.0),
            egui::vec2(320.0, 440.0),
            24.0,
            24.0,
        );
        assert_eq!(pos, egui::Pos2::new(1576.0, 24.0));
        // 画面より大きい窓ははみ出さず原点側に寄せる
        let pos = top_right(
            egui::vec2(100.0, 100.0),
            egui::vec2(320.0, 440.0),
            24.0,
            24.0,
        );
        assert_eq!(pos, egui::Pos2::new(0.0, 24.0));
    }
}

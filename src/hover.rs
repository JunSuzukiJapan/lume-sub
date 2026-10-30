//! 字幕オーバーレイのホバー連動操作
//!
//! 字幕ウィンドウは普段マウス透過だが、OS のマウス位置を毎フレーム照会し、
//! 矩形内に入ったら透過を外して移動・リサイズ用のフレームを出す。
//! egui 側のポインタ状態は透過中更新されないため、内外・押下とも OS 値を用いる。
//! 取得は AppKit の状態照会のみで、アクセシビリティ権限は不要。

use crate::geometry;

/// OS のマウス状態（pt・左上原点の位置＋押下有無）。取れなければ None。
/// NSEvent の画面座標（pt・左下原点）を主画面の高さで反転し、
/// egui の outer_rect（pt・左上原点）に合わせる
#[cfg(target_os = "macos")]
fn os_mouse_state() -> Option<(egui::Pos2, bool)> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSScreen};
    let mtm = MainThreadMarker::new()?;
    let primary = NSScreen::screens(mtm).firstObject()?;
    let height = primary.frame().size.height;
    let p = NSEvent::mouseLocation();
    let pos = egui::Pos2::new(p.x as f32, (height - p.y) as f32);
    let pressed = NSEvent::pressedMouseButtons() != 0;
    Some((pos, pressed))
}

#[cfg(not(target_os = "macos"))]
fn os_mouse_state() -> Option<(egui::Pos2, bool)> {
    unimplemented!("global mouse polling is macOS-only")
}

/// 透過↔操作の切替状態機械。
/// 押下の立ち上がりが矩形内だったときだけ、その押下中は外に出ても操作を維持する。
/// 外で押したまま中に入っても透過を奪わず（クリック透過を壊さない）、
/// 外で離しても OS 値で検知するため固着しない。
pub struct HoverLatch {
    prev_pressed: bool,
    press_started_inside: bool,
}

impl HoverLatch {
    pub fn new() -> Self {
        Self {
            prev_pressed: false,
            press_started_inside: false,
        }
    }

    /// 現在の操作状態（true＝透過OFF＋フレーム表示）を返す。
    /// 押下中は開始位置だけで決め、横切りで奪い取らない
    pub fn update(&mut self, inside: bool, pressed: bool) -> bool {
        if pressed && !self.prev_pressed {
            self.press_started_inside = inside;
        } else if !pressed {
            self.press_started_inside = false;
        }
        self.prev_pressed = pressed;
        if pressed {
            self.press_started_inside
        } else {
            inside
        }
    }
}

/// 矩形（pt・左上原点）内に OS マウス位置があるか
pub fn hit_test(outer: &egui::Rect, mouse_pt: egui::Pos2) -> bool {
    outer.contains(mouse_pt)
}

/// 毎フレームのポーリングと透過切替（変化時のみコマンド送信）
pub struct HoverState {
    latch: HoverLatch,
    interactive: bool,
}

impl HoverState {
    pub fn new() -> Self {
        Self {
            latch: HoverLatch::new(),
            interactive: false,
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let outer = ctx.input(|i| i.viewport().outer_rect);
        let (Some(outer), Some((pos, pressed))) = (outer, os_mouse_state()) else {
            return self.interactive;
        };
        let inside = hit_test(&outer, pos);
        let next = self.latch.update(inside, pressed);
        if next != self.interactive {
            self.interactive = next;
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(!next));
        }
        next
    }
}

/// 移動ストリップ＋リサイズグリップ＋バツ付きの枠を描く。レイアウトには影響しない。
/// `tag` は複数窓での操作ID衝突を避ける窓名。バツが押されたら true を返す。
/// もう片方の窓が非表示中など、閉じられないときは `close_enabled` を false にすると
/// バツがグレーアウトしてクリックを受け付けない
pub fn paint_frame(ui: &mut egui::Ui, drag_label: &str, tag: &str, close_enabled: bool) -> bool {
    let rect = ui.max_rect();
    let painter = ui.painter();
    let bright = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 140);
    let dim = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 160);
    painter.rect_stroke(rect.shrink(1.0), 8.0, egui::Stroke::new(1.5_f32, bright));

    // 上部の移動ストリップ（右上のバツと重ならないよう短くする）
    let strip = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() - 34.0, 20.0));
    painter.rect_filled(
        strip,
        8.0,
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14),
    );
    painter.text(
        strip.center(),
        egui::Align2::CENTER_CENTER,
        drag_label,
        egui::FontId::proportional(11.0),
        dim,
    );
    let move_resp = ui
        .interact(
            strip,
            ui.id().with(("overlay-move", tag)),
            egui::Sense::drag(),
        )
        .on_hover_cursor(egui::CursorIcon::Grab);
    if move_resp.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    // 右下のリサイズグリップ（豆腐回避のため図形で描く）
    let max = rect.max;
    for i in 0..3 {
        let d = 8.0 + i as f32 * 6.0;
        painter.line_segment(
            [
                egui::Pos2::new(max.x - d, max.y - 5.0),
                egui::Pos2::new(max.x - 5.0, max.y - d),
            ],
            egui::Stroke::new(1.5_f32, bright),
        );
    }
    let corner = egui::Rect::from_min_size(max - egui::vec2(28.0, 28.0), egui::vec2(28.0, 28.0));
    let resize_resp = ui
        .interact(
            corner,
            ui.id().with(("overlay-resize", tag)),
            egui::Sense::drag(),
        )
        .on_hover_cursor(egui::CursorIcon::ResizeNwSe);
    if resize_resp.dragged()
        && let Some(size) = ui.ctx().input(|i| i.viewport().inner_rect.map(|r| r.size()))
    {
        let new =
            (size + resize_resp.drag_delta()).max(egui::vec2(geometry::MIN_W, geometry::MIN_H));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::InnerSize(new));
    }

    // 右上のバツ（豆腐回避のため図形で描く）
    let close_rect = egui::Rect::from_min_size(
        egui::Pos2::new(rect.max.x - 30.0, rect.min.y + 2.0),
        egui::vec2(26.0, 20.0),
    );
    let clicked = if close_enabled {
        let close_resp = ui
            .interact(
                close_rect,
                ui.id().with(("overlay-close", tag)),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if close_resp.hovered() {
            painter.rect_filled(
                close_rect,
                4.0,
                egui::Color32::from_rgba_unmultiplied(255, 80, 80, 90),
            );
        }
        close_resp.clicked()
    } else {
        false
    };
    let center = close_rect.center();
    let r = 5.5;
    let cross_color = if close_enabled {
        bright
    } else {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 45)
    };
    let cross = egui::Stroke::new(2.0_f32, cross_color);
    painter.line_segment(
        [center + egui::vec2(-r, -r), center + egui::vec2(r, r)],
        cross,
    );
    painter.line_segment(
        [center + egui::vec2(-r, r), center + egui::vec2(r, -r)],
        cross,
    );
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_toggles_interactive() {
        let mut latch = HoverLatch::new();
        assert!(!latch.update(false, false));
        assert!(latch.update(true, false));
        assert!(!latch.update(false, false));
    }

    #[test]
    fn drag_out_stays_until_release_outside() {
        let mut latch = HoverLatch::new();
        assert!(latch.update(true, true)); // 内で押す
        assert!(latch.update(false, true)); // 外に出ても維持
        assert!(!latch.update(false, false)); // 外で離したら透過に戻る（固着しない）
    }

    #[test]
    fn click_through_is_not_stolen() {
        let mut latch = HoverLatch::new();
        assert!(!latch.update(false, true)); // 外で押す
        assert!(!latch.update(true, true)); // 押したまま中に入っても奪わない
        assert!(!latch.update(false, true)); // 押したまま外に出ても透過のまま
        assert!(!latch.update(false, false)); // 外で離す
    }

    #[test]
    fn release_inside_shows_frame() {
        let mut latch = HoverLatch::new();
        assert!(!latch.update(false, true));
        assert!(!latch.update(true, true));
        assert!(latch.update(true, false)); // 中で離すと通常ホバーに戻る
    }

    #[test]
    fn hit_test_points() {
        let outer = egui::Rect::from_min_max(
            egui::Pos2::new(100.0, 100.0),
            egui::Pos2::new(500.0, 250.0),
        );
        let pt = egui::Pos2::new;
        assert!(hit_test(&outer, pt(200.0, 150.0)));
        assert!(hit_test(&outer, pt(100.0, 100.0)));
        assert!(!hit_test(&outer, pt(50.0, 50.0)));
        // egui の矩形は閉区間のため境界上は内側
        assert!(hit_test(&outer, pt(500.0, 250.0)));
        assert!(!hit_test(&outer, pt(500.5, 250.5)));
    }
}

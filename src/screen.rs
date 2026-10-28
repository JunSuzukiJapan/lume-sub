//! メイン画面への字幕追従
//!
//! フルスクリーン動画などで操作対象の画面が切り替わったら、字幕も追いかける。
//! マウスカーソルのある画面を毎フレーム照会し（`NSScreen::mainScreen` は
//! 追従が鈍い／ずれることがあるため使わない）、字幕がその画面の外にあれば
//! 中央下部へ移す（画面内の微調整とは干渉しない）。
//! オン・オフは [`Settings`] で保持し、`~/.config/lume-sub/settings.json` に保存する。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::geometry;

#[cfg(target_os = "macos")]
use objc2::{MainThreadMarker, rc::Retained};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSPanel, NSTextField};

/// 操作対象画面のスナップショット（AppKit座標系・pt）。
/// 反転用にメインディスプレイの物理高さも持つ（winit と同じ定数）。
/// `screens` は接続画面数（診断用）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MainScreen {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    flip_h: f32,
    screens: usize,
}

impl MainScreen {
    /// egui/winit座標系（左上原点）での画面上端
    fn top_in_egui(&self) -> f32 {
        self.flip_h - (self.y + self.h)
    }

    /// egui/winit座標系での画面矩形
    fn rect_in_egui(&self) -> egui::Rect {
        egui::Rect::from_min_size(
            egui::Pos2::new(self.x, self.top_in_egui()),
            egui::Vec2::new(self.w, self.h),
        )
    }

    /// 点が画面内にあるか
    fn contains_in_egui(&self, p: egui::Pos2) -> bool {
        self.rect_in_egui().contains(p)
    }
}

/// 画面の中央下部（egui座標系の左上）。単画面なら
/// [`geometry::bottom_center`] と一致する
pub fn bottom_center_on(main: MainScreen, win: egui::Vec2) -> egui::Pos2 {
    egui::Pos2::new(
        main.x + (main.w - win.x).max(0.0) / 2.0,
        main.top_in_egui() + (main.h - win.y - geometry::MARGIN_BOTTOM).max(0.0),
    )
}

/// 字幕の現在位置をメイン画面と照合し、外にあれば移動先（egui座標系の左上）を返す。
/// 中心が画面内なら何もしない。OS照会は呼び出し側が行う
pub fn reconcile_target(main: MainScreen, outer: egui::Rect) -> Option<egui::Pos2> {
    if main.contains_in_egui(outer.center()) {
        None
    } else {
        Some(bottom_center_on(main, outer.size()))
    }
}

/// 操作対象画面スナップショットの失敗理由（診断用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapFail {
    /// メインスレッド以外から呼ばれた
    NotMainThread,
    /// 画面が1つもない
    NoScreens,
    /// マウスがいずれの画面の外にある（画面間の隙間など）
    MouseOutsideScreens,
}

/// 操作対象の画面（マウスカーソルのある画面）のスナップショット。
/// 照会できないときは理由付きで失敗する
#[cfg(target_os = "macos")]
pub fn active_screen_snapshot() -> Result<MainScreen, SnapFail> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSScreen};

    let mtm = MainThreadMarker::new().ok_or(SnapFail::NotMainThread)?;
    let screens = NSScreen::screens(mtm);
    if screens.is_empty() {
        return Err(SnapFail::NoScreens);
    }
    // マウス位置（AppKit座標系・pt）を含む画面を探す
    let p = NSEvent::mouseLocation();
    let found = screens.iter().find(|s| {
        let f = s.frame();
        p.x >= f.origin.x
            && p.x <= f.origin.x + f.size.width
            && p.y >= f.origin.y
            && p.y <= f.origin.y + f.size.height
    });
    let target = found.ok_or(SnapFail::MouseOutsideScreens)?;
    let f = target.frame();
    Ok(MainScreen {
        x: f.origin.x as f32,
        y: f.origin.y as f32,
        w: f.size.width as f32,
        h: f.size.height as f32,
        flip_h: main_display_pixels_high() as f32,
        screens: screens.len(),
    })
}

#[cfg(not(target_os = "macos"))]
pub fn active_screen_snapshot() -> Result<MainScreen, SnapFail> {
    unimplemented!("active screen detection is macOS-only")
}

/// 字幕オーバーレイを常時最前面にする（全Space参加＋スクリーンセーバーレベル）。
/// winit は両方とも出さないため AppKit 直叩き。
/// 適用できたら true（窓がまだなければ false）
#[cfg(target_os = "macos")]
pub fn ensure_overlay_over_fullscreen() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSScreenSaverWindowLevel, NSWindowCollectionBehavior};
    use objc2_foundation::ns_string;

    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return false,
    };
    let want = NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::FullScreenAuxiliary;
    for window in NSApplication::sharedApplication(mtm).windows().iter() {
        if !window.title().isEqualToString(ns_string!("LumeSub")) {
            continue;
        }
        let mut behavior = window.collectionBehavior();
        if !behavior.contains(want) {
            behavior.insert(want);
            window.setCollectionBehavior(behavior);
        }
        if window.level() != NSScreenSaverWindowLevel {
            window.setLevel(NSScreenSaverWindowLevel);
        }
        return true;
    }
    false
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_overlay_over_fullscreen() -> bool {
    unimplemented!("fullscreen overlay control is macOS-only")
}

/// winit字幕窓の可視状態。窓がなければ None
#[cfg(target_os = "macos")]
pub fn overlay_occlusion_visible() -> Option<bool> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSWindowOcclusionState};
    use objc2_foundation::ns_string;

    let mtm = MainThreadMarker::new()?;
    for window in NSApplication::sharedApplication(mtm).windows().iter() {
        if window.title().isEqualToString(ns_string!("LumeSub")) {
            return Some(
                window
                    .occlusionState()
                    .contains(NSWindowOcclusionState::Visible),
            );
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
pub fn overlay_occlusion_visible() -> Option<bool> {
    unimplemented!("occlusion query is macOS-only")
}

/// winit と同じ反転定数（メインディスプレイの物理ピクセル高）の公開窓口
#[cfg(target_os = "macos")]
pub fn flip_height() -> f32 {
    main_display_pixels_high() as f32
}

#[cfg(not(target_os = "macos"))]
pub fn flip_height() -> f32 {
    unimplemented!("screen geometry is macOS-only")
}

/// パネルの高さ（pt）。字幕の下辺に合わせる固定値
const PANEL_HEIGHT: f32 = 120.0;

/// 字幕枠（egui座標系）→パネル枠（AppKit座標系のx,y,w,h）。
/// 幅と下辺は字幕と同じ、高さは固定
fn panel_frame_for(outer: egui::Rect, flip_h: f32) -> (f32, f32, f32, f32) {
    let bottom_ns = flip_h - outer.max.y;
    (outer.min.x, bottom_ns, outer.size().x, PANEL_HEIGHT)
}

/// フルスクリーン動画用のNSPanel字幕。
/// NSWindow は他アプリのフルスクリーンSpaceに入れないため、パネルで出す。
/// 生成・操作・破棄はすべてメインスレッド（egui の update から呼ぶ）。
#[cfg(target_os = "macos")]
pub struct FullscreenPanel {
    panel: Retained<NSPanel>,
    field: Retained<NSTextField>,
    last_frame: (f32, f32, f32, f32),
    last_text: String,
}

#[cfg(not(target_os = "macos"))]
pub struct FullscreenPanel {
    _private: (),
}

#[cfg(target_os = "macos")]
impl FullscreenPanel {
    fn new(mtm: MainThreadMarker, frame: (f32, f32, f32, f32)) -> Self {
        use objc2::msg_send;
        use objc2_app_kit::{
            NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSFont, NSFontWeightBold,
            NSLineBreakMode, NSScreenSaverWindowLevel, NSTextAlignment, NSWindowCollectionBehavior,
            NSWindowStyleMask,
        };
        use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

        let rect = NSRect {
            origin: NSPoint {
                x: frame.0 as f64,
                y: frame.1 as f64,
            },
            size: NSSize {
                width: frame.2 as f64,
                height: frame.3 as f64,
            },
        };
        // SAFETY: 引数はすべて有効な値。生成直後の初期化のみに使う
        let panel: Retained<NSPanel> = unsafe {
            msg_send![
                mtm.alloc::<NSPanel>(),
                initWithContentRect: rect,
                styleMask: NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        panel.setTitle(&NSString::from_str("LumeSub Subtitle"));
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setIgnoresMouseEvents(true);
        panel.setHidesOnDeactivate(false);
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setLevel(NSScreenSaverWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );

        let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        field.setAlignment(NSTextAlignment(2)); // Center（定数がないため直値）
        // SAFETY: 定数の読み取りのみ
        let bold = unsafe { NSFontWeightBold };
        field.setFont(Some(&NSFont::systemFontOfSize_weight(28.0, bold)));
        field.setTextColor(Some(&NSColor::whiteColor()));
        field.setDrawsBackground(true);
        field.setBackgroundColor(Some(&NSColor::blackColor().colorWithAlphaComponent(0.65)));
        field.setMaximumNumberOfLines(0);
        if let Some(cell) = field.cell() {
            cell.setWraps(true);
            cell.setUsesSingleLineMode(false);
            cell.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        }
        if let Some(content) = panel.contentView() {
            let bounds = content.bounds();
            field.setFrame(NSRect {
                origin: NSPoint { x: 12.0, y: 8.0 },
                size: NSSize {
                    width: (bounds.size.width - 24.0).max(0.0),
                    height: (bounds.size.height - 16.0).max(0.0),
                },
            });
            field.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            content.addSubview(&field);
        }
        Self {
            panel,
            field,
            last_frame: frame,
            last_text: String::new(),
        }
    }

    /// 枠・文言を同期して前面に出す。変化がなければ順序だけ。
    /// text は論理順の原文（整形はAppKit任せ。二重整形しないこと）
    fn show(&mut self, frame: (f32, f32, f32, f32), text: &str) {
        use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

        if self.last_frame != frame {
            self.panel.setFrame_display(
                NSRect {
                    origin: NSPoint {
                        x: frame.0 as f64,
                        y: frame.1 as f64,
                    },
                    size: NSSize {
                        width: frame.2 as f64,
                        height: frame.3 as f64,
                    },
                },
                true,
            );
            self.last_frame = frame;
        }
        if self.last_text != text {
            self.field.setStringValue(&NSString::from_str(text));
            self.last_text = text.to_string();
        }
        self.panel.orderFrontRegardless();
    }

    fn hide(&self) {
        self.panel.orderOut(None);
    }
}

/// フルスクリーン動画中だけNSPanel字幕を出す。
/// overlay_visible が false（＝他アプリのフルスクリーン中とみなす）のときだけ表示し、
/// それ以外では隠す。表示切替時のみ診断ログを出す
#[cfg(target_os = "macos")]
pub fn sync_fullscreen_panel(
    slot: &mut Option<FullscreenPanel>,
    overlay_visible: Option<bool>,
    outer: egui::Rect,
    flip_h: f32,
    text: &str,
    diag: &mut DiagLog,
) {
    use objc2::MainThreadMarker;

    if overlay_visible != Some(false) {
        if slot.is_some() {
            diag.log_if_changed("panel|hidden", "🖥️ パネル字幕: 非表示");
        }
        if let Some(panel) = slot.as_ref() {
            panel.hide();
        }
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let frame = panel_frame_for(outer, flip_h);
    let panel = slot.get_or_insert_with(|| FullscreenPanel::new(mtm, frame));
    diag.log_if_changed("panel|shown", &format!("🖥️ パネル字幕: 表示 {frame:?}"));
    panel.show(frame, text);
}

#[cfg(not(target_os = "macos"))]
pub fn sync_fullscreen_panel(
    _slot: &mut Option<FullscreenPanel>,
    _overlay_visible: Option<bool>,
    _outer: egui::Rect,
    _flip_h: f32,
    _text: &str,
    _diag: &mut DiagLog,
) {
    unimplemented!("fullscreen overlay control is macOS-only")
}

/// 診断ログ。キーが変わったときだけ出す（定常時は無言）
#[derive(Default)]
pub struct DiagLog {
    last_key: Option<String>,
}

impl DiagLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn log_if_changed(&mut self, key: &str, detail: &str) {
        if self.last_key.as_deref() != Some(key) {
            self.last_key = Some(key.to_string());
            println!("{detail}");
        }
    }
}

/// winit と同じ反転定数（メインディスプレイの物理ピクセル高）。
/// AppKit座標はptだが、winit 0.29 の macOS 位置空間はこの値で反転している
#[cfg(target_os = "macos")]
fn main_display_pixels_high() -> u32 {
    unsafe { CGDisplayPixelsHigh(CGMainDisplayID()) as u32 }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
}

fn default_true() -> bool {
    true
}

/// 字幕ウィンドウの振る舞い設定。追従は既定オン
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_true")]
    pub follow_main_screen: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            follow_main_screen: true,
        }
    }
}

/// 保存先。`~/.config/lume-sub/settings.json`（`XDG_CONFIG_HOME` 尊重）
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lume-sub").join("settings.json"))
}

/// 保存済み設定を読む。存在しない・壊れている場合は None
pub fn load(path: &std::path::Path) -> Option<Settings> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            println!("⚠️ 設定ファイルを読めません（{}）: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            println!("⚠️ 設定ファイルが壊れています（{}）: {e}", path.display());
            None
        }
    }
}

/// 設定を保存する。親ディレクトリがなければ作る
pub fn save(path: &std::path::Path, settings: &Settings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(settings).expect("settings serialization");
    std::fs::write(path, text)
}

/// GUI で共有する設定。操作ウィンドウから実行時変更する
pub type SharedSettings = Arc<Mutex<Settings>>;

pub fn shared(settings: Settings) -> SharedSettings {
    Arc::new(Mutex::new(settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: f32, y: f32, w: f32, h: f32) -> MainScreen {
        MainScreen {
            x,
            y,
            w,
            h,
            flip_h: 1440.0,
            screens: 2,
        }
    }

    fn win() -> egui::Vec2 {
        egui::vec2(800.0, 150.0)
    }

    #[test]
    fn single_screen_matches_geometry_bottom_center() {
        let m = screen(0.0, 0.0, 2560.0, 1440.0);
        assert_eq!(
            bottom_center_on(m, win()),
            geometry::bottom_center(egui::vec2(2560.0, 1440.0), win(), geometry::MARGIN_BOTTOM)
        );
    }

    #[test]
    fn second_screen_right_offsets_by_its_origin() {
        // 主画面 2560x1440 の右に 1920x1080 を並べた場合
        let m = screen(2560.0, 0.0, 1920.0, 1080.0);
        // egui空間での画面上端 = 1440 - 1080 = 360
        assert_eq!(
            bottom_center_on(m, win()),
            egui::Pos2::new(2560.0 + 560.0, 360.0 + 870.0)
        );
    }

    #[test]
    fn panel_frame_bottom_aligns_with_fixed_height() {
        let outer = egui::Rect::from_min_size(egui::Pos2::new(880.0, 1230.0), win());
        assert_eq!(panel_frame_for(outer, 1440.0), (880.0, 60.0, 800.0, 120.0));
    }

    #[test]
    fn oversized_window_clamps_to_screen_top_left() {
        let m = screen(2560.0, 0.0, 1920.0, 1080.0);
        assert_eq!(
            bottom_center_on(m, egui::vec2(3000.0, 2000.0)),
            egui::Pos2::new(2560.0, 360.0)
        );
    }

    #[test]
    fn reconcile_moves_window_on_other_screen() {
        // 字幕は主画面、メインは右の副画面。画面変化がなくても動く
        let main = screen(2560.0, 0.0, 1920.0, 1080.0);
        let outer = egui::Rect::from_min_size(egui::Pos2::new(880.0, 1230.0), win());
        assert_eq!(
            reconcile_target(main, outer),
            Some(bottom_center_on(main, win()))
        );
    }

    #[test]
    fn reconcile_leaves_window_on_main_screen() {
        let main = screen(2560.0, 0.0, 1920.0, 1080.0);
        // 画面内なら中央下部でなくても動かない（微調整と干渉しない）
        let outer = egui::Rect::from_min_size(egui::Pos2::new(2600.0, 500.0), win());
        assert_eq!(reconcile_target(main, outer), None);
    }

    #[test]
    fn reconcile_judges_by_center() {
        let main = screen(2560.0, 0.0, 1920.0, 1080.0);
        // 中心が画面内ならはみ出していても動かない
        let straddle_in = egui::Rect::from_min_size(egui::Pos2::new(2500.0, 500.0), win());
        assert_eq!(reconcile_target(main, straddle_in), None);
        // 中心が画面外なら動く
        let straddle_out = egui::Rect::from_min_size(egui::Pos2::new(1700.0, 500.0), win());
        assert_eq!(
            reconcile_target(main, straddle_out),
            Some(bottom_center_on(main, win()))
        );
    }

    #[test]
    fn reconcile_landing_is_stable() {
        // 移動先では中心が画面内に収まり、次回は何もしない
        let main = screen(2560.0, 0.0, 1920.0, 1080.0);
        let landed = egui::Rect::from_min_size(bottom_center_on(main, win()), win());
        assert_eq!(reconcile_target(main, landed), None);
    }

    #[test]
    fn settings_default_is_follow_on() {
        assert!(Settings::default().follow_main_screen);
        // 空オブジェクトでも既定オン
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.follow_main_screen);
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lume-sub-settings-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn roundtrip_save_load() {
        for follow in [true, false] {
            let path = scratch(if follow { "on" } else { "off" });
            let s = Settings {
                follow_main_screen: follow,
            };
            save(&path, &s).unwrap();
            assert_eq!(load(&path), Some(s));
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn missing_or_corrupt_loads_none() {
        assert_eq!(load(&scratch("definitely-missing")), None);
        let path = scratch("corrupt");
        std::fs::write(&path, "{oops").unwrap();
        assert_eq!(load(&path), None);
        let _ = std::fs::remove_file(&path);
    }
}

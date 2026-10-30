//! メイン画面への字幕追従
//!
//! フルスクリーン動画などで操作対象の画面が切り替わったら、字幕も追いかける。
//! マウスカーソルのある画面を毎フレーム照会し（`NSScreen::mainScreen` は
//! 追従が鈍い／ずれることがあるため使わない）、字幕がその画面の外にあれば
//! 中央下部へ移す（画面内の微調整とは干渉しない）。
//! オン・オフは [`crate::store::Store`] で保持・保存する。

use crate::geometry;

#[cfg(target_os = "macos")]
use objc2::{MainThreadMarker, rc::Retained};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSImageView, NSPanel, NSTextField};

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

/// 字幕オーバーレイ窓を常時最前面にする（全Space参加＋スクリーンセーバーレベル）。
/// winit は両方とも出さないため AppKit 直叩き。
/// 適用できたら true（窓がまだなければ false）
#[cfg(target_os = "macos")]
pub fn ensure_window_over_fullscreen(title: &str) -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSScreenSaverWindowLevel, NSWindowCollectionBehavior};
    use objc2_foundation::NSString;

    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return false,
    };
    let want = NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::FullScreenAuxiliary;
    let target = NSString::from_str(title);
    for window in NSApplication::sharedApplication(mtm).windows().iter() {
        if !window.title().isEqualToString(&target) {
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
pub fn ensure_window_over_fullscreen(_title: &str) -> bool {
    unimplemented!("fullscreen overlay control is macOS-only")
}

/// winit字幕窓の可視状態。窓がなければ None
#[cfg(target_os = "macos")]
pub fn window_occlusion_visible(title: &str) -> Option<bool> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSWindowOcclusionState};
    use objc2_foundation::NSString;

    let mtm = MainThreadMarker::new()?;
    let target = NSString::from_str(title);
    for window in NSApplication::sharedApplication(mtm).windows().iter() {
        if window.title().isEqualToString(&target) {
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
pub fn window_occlusion_visible(_title: &str) -> Option<bool> {
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

/// 字幕枠（egui座標系）→パネル枠（AppKit座標系のx,y,w,h）。
/// 幅・高さ・下辺は字幕と同じ
fn panel_frame_for(outer: egui::Rect, flip_h: f32) -> (f32, f32, f32, f32) {
    let bottom_ns = flip_h - outer.max.y;
    (outer.min.x, bottom_ns, outer.size().x, outer.size().y)
}

/// [`panel_frame_for`] の逆。AppKit枠→egui字幕枠（ドラッグ結果の書戻し用）
fn ns_frame_to_egui(frame: (f32, f32, f32, f32), flip_h: f32) -> egui::Rect {
    let (x, y_bottom, w, h) = frame;
    egui::Rect::from_min_size(
        egui::Pos2::new(x, flip_h - (y_bottom + h)),
        egui::Vec2::new(w, h),
    )
}

/// パネル上のホバ位置（AppKit座標系で判定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelHover {
    /// 上部の移動ストリップ
    Move,
    /// 右下（画面見た目）のリサイズ角
    Resize,
    /// 右上のバツ
    Close,
    /// ハンドル以外の板面
    Body,
    /// パネル外
    Outside,
}

impl PanelHover {
    fn is_handle(self) -> bool {
        matches!(self, PanelHover::Move | PanelHover::Resize | PanelHover::Close)
    }
}

/// 移動ストリップの高さ（pt）。通常窓（20pt）より太くして掴みやすくする
const PANEL_MOVE_STRIP_H: f32 = 24.0;
/// リサイズ角の一辺（pt）。通常窓のホバー枠と合わせる
const PANEL_RESIZE_S: f32 = 28.0;
/// バツの高さ（pt）。ストリップに揃える（幅 26pt・上余白 2ptは通常窓と同じ）
const PANEL_CLOSE_H: f32 = 24.0;
/// 字幕地の縁 inset（pt）。横・下。上は端まで伸ばして隙間を作らない
const FIELD_INSET_X: f32 = 12.0;
const FIELD_INSET_Y: f32 = 8.0;

/// AppKit枠 (x, 下辺y, w, h) に対するマウス位置の分類。
/// バツ→角→帯の順に見る。バツ無効時は帯に読み替える
fn classify_panel_hover(
    frame: (f32, f32, f32, f32),
    mouse: (f32, f32),
    close_enabled: bool,
) -> PanelHover {
    let (x, y, w, h) = frame;
    let (mx, my) = mouse;
    if mx < x || mx > x + w || my < y || my > y + h {
        return PanelHover::Outside;
    }
    // 右上のバツ（幅 26pt・上余白 2pt、高さはストリップに揃える）
    if close_enabled
        && mx >= x + w - 30.0
        && mx <= x + w - 4.0
        && my >= y + h - 2.0 - PANEL_CLOSE_H
        && my <= y + h - 2.0
    {
        return PanelHover::Close;
    }
    if mx >= x + w - PANEL_RESIZE_S && my <= y + PANEL_RESIZE_S {
        return PanelHover::Resize;
    }
    if my >= y + h - PANEL_MOVE_STRIP_H {
        return PanelHover::Move;
    }
    PanelHover::Body
}

/// パネル操作の掴み状態
#[derive(Debug, Clone, Copy, PartialEq)]
enum PanelDragState {
    Idle,
    Move { dx: f32, dy: f32 },
    Resize { start: (f32, f32, f32, f32), mx: f32, my: f32 },
    CloseArmed,
}

/// フルスクリーンパネルのドラッグ状態機械。座標はすべてAppKit系pt。
/// 押下の立ち上がりがハンドル内だったときだけ掴み、外で押したまま
/// 入ってきても奪わない（[`crate::hover::HoverLatch`] と同じ約束）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelDrag {
    state: PanelDragState,
    prev_down: bool,
    dirty: bool,
}

/// [`PanelDrag::update`] の結果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelDragOutput {
    /// パネルに被せるマウス無視。ホバ中だけ入力を受ける
    pub ignore_mouse: bool,
    pub hover: PanelHover,
    /// ドラッグで変わった先のAppKit枠。変化フレームのみ Some
    pub desired: Option<(f32, f32, f32, f32)>,
    /// 押下終了かつ移動済み。呼び出し側は保存を確定する
    pub finished: bool,
    /// バツの押下がバツ内で離された。呼び出し側は窓を閉じる
    pub close_requested: bool,
}

impl PanelDrag {
    pub fn new() -> Self {
        Self {
            state: PanelDragState::Idle,
            prev_down: false,
            dirty: false,
        }
    }

    /// ドラッグ中か
    pub fn is_active(&self) -> bool {
        !matches!(self.state, PanelDragState::Idle)
    }

    /// 今回の表示中に枠を動かしたか。鏡写しの源を実窓から保存値に切り替える合図
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// パネル非表示時に捨てる
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn update(
        &mut self,
        frame: (f32, f32, f32, f32),
        mouse: (f32, f32),
        down: bool,
        close_enabled: bool,
    ) -> PanelDragOutput {
        let rising = down && !self.prev_down;
        self.prev_down = down;
        let hover = classify_panel_hover(frame, mouse, close_enabled);
        match self.state {
            PanelDragState::Idle => {
                if rising && hover.is_handle() {
                    self.state = match hover {
                        PanelHover::Move => PanelDragState::Move {
                            dx: mouse.0 - frame.0,
                            dy: mouse.1 - frame.1,
                        },
                        PanelHover::Close => PanelDragState::CloseArmed,
                        _ => PanelDragState::Resize {
                            start: frame,
                            mx: mouse.0,
                            my: mouse.1,
                        },
                    };
                    PanelDragOutput {
                        ignore_mouse: false,
                        hover,
                        desired: None,
                        finished: false,
                        close_requested: false,
                    }
                } else if down {
                    // 外で押したまま。透過を維持する
                    PanelDragOutput {
                        ignore_mouse: true,
                        hover,
                        desired: None,
                        finished: false,
                        close_requested: false,
                    }
                } else {
                    PanelDragOutput {
                        ignore_mouse: !hover.is_handle(),
                        hover,
                        desired: None,
                        finished: false,
                        close_requested: false,
                    }
                }
            }
            PanelDragState::Move { dx, dy } => {
                if !down {
                    self.state = PanelDragState::Idle;
                    PanelDragOutput {
                        ignore_mouse: true,
                        hover,
                        desired: None,
                        finished: self.dirty,
                        close_requested: false,
                    }
                } else {
                    let next = (mouse.0 - dx, mouse.1 - dy, frame.2, frame.3);
                    let changed = next != frame;
                    self.dirty |= changed;
                    PanelDragOutput {
                        ignore_mouse: false,
                        hover: PanelHover::Move,
                        desired: changed.then_some(next),
                        finished: false,
                        close_requested: false,
                    }
                }
            }
            PanelDragState::Resize { start, mx, my } => {
                if !down {
                    self.state = PanelDragState::Idle;
                    PanelDragOutput {
                        ignore_mouse: true,
                        hover,
                        desired: None,
                        finished: self.dirty,
                        close_requested: false,
                    }
                } else {
                    // 上辺は固定し、右辺・下辺（画面見た目）だけ動かす
                    let top = start.1 + start.3;
                    let w = (start.2 + (mouse.0 - mx)).max(geometry::MIN_W);
                    let h = (top - (start.1 + (mouse.1 - my))).max(geometry::MIN_H);
                    let next = (start.0, top - h, w, h);
                    let changed = next != frame;
                    self.dirty |= changed;
                    PanelDragOutput {
                        ignore_mouse: false,
                        hover: PanelHover::Resize,
                        desired: changed.then_some(next),
                        finished: false,
                        close_requested: false,
                    }
                }
            }
            PanelDragState::CloseArmed => {
                if !down {
                    self.state = PanelDragState::Idle;
                    PanelDragOutput {
                        ignore_mouse: true,
                        hover,
                        desired: None,
                        finished: false,
                        close_requested: hover == PanelHover::Close,
                    }
                } else {
                    PanelDragOutput {
                        ignore_mouse: false,
                        hover: PanelHover::Close,
                        desired: None,
                        finished: false,
                        close_requested: false,
                    }
                }
            }
        }
    }
}

/// フルスクリーン動画用のNSPanel字幕。
/// NSWindow は他アプリのフルスクリーンSpaceに入れないため、パネルで出す。
/// 普段はマウスを無視するが、ホバ中は通常窓と同じ枠（移動バー・バツ・
/// グリップ）を出して移動・リサイズ・閉じるを受け付ける。生成・操作・
/// 破棄はすべてメインスレッド（egui の update から呼ぶ）。
#[cfg(target_os = "macos")]
pub struct FullscreenPanel {
    panel: Retained<NSPanel>,
    field: Retained<NSTextField>,
    strip: Retained<NSTextField>,
    close_icon: Retained<NSImageView>,
    grip_icon: Retained<NSImageView>,
    last_frame: (f32, f32, f32, f32),
    last_text: String,
    last_hint: String,
    ignores: bool,
    frame_shown: bool,
    close_shown: bool,
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
            NSImage, NSImageView, NSLineBreakMode, NSScreenSaverWindowLevel, NSTextAlignment,
            NSWindowCollectionBehavior, NSWindowStyleMask,
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

        // 字幕地（常時表示）。文言欄はバーの分だけ低く固定し、
        // ホバによる文言のずれをなくす（通常窓の上余白と同じ）
        let backdrop = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        backdrop.setDrawsBackground(true);
        backdrop.setBackgroundColor(Some(&NSColor::blackColor().colorWithAlphaComponent(0.65)));
        let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        field.setAlignment(NSTextAlignment(2)); // Center（定数がないため直値）
        // SAFETY: 定数の読み取りのみ
        let bold = unsafe { NSFontWeightBold };
        field.setFont(Some(&NSFont::systemFontOfSize_weight(28.0, bold)));
        field.setTextColor(Some(&NSColor::whiteColor()));
        field.setDrawsBackground(false);

        // ホバー枠（通常窓と同寸）。掴み位置の目安で、入力自体はOS照会で捌く。
        // バー地は字幕地と同じ黒にして一体化する
        let strip = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        strip.setAlignment(NSTextAlignment(2)); // Center
        strip.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        strip.setTextColor(Some(&NSColor::whiteColor()));
        strip.setDrawsBackground(true);
        strip.setBackgroundColor(Some(&NSColor::blackColor().colorWithAlphaComponent(0.65)));
        strip.setHidden(true);
        // SAFETY: 既定初期化のみ
        let close_icon: Retained<NSImageView> =
            unsafe { msg_send![mtm.alloc::<NSImageView>(), init] };
        if let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("xmark"),
            None,
        ) {
            close_icon.setImage(Some(&img));
        }
        close_icon.setContentTintColor(Some(&NSColor::whiteColor()));
        close_icon.setHidden(true);
        // SAFETY: 既定初期化のみ
        let grip_icon: Retained<NSImageView> =
            unsafe { msg_send![mtm.alloc::<NSImageView>(), init] };
        if let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("arrow.up.left.and.arrow.down.right"),
            None,
        ) {
            grip_icon.setImage(Some(&img));
        }
        grip_icon.setContentTintColor(Some(&NSColor::whiteColor()));
        grip_icon.setHidden(true);
        field.setMaximumNumberOfLines(0);
        if let Some(cell) = field.cell() {
            cell.setWraps(true);
            cell.setUsesSingleLineMode(false);
            cell.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        }
        if let Some(content) = panel.contentView() {
            let bounds = content.bounds();
            backdrop.setFrame(NSRect {
                origin: NSPoint {
                    x: FIELD_INSET_X as f64,
                    y: FIELD_INSET_Y as f64,
                },
                size: NSSize {
                    width: (bounds.size.width - 2.0 * FIELD_INSET_X as f64).max(0.0),
                    // 上端は欄の上まで伸ばして隙間を作らない
                    height: (bounds.size.height - FIELD_INSET_Y as f64).max(0.0),
                },
            });
            backdrop.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            content.addSubview(&backdrop);
            // 文言欄の上端はバーの下端に固定（ホバ前後で不変）
            field.setFrame(NSRect {
                origin: NSPoint {
                    x: FIELD_INSET_X as f64,
                    y: FIELD_INSET_Y as f64,
                },
                size: NSSize {
                    width: (bounds.size.width - 2.0 * FIELD_INSET_X as f64).max(0.0),
                    height: (bounds.size.height
                        - PANEL_MOVE_STRIP_H as f64
                        - FIELD_INSET_Y as f64)
                        .max(0.0),
                },
            });
            field.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            content.addSubview(&field);
            strip.setFrame(NSRect {
                origin: NSPoint {
                    // 字幕地の左右に揃える（文言は字幕の中央に来る）
                    x: FIELD_INSET_X as f64,
                    y: bounds.size.height - PANEL_MOVE_STRIP_H as f64,
                },
                size: NSSize {
                    width: (bounds.size.width - 2.0 * FIELD_INSET_X as f64).max(0.0),
                    height: PANEL_MOVE_STRIP_H as f64,
                },
            });
            strip.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewMinYMargin,
            );
            content.addSubview(&strip);
            close_icon.setFrame(NSRect {
                origin: NSPoint {
                    x: bounds.size.width - 30.0,
                    y: bounds.size.height - 2.0 - PANEL_CLOSE_H as f64,
                },
                size: NSSize {
                    width: 26.0,
                    height: PANEL_CLOSE_H as f64,
                },
            });
            close_icon.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
            );
            content.addSubview(&close_icon);
            grip_icon.setFrame(NSRect {
                origin: NSPoint {
                    x: bounds.size.width - 28.0,
                    y: 0.0,
                },
                size: NSSize {
                    width: 28.0,
                    height: 28.0,
                },
            });
            grip_icon.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxYMargin,
            );
            content.addSubview(&grip_icon);
        }
        Self {
            panel,
            field,
            strip,
            close_icon,
            grip_icon,
            last_frame: frame,
            last_text: String::new(),
            last_hint: String::new(),
            ignores: true,
            frame_shown: false,
            close_shown: false,
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

    /// 変化時のみマウス無視を切り替える
    fn set_ignores_mouse(&mut self, ignore: bool) {
        if self.ignores != ignore {
            self.panel.setIgnoresMouseEvents(ignore);
            self.ignores = ignore;
        }
    }

    /// ホバー枠（移動バー・バツ・グリップ）の表示と文言を整える。
    /// 掴める位置の目安で、入力の透過切替は別途行う。
    /// 文言欄は初めからバーの下に固定してあるため、ここでは動かさない
    fn set_hover_frame(&mut self, show: bool, close_enabled: bool, hint: &str) {
        use objc2_foundation::NSString;

        if self.last_hint != hint {
            self.strip.setStringValue(&NSString::from_str(hint));
            self.last_hint = hint.to_string();
        }
        if self.frame_shown != show {
            self.strip.setHidden(!show);
            self.grip_icon.setHidden(!show);
            self.frame_shown = show;
        }
        let show_close = show && close_enabled;
        if self.close_shown != show_close {
            self.close_icon.setHidden(!show_close);
            self.close_shown = show_close;
        }
    }
}

/// フルスクリーン動画中だけNSPanel字幕を出す。
/// overlay_visible が false（＝他アプリのフルスクリーン中とみなす）のときだけ表示し、
/// それ以外では隠す。表示切替時のみ診断ログを出す。表示中なら true
#[cfg(target_os = "macos")]
pub fn sync_fullscreen_panel(
    slot: &mut Option<FullscreenPanel>,
    overlay_visible: Option<bool>,
    outer: egui::Rect,
    flip_h: f32,
    text: &str,
    diag: &mut DiagLog,
) -> bool {
    use objc2::MainThreadMarker;

    if overlay_visible != Some(false) {
        if slot.is_some() {
            diag.log_if_changed("panel|hidden", "🖥️ パネル字幕: 非表示");
        }
        if let Some(panel) = slot.as_ref() {
            panel.hide();
        }
        return false;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let frame = panel_frame_for(outer, flip_h);
    let panel = slot.get_or_insert_with(|| FullscreenPanel::new(mtm, frame));
    diag.log_if_changed("panel|shown", &format!("🖥️ パネル字幕: 表示 {frame:?}"));
    panel.show(frame, text);
    true
}

#[cfg(not(target_os = "macos"))]
pub fn sync_fullscreen_panel(
    _slot: &mut Option<FullscreenPanel>,
    _overlay_visible: Option<bool>,
    _outer: egui::Rect,
    _flip_h: f32,
    _text: &str,
    _diag: &mut DiagLog,
) -> bool {
    unimplemented!("fullscreen overlay control is macOS-only")
}

/// パネル操作用のマウス照会。AppKit座標系ptと押下有無。
/// 通常窓と違って egui 側にポインタが届かないため OS 値を直接見る
#[cfg(target_os = "macos")]
pub fn poll_pointer() -> ((f32, f32), bool) {
    use objc2_app_kit::NSEvent;
    let p = NSEvent::mouseLocation();
    ((p.x as f32, p.y as f32), NSEvent::pressedMouseButtons() != 0)
}

#[cfg(not(target_os = "macos"))]
pub fn poll_pointer() -> ((f32, f32), bool) {
    unimplemented!("global mouse polling is macOS-only")
}

/// 表示中パネルの毎フレーム操作の結果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelInteractOutput {
    /// ドラッグで変わった先の字幕枠（egui座標系）。変化フレームのみ Some
    pub desired: Option<egui::Rect>,
    /// 押下終了かつ移動済み。呼び出し側は保存を確定する
    pub finished: bool,
    /// バツの押下がバツ内で離された。呼び出し側は窓を閉じる
    pub close_requested: bool,
}

/// 表示中パネルの毎フレーム操作。ホバ中だけマウスを受け、ドラッグ先を返す。
/// `mirror` は [`sync_fullscreen_panel`] に渡したのと同じ字幕枠。
/// `drag_hint` は移動バーの文言（論理順のまま。整形はAppKit任せ）
#[cfg(target_os = "macos")]
pub fn interact_fullscreen_panel(
    panel: &mut FullscreenPanel,
    drag: &mut PanelDrag,
    mirror: egui::Rect,
    flip_h: f32,
    pointer: ((f32, f32), bool),
    drag_hint: &str,
    close_enabled: bool,
) -> PanelInteractOutput {
    let (mouse, down) = pointer;
    let out = drag.update(panel_frame_for(mirror, flip_h), mouse, down, close_enabled);
    panel.set_ignores_mouse(out.ignore_mouse);
    panel.set_hover_frame(
        out.hover != PanelHover::Outside,
        close_enabled,
        drag_hint,
    );
    PanelInteractOutput {
        desired: out.desired.map(|f| ns_frame_to_egui(f, flip_h)),
        finished: out.finished,
        close_requested: out.close_requested,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn interact_fullscreen_panel(
    _panel: &mut FullscreenPanel,
    _drag: &mut PanelDrag,
    _mirror: egui::Rect,
    _flip_h: f32,
    _pointer: ((f32, f32), bool),
    _drag_hint: &str,
    _close_enabled: bool,
) -> PanelInteractOutput {
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
    fn panel_frame_mirrors_size_with_bottom_aligned() {
        let outer = egui::Rect::from_min_size(egui::Pos2::new(880.0, 1230.0), win());
        assert_eq!(panel_frame_for(outer, 1440.0), (880.0, 60.0, 800.0, 150.0));
    }

    /// AppKit枠 (x, 下辺y, w, h)
    fn panel() -> (f32, f32, f32, f32) {
        (100.0, 200.0, 800.0, 150.0)
    }

    #[test]
    fn ns_egui_roundtrip() {
        let outer = egui::Rect::from_min_size(egui::Pos2::new(880.0, 1230.0), win());
        assert_eq!(ns_frame_to_egui(panel_frame_for(outer, 1440.0), 1440.0), outer);
    }

    #[test]
    fn hover_regions() {
        let f = panel();
        // 上部ストリップ＝移動（AppKit上辺 350 の下 24pt）
        assert_eq!(
            classify_panel_hover(f, (150.0, 340.0), true),
            PanelHover::Move
        );
        assert_eq!(
            classify_panel_hover(f, (850.0, 349.0), true),
            PanelHover::Move
        );
        assert_eq!(
            classify_panel_hover(f, (400.0, 327.0), true),
            PanelHover::Move
        );
        assert_eq!(
            classify_panel_hover(f, (400.0, 325.0), true),
            PanelHover::Body
        );
        // 右下角＝リサイズ（右辺 900 の左 28pt × 下辺 200 の上 28pt）
        assert_eq!(
            classify_panel_hover(f, (890.0, 210.0), true),
            PanelHover::Resize
        );
        assert_eq!(
            classify_panel_hover(f, (899.0, 201.0), true),
            PanelHover::Resize
        );
        // 板面
        assert_eq!(
            classify_panel_hover(f, (400.0, 280.0), true),
            PanelHover::Body
        );
        // 外
        assert_eq!(
            classify_panel_hover(f, (50.0, 280.0), true),
            PanelHover::Outside
        );
        assert_eq!(
            classify_panel_hover(f, (400.0, 100.0), true),
            PanelHover::Outside
        );
    }

    #[test]
    fn tiny_window_corner_wins() {
        // 高さ 40 では帯（上 20pt）と角（下 28pt）が重なる。角を優先する
        let f = (0.0, 0.0, 300.0, 40.0);
        // バツ（右 30〜4pt）は角よりさらに優先する
        assert_eq!(
            classify_panel_hover(f, (290.0, 25.0), true),
            PanelHover::Close
        );
        // バツの右隣 4pt は角
        assert_eq!(
            classify_panel_hover(f, (298.0, 25.0), true),
            PanelHover::Resize
        );
        assert_eq!(
            classify_panel_hover(f, (100.0, 35.0), true),
            PanelHover::Move
        );
    }

    #[test]
    fn close_region_priority() {
        let f = panel();
        // 右上のバツ（右辺 900 の左 30〜4pt × 上辺 350 の下 26〜2pt）
        assert_eq!(
            classify_panel_hover(f, (880.0, 340.0), true),
            PanelHover::Close
        );
        assert_eq!(
            classify_panel_hover(f, (880.0, 325.0), true),
            PanelHover::Close
        );
        assert_eq!(
            classify_panel_hover(f, (880.0, 323.0), true),
            PanelHover::Body
        );
        // バツの左隣は帯のまま
        assert_eq!(
            classify_panel_hover(f, (860.0, 340.0), true),
            PanelHover::Move
        );
        // バツ無効時は帯に読み替える
        assert_eq!(
            classify_panel_hover(f, (880.0, 340.0), false),
            PanelHover::Move
        );
    }

    #[test]
    fn idle_hover_gates_input() {
        let mut d = PanelDrag::new();
        let f = panel();
        let o = d.update(f, (400.0, 280.0), false, true);
        assert!(o.ignore_mouse);
        assert_eq!(o.hover, PanelHover::Body);
        assert!(o.desired.is_none());
        assert!(!o.finished);
        assert!(!o.close_requested);
        let o = d.update(f, (400.0, 340.0), false, true);
        assert!(!o.ignore_mouse);
        assert_eq!(o.hover, PanelHover::Move);
    }

    #[test]
    fn rising_edge_grabs_and_moves() {
        let mut d = PanelDrag::new();
        let f = panel();
        // ホバ→押下で掴む
        let o = d.update(f, (400.0, 340.0), true, true);
        assert!(d.is_active());
        assert!(!o.ignore_mouse);
        // 動かすと掴み点のずれを保って枠が追従する
        let o = d.update(f, (410.0, 330.0), true, true);
        assert_eq!(o.desired, Some((110.0, 190.0, 800.0, 150.0)));
        assert!(d.is_dirty());
        // 離すと終了
        let o = d.update((110.0, 190.0, 800.0, 150.0), (410.0, 330.0), false, true);
        assert!(!d.is_active());
        assert!(o.finished);
    }

    #[test]
    fn press_outside_never_grabs() {
        let mut d = PanelDrag::new();
        let f = panel();
        // 外で押す→透過維持
        let o = d.update(f, (50.0, 280.0), true, true);
        assert!(o.ignore_mouse);
        assert!(!d.is_active());
        // 押したままハンドルに入っても奪わない
        let o = d.update(f, (400.0, 340.0), true, true);
        assert!(o.ignore_mouse);
        assert!(!d.is_active());
        assert!(o.desired.is_none());
        // 離したら通常ホバーに戻る
        let o = d.update(f, (400.0, 340.0), false, true);
        assert!(!o.ignore_mouse);
        assert!(!o.finished);
    }

    #[test]
    fn click_without_move_finishes_quietly() {
        let mut d = PanelDrag::new();
        let f = panel();
        d.update(f, (400.0, 340.0), true, true);
        assert!(d.is_active());
        let o = d.update(f, (400.0, 340.0), true, true);
        assert!(o.desired.is_none());
        let o = d.update(f, (400.0, 340.0), false, true);
        assert!(!o.finished);
        assert!(!d.is_dirty());
    }

    #[test]
    fn resize_grows_and_clamps() {
        let mut d = PanelDrag::new();
        let f = panel();
        d.update(f, (890.0, 210.0), true, true);
        assert!(d.is_active());
        // 右下（画面見た目）に引くと右辺・下辺が伸びる
        let o = d.update(f, (910.0, 190.0), true, true);
        assert_eq!(o.desired, Some((100.0, 180.0, 820.0, 170.0)));
        assert!(d.is_dirty());
        // 縮めすぎは下限（240x80）で止まる
        let o = d.update(f, (150.0, 340.0), true, true);
        assert_eq!(o.desired, Some((100.0, 270.0, 240.0, 80.0)));
    }

    #[test]
    fn close_click_lifecycle() {
        let mut d = PanelDrag::new();
        let f = panel();
        // バツ上で押すと構える
        let o = d.update(f, (880.0, 330.0), true, true);
        assert!(d.is_active());
        assert!(!o.ignore_mouse);
        assert!(!o.close_requested);
        // バツ内で離すと閉じる（移動はなし）
        let o = d.update(f, (880.0, 330.0), false, true);
        assert!(!d.is_active());
        assert!(o.close_requested);
        assert!(!o.finished);
        assert!(!d.is_dirty());
    }

    #[test]
    fn close_release_outside_cancels() {
        let mut d = PanelDrag::new();
        let f = panel();
        d.update(f, (880.0, 330.0), true, true);
        // 押したまま外に出て離すと何も起きない
        let o = d.update(f, (400.0, 280.0), true, true);
        assert!(!o.close_requested);
        let o = d.update(f, (400.0, 280.0), false, true);
        assert!(!o.close_requested);
        assert!(!o.finished);
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
}

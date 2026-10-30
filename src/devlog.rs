//! 開発ログの出力制御
//!
//! コンソールへのログは開発オプションがオンのときだけ出す。
//! 設定値は [`crate::store::Store`] が保持し、起動時と切替時にここへ反映する。
//! ホットパスからロックなしで読めるよう、`AtomicBool` で持つ。

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// 開発ログのオン・オフを切り替える
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// 開発ログが出るか
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 開発オプションがオンのときだけ `println!` する
#[macro_export]
macro_rules! dev_log {
    ($($arg:tt)*) => {
        if $crate::devlog::enabled() {
            println!($($arg)*);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_roundtrip() {
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());
    }
}

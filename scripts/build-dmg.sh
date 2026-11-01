#!/usr/bin/env bash
#
# LumeSub 配布用 .dmg ビルドスクリプト（Universal 2: Intel + Apple Silicon）
#
# 使い方:
#   ./scripts/build-dmg.sh              # フルビルド + .dmg 作成
#   ./scripts/build-dmg.sh --skip-build # 既存バイナリから .app/.dmg だけ作り直す
#   ./scripts/build-dmg.sh --help       # ヘルプ表示
#
# 出力:
#   dist/LumeSub-<version>-universal.dmg  (+ 副産物 dist/LumeSub.app)
#
# 署名・公証（任意）:
#   署名なし（アドホック）のままだと配布先の Mac で Gatekeeper に止められる。
#   正式配布には Developer ID が必要。以下を設定して実行すると
#   署名 → 公証 → staple まで行う:
#     export APPLE_DEVELOPER_ID="Developer ID Application: Your Name (TEAMID)"
#     export APPLE_ID="you@example.com"
#     export APPLE_TEAM_ID="TEAMID"
#     export APPLE_APP_PASSWORD="xxxx-xxxx-xxxx-xxxx"  # App 用パスワード
#   またはキーチェーン済みプロファイルを使う場合:
#     export APPLE_KEYCHAIN_PROFILE="your-profile"
#
set -euo pipefail

# ── 設定 ──────────────────────────────────────────────
APP_NAME="LumeSub"
BIN_NAME="lume-sub"
BUNDLE_ID="com.lumesub.app"
# ScreenCaptureKit が必要なため macOS 12.3 以降
MIN_SYSTEM_VERSION="12.3"
TARGET_INTEL="x86_64-apple-darwin"
TARGET_SILICON="aarch64-apple-darwin"

# ── 引数 ──────────────────────────────────────────────
SKIP_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=1 ;;
        -h|--help)
            awk 'NR>1 && /^[^#]/{exit} NR>1' "$0" | sed -E 's/^# ?//'
            exit 0
            ;;
        *)
            echo "error: 不明な引数: ${arg}（--help を参照）" >&2
            exit 1
            ;;
    esac
done

# ── 準備 ──────────────────────────────────────────────
if [[ "$(uname)" != "Darwin" ]]; then
    echo "error: このスクリプトは macOS 専用です" >&2
    exit 1
fi
for cmd in cargo lipo hdiutil codesign; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "error: 必須コマンドが見つかりません: $cmd" >&2
        exit 1
    fi
done

# rustup があればそのツールチェーンの cargo/rustc を使う。
# PATH 先頭の cargo が Homebrew 版だと rustup target を認識できず、
# クロスビルドに失敗するため、ここで明示的に切り替える。
# 注意: `rustup run` では cargo の子プロセスの rustc 解決が付け替わらないため、
# ツールチェーンの bin を PATH の先頭に載せ、RUSTC も明示する。
TOOLCHAIN=""
if command -v rustup >/dev/null 2>&1; then
    TOOLCHAIN="$(rustup show active-toolchain 2>/dev/null | awk '{print $1}')"
    if [[ -z "$TOOLCHAIN" ]]; then
        echo "error: rustup のアクティブなツールチェーンを取得できませんでした" >&2
        exit 1
    fi
    TOOLCHAIN_BIN="$(dirname "$(rustup which --toolchain "$TOOLCHAIN" cargo 2>/dev/null)")"
    if [[ ! -x "$TOOLCHAIN_BIN/cargo" || ! -x "$TOOLCHAIN_BIN/rustc" ]]; then
        echo "error: ツールチェーン $TOOLCHAIN の cargo/rustc が見つかりません" >&2
        exit 1
    fi
    export PATH="$TOOLCHAIN_BIN:$PATH"
    export RUSTC="$TOOLCHAIN_BIN/rustc"
    hash -r
    echo "==> cargo: $TOOLCHAIN ($(cargo --version))"
else
    echo "==> cargo: $(cargo --version)"
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version[[:space:]]*=' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
if [[ -z "$VERSION" ]]; then
    echo "error: Cargo.toml から version を取得できませんでした" >&2
    exit 1
fi

BIN_INTEL="target/$TARGET_INTEL/release/$BIN_NAME"
BIN_SILICON="target/$TARGET_SILICON/release/$BIN_NAME"
APP_DIR="dist/$APP_NAME.app"
APP_BIN="$APP_DIR/Contents/MacOS/$APP_NAME"
DMG_BASE="dist/${APP_NAME}-${VERSION}-universal"
DMG="$DMG_BASE.dmg"
STAGE="dist/dmg-staging"

echo "==> $APP_NAME $VERSION (Universal 2) をビルドします"

# ── 1. ターゲット追加 ─────────────────────────────────
if [[ "$SKIP_BUILD" -eq 0 ]]; then
    if ! command -v rustup >/dev/null 2>&1; then
        echo "error: rustup が見つかりません。$TARGET_INTEL と $TARGET_SILICON の" >&2
        echo "       両ターゲットが必要です。rustup を導入するか、手動で追加してください" >&2
        exit 1
    fi
    for target in "$TARGET_INTEL" "$TARGET_SILICON"; do
        if ! rustup target list --toolchain "$TOOLCHAIN" --installed 2>/dev/null | grep -qx "$target"; then
            echo "==> ターゲットを追加します: $target"
            rustup target add --toolchain "$TOOLCHAIN" "$target"
        fi
    done

    # ── 2. 両アーキテクチャをリリースビルド ────────────────
    echo "==> ビルド中: $TARGET_INTEL"
    cargo build --release --target "$TARGET_INTEL"
    echo "==> ビルド中: $TARGET_SILICON"
    cargo build --release --target "$TARGET_SILICON"
fi

for bin in "$BIN_INTEL" "$BIN_SILICON"; do
    if [[ ! -f "$bin" ]]; then
        echo "error: バイナリが見つかりません: $bin" >&2
        echo "       --skip-build なしで実行してください" >&2
        exit 1
    fi
done
echo "==> 入力バイナリ:"
echo "    Intel : $(lipo -info "$BIN_INTEL")"
echo "    Silicon: $(lipo -info "$BIN_SILICON")"

# ── 3. .app バンドル作成 ───────────────────────────────
echo "==> .app バンドルを作成します: $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

# Universal バイナリを合成
lipo -create -output "$APP_BIN" "$BIN_INTEL" "$BIN_SILICON"
chmod +x "$APP_BIN"

# 合成結果を検証（両アーキテクチャ必須）
LIPO_INFO="$(lipo -info "$APP_BIN")"
if [[ "$LIPO_INFO" != *"x86_64"* ]] || [[ "$LIPO_INFO" != *"arm64"* ]]; then
    echo "error: Universal バイナリ化に失敗しました: $LIPO_INFO" >&2
    exit 1
fi
echo "    合成結果: $LIPO_INFO"

# アイコン（あれば同梱、なければスキップ）
ICON_SRC=""
for candidate in "assets/AppIcon.icns" "assets/icon.icns" "AppIcon.icns"; do
    if [[ -f "$candidate" ]]; then
        ICON_SRC="$candidate"
        break
    fi
done
ICON_KEYS=""
if [[ -n "$ICON_SRC" ]]; then
    cp "$ICON_SRC" "$APP_DIR/Contents/Resources/AppIcon.icns"
    ICON_KEYS="	<key>CFBundleIconFile</key>
	<string>AppIcon</string>"
    echo "    アイコン: $ICON_SRC"
else
    echo "    アイコン: なし（assets/AppIcon.icns を置くと同梱されます）"
fi

cat > "$APP_DIR/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>$APP_NAME</string>
	<key>CFBundleDisplayName</key>
	<string>$APP_NAME</string>
	<key>CFBundleIdentifier</key>
	<string>$BUNDLE_ID</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleSignature</key>
	<string>????</string>
	<key>CFBundleExecutable</key>
	<string>$APP_NAME</string>
$ICON_KEYS
	<key>LSMinimumSystemVersion</key>
	<string>$MIN_SYSTEM_VERSION</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>マイク音声の文字起こしに使用します</string>
	<key>NSScreenCaptureUsageDescription</key>
	<string>画面の音声キャプチャに使用します</string>
</dict>
</plist>
EOF

# ── 4. 署名 ───────────────────────────────────────────
if [[ -n "${APPLE_DEVELOPER_ID:-}" ]]; then
    echo "==> 署名します（Developer ID）: $APPLE_DEVELOPER_ID"
    codesign --force --deep --options runtime --timestamp \
        --sign "$APPLE_DEVELOPER_ID" "$APP_DIR"
else
    echo "==> アドホック署名します（配布先では Gatekeeper に止められます）"
    codesign --force --deep --sign - "$APP_DIR"
fi
codesign --verify --deep --strict "$APP_DIR"
echo "    署名 OK: $(codesign -dv "$APP_DIR" 2>&1 | grep -m1 'Authority' || echo 'adhoc')"

# ── 5. .dmg 作成 ──────────────────────────────────────
echo "==> .dmg を作成します: $DMG"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp -R "$APP_DIR" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create \
    -volname "$APP_NAME $VERSION" \
    -srcfolder "$STAGE" \
    -ov -format UDZO -imagekey zlib-level=9 \
    -o "$DMG_BASE" >/dev/null
rm -rf "$STAGE"

# ── 6. 公証（認証情報がある場合のみ） ──────────────────
if [[ -n "${APPLE_DEVELOPER_ID:-}" ]]; then
    codesign --force --sign "$APPLE_DEVELOPER_ID" "$DMG"
    if [[ -n "${APPLE_KEYCHAIN_PROFILE:-}" ]]; then
        echo "==> 公証を申請します（profile: ${APPLE_KEYCHAIN_PROFILE}）"
        xcrun notarytool submit "$DMG" --keychain-profile "$APPLE_KEYCHAIN_PROFILE" --wait
        xcrun stapler staple "$DMG"
    elif [[ -n "${APPLE_ID:-}" && -n "${APPLE_TEAM_ID:-}" && -n "${APPLE_APP_PASSWORD:-}" ]]; then
        echo "==> 公証を申請します（Apple ID: ${APPLE_ID}）"
        xcrun notarytool submit "$DMG" \
            --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
            --password "$APPLE_APP_PASSWORD" --wait
        xcrun stapler staple "$DMG"
    else
        echo "==> 公証はスキップします（APPLE_ID 一式または APPLE_KEYCHAIN_PROFILE が未設定）"
    fi
else
    echo "==> 公証はスキップします（APPLE_DEVELOPER_ID が未設定）"
fi

# ── 完了 ──────────────────────────────────────────────
echo ""
echo "完了: $DMG"
lipo -info "$APP_BIN"
ls -lh "$DMG"
shasum -a 256 "$DMG"

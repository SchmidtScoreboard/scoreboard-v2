#!/usr/bin/env bash
# Regenerates the Swift bindings + AppCore.xcframework from the Rust `app-core`
# crate, and syncs the bindings into the app target. Re-run after changing app-core.
# Builds BOTH the iOS Simulator (aarch64-apple-ios-sim) and device
# (aarch64-apple-ios) slices, then code-signs the xcframework — Xcode 15+ refuses
# to build against an unsigned xcframework when targeting a device.
set -euo pipefail
cd "$(dirname "$0")/.." # repo root

SIM_TARGET=aarch64-apple-ios-sim
DEV_TARGET=aarch64-apple-ios
GEN=ios/Generated
HDR=ios/headers
XCF=ios/AppCore.xcframework
# Where cargo actually puts artifacts. Hardcoding "target" broke this script for
# anyone with CARGO_TARGET_DIR set (a shared build cache, or a workspace whose
# own target dir isn't writable): cargo built fine and then every path below
# pointed at a directory that stayed empty.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
# Min iOS version for the Rust static lib — must match the app target's
# IPHONEOS_DEPLOYMENT_TARGET, or the linker warns once per object file that the
# lib was built for a newer iOS than it's being linked against.
export IPHONEOS_DEPLOYMENT_TARGET=17.0

echo "==> ensuring iOS targets are installed"
rustup target add "$SIM_TARGET" "$DEV_TARGET" >/dev/null

echo "==> host build (needed for uniffi-bindgen library mode)"
cargo build -p app-core

echo "==> generating Swift bindings"
rm -rf "$GEN"; mkdir -p "$GEN"
cargo run -q -p app-core --bin uniffi-bindgen -- generate \
  --library "$TARGET_DIR/debug/libapp_core.dylib" --language swift --out-dir "$GEN"

echo "==> building static lib for $SIM_TARGET + $DEV_TARGET"
cargo build -p app-core --lib --target "$SIM_TARGET"
cargo build -p app-core --lib --target "$DEV_TARGET"

echo "==> assembling $XCF (simulator + device slices)"
rm -rf "$HDR" "$XCF"; mkdir -p "$HDR"
cp "$GEN/app_coreFFI.h" "$HDR/"
cp "$GEN/app_coreFFI.modulemap" "$HDR/module.modulemap"
xcodebuild -create-xcframework \
  -library "$TARGET_DIR/$SIM_TARGET/debug/libapp_core.a" -headers "$HDR" \
  -library "$TARGET_DIR/$DEV_TARGET/debug/libapp_core.a" -headers "$HDR" \
  -output "$XCF"

# iCloud Drive (this repo lives in ~/Documents) sometimes drops conflict copies
# like "Info 2.plist" inside the bundle, which breaks the codesign seal — scrub
# them before signing.
find "$XCF" -name "* 2.*" -delete 2>/dev/null || true

echo "==> code-signing $XCF (Xcode 15+ requires a signed xcframework for device builds)"
IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null | grep -m1 "Apple Development" | awk '{print $2}' || true)
if [ -n "${IDENTITY:-}" ]; then
  echo "    using Apple Development identity $IDENTITY"
  codesign --timestamp=none --force --sign "$IDENTITY" "$XCF"
else
  echo "    no Apple Development identity found — signing ad-hoc (device may need a real identity)"
  codesign --timestamp=none --force --sign - "$XCF"
fi

echo "==> syncing bindings into the app target"
cp "$GEN/app_core.swift" ios/ScoreboardApp/ScoreboardApp/app_core.swift

echo "done -> open ios/ScoreboardApp/ScoreboardApp.xcodeproj"

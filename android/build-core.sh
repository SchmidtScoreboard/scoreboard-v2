#!/usr/bin/env bash
# Build the shared Rust core for Android and sync the Kotlin bindings —
# the Android sibling of ios/build-core.sh, and like it, THE ONLY correct
# way to refresh what the app links against.
#
#   1. per-ABI libapp_core.so via cargo-ndk (arm64 + x86_64 for the emulator)
#      into core/src/main/jniLibs/
#   2. uniffi Kotlin bindings into core/src/main/kotlin/ (committed;
#      the .so files are not)
#
# Requirements (one-time):
#   rustup target add aarch64-linux-android x86_64-linux-android
#   cargo install cargo-ndk
#   Android NDK r27+ (Android Studio > SDK Manager > NDK), ANDROID_NDK_HOME set
set -euo pipefail
cd "$(dirname "$0")"

command -v cargo-ndk >/dev/null || { echo "error: cargo-ndk not installed (cargo install cargo-ndk)" >&2; exit 1; }
[[ -n "${ANDROID_NDK_HOME:-}" ]] || { echo "error: ANDROID_NDK_HOME not set" >&2; exit 1; }

echo "==> building libapp_core.so (arm64-v8a, x86_64)"
# 16 KB page alignment: required by Play for native libraries as of NDK r27
# era policy — cheaper to carry from day one than to retrofit.
RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384" \
cargo ndk -t arm64-v8a -t x86_64 -o core/src/main/jniLibs \
  build --release -p app-core --manifest-path ../Cargo.toml

echo "==> regenerating Kotlin bindings"
cargo build -q -p app-core --manifest-path ../Cargo.toml
cargo run -q -p app-core --bin uniffi-bindgen --manifest-path ../Cargo.toml -- generate \
  --library ../target/debug/libapp_core.dylib --language kotlin \
  --out-dir core/src/main/kotlin

echo "done -> ./gradlew assembleDebug"

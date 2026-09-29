#!/usr/bin/env bash
# Builds libzecswap.so for the Android module into android/src/main/jniLibs/<abi>/, which is
# committed so the app's build needs no Rust toolchain: release, stripped, and aligned for 16 KB
# pages, which Play requires of apps targeting Android 15+.
#   cargo install cargo-ndk; scripts/build-android.sh
# Uses the NDK zapp-android builds with unless ANDROID_NDK_HOME names another.
set -euo pipefail
cd "$(dirname "$0")/.."

export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$HOME/Library/Android/sdk/ndk/27.0.12077973}"
export CARGO_PROFILE_RELEASE_STRIP=symbols
# Applies to the Android targets only: cargo-ndk passes --target, which keeps it off build scripts.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384"

cargo ndk --platform 27 -t arm64-v8a -t armeabi-v7a -t x86_64 -o android/src/main/jniLibs \
  build -p zecswap-jni --release

readelf="$(ls -d "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-readelf | head -1)"
for library in android/src/main/jniLibs/*/libzecswap.so; do
  if "$readelf" -lW "$library" | awk '$1 == "LOAD" && $NF != "0x4000" { found = 1 } END { exit !found }'; then
    echo "$library has a LOAD segment not aligned to 16 KB" >&2
    exit 1
  fi
  echo "$library: $(du -h "$library" | cut -f1), 16 KB aligned"
done

#!/usr/bin/env bash
set -euo pipefail

: "${ANDROID_SDK_ROOT:?Set ANDROID_SDK_ROOT or export it before running this script}"
: "${JAVA_HOME:?Set JAVA_HOME to JDK 17+}"
: "${CARGO_APK_RELEASE_KEYSTORE:?Set CARGO_APK_RELEASE_KEYSTORE to the release keystore}"
: "${CARGO_APK_RELEASE_KEYSTORE_PASSWORD:?Set CARGO_APK_RELEASE_KEYSTORE_PASSWORD}"

export ANDROID_HOME="${ANDROID_SDK_ROOT}"
export ANDROID_SDK_ROOT

if [[ -z "${ANDROID_NDK_ROOT:-}" ]]; then
    ANDROID_NDK_ROOT="$(find "${ANDROID_SDK_ROOT}/ndk" -mindepth 1 -maxdepth 1 -type d -print | sort -V | tail -n 1)"
    export ANDROID_NDK_ROOT
fi

if [[ -z "${ANDROID_NDK_ROOT:-}" || ! -d "${ANDROID_NDK_ROOT}" ]]; then
    echo "Android NDK was not found under ${ANDROID_SDK_ROOT}/ndk" >&2
    exit 1
fi

export PATH="${JAVA_HOME}/bin:${PATH}"

cargo fmt --all -- --check
cargo apk2 build --release --lib

source_apk="target/release/apk/FastTran.apk"
if [[ ! -f "${source_apk}" ]]; then
    echo "Android build did not produce ${source_apk}" >&2
    exit 1
fi

mkdir -p dist
cp "${source_apk}" dist/FastTran-android-universal.apk
sha256sum dist/FastTran-android-universal.apk \
    | awk '{print $1 "  FastTran-android-universal.apk"}' \
    > dist/FastTran-android-universal.apk.sha256

echo "APK: dist/FastTran-android-universal.apk"
echo "SHA-256: $(cut -d' ' -f1 dist/FastTran-android-universal.apk.sha256)"

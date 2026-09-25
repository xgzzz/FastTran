param(
    [string]$AndroidSdkRoot = $env:ANDROID_SDK_ROOT,
    [string]$JavaHome = $env:JAVA_HOME
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($AndroidSdkRoot)) {
    throw "Set ANDROID_SDK_ROOT or pass -AndroidSdkRoot."
}
if ([string]::IsNullOrWhiteSpace($JavaHome)) {
    throw "Set JAVA_HOME to a JDK 17+ installation or pass -JavaHome."
}
if ([string]::IsNullOrWhiteSpace($env:CARGO_APK_RELEASE_KEYSTORE)) {
    throw "Set CARGO_APK_RELEASE_KEYSTORE to your release keystore."
}
if ([string]::IsNullOrWhiteSpace($env:CARGO_APK_RELEASE_KEYSTORE_PASSWORD)) {
    throw "Set CARGO_APK_RELEASE_KEYSTORE_PASSWORD to the keystore password."
}

$env:ANDROID_HOME = $AndroidSdkRoot
$env:ANDROID_SDK_ROOT = $AndroidSdkRoot
if ([string]::IsNullOrWhiteSpace($env:ANDROID_NDK_ROOT)) {
    $ndkDirectory = Get-ChildItem -LiteralPath (Join-Path $AndroidSdkRoot "ndk") -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending |
        Select-Object -First 1
    if ($null -ne $ndkDirectory) {
        $env:ANDROID_NDK_ROOT = $ndkDirectory.FullName
    }
}
if ([string]::IsNullOrWhiteSpace($env:ANDROID_NDK_ROOT)) {
    $ndkRoot = Join-Path $AndroidSdkRoot "ndk"
    throw "Android NDK was not found. Install it under $ndkRoot or set ANDROID_NDK_ROOT."
}
$env:Path = "$JavaHome\bin;$env:Path"

cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) {
    throw "cargo fmt failed with exit code $LASTEXITCODE."
}

cargo apk2 build --release --lib
if ($LASTEXITCODE -ne 0) {
    throw "cargo apk2 build failed with exit code $LASTEXITCODE."
}

$source = Join-Path $PSScriptRoot "..\target\release\apk\FastTran.apk"
if (-not (Test-Path -LiteralPath $source)) {
    throw "Android build did not produce $source."
}
$dist = Join-Path $PSScriptRoot "..\dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$destination = Join-Path $dist "FastTran-android-universal.apk"
Copy-Item -LiteralPath $source -Destination $destination -Force
$hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  FastTran-android-universal.apk" |
    Set-Content -LiteralPath "$destination.sha256" -Encoding ascii

Write-Host "APK: $destination"
Write-Host "SHA-256: $hash"

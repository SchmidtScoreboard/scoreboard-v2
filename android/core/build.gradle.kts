// The shared Rust core, as an Android library: the uniffi-generated Kotlin
// bindings (committed, refreshed by ../build-core.sh) over per-ABI
// libapp_core.so files in src/main/jniLibs (produced by the same script,
// not committed). This module is the ONE place the app touches the core.
plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
}

android {
    namespace = "uniffi.app_core"
    compileSdk = 35
    defaultConfig { minSdk = 26 }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    // The @aar variant bundles JNA's own per-ABI native dispatcher, which the
    // plain jar does not — without it the bindings die at Native.load on
    // device.
    api("${libs.jna.get()}@aar")
}

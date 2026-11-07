import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

// The Play upload key lives outside the repo, beside the board and firmware
// keys: ~/.config/scoreboard/play-upload.properties (storeFile, storePassword,
// keyAlias, keyPassword), or wherever $SCOREBOARD_PLAY_UPLOAD points. NEVER
// commit it. Google re-signs for the store (Play App Signing), so a lost
// upload key can be reset in Play Console — but only by its owner.
val playUpload: Properties? =
    file(System.getenv("SCOREBOARD_PLAY_UPLOAD") ?: "${System.getProperty("user.home")}/.config/scoreboard/play-upload.properties")
        .takeIf { it.isFile }
        ?.let { f -> Properties().apply { f.inputStream().use { load(it) } } }

android {
    signingConfigs {
        if (playUpload != null) {
            create("upload") {
                storeFile = file(playUpload.getProperty("storeFile"))
                storePassword = playUpload.getProperty("storePassword")
                keyAlias = playUpload.getProperty("keyAlias")
                keyPassword = playUpload.getProperty("keyPassword")
            }
        }
    }
    namespace = "com.markschmidt.scoreboard"
    // Play's target-API floor is 36 (Android 16) for new uploads.
    compileSdk = 36
    // The NDK strips libapp_core.so for packaging and extracts its symbol
    // table into the bundle for Play's crash reports. Same NDK build-core.sh
    // builds the core with; without it the libs ship unstripped.
    ndkVersion = "27.2.12479018"
    defaultConfig {
        // The Play package name, permanent once uploaded; matches the iOS
        // bundle id. `namespace` (the Kotlin package) stays as it was.
        applicationId = "com.markschmidt.openscoreboard"
        minSdk = 26
        targetSdk = 36
        // versionCode must rise with every Play upload — including uploads
        // Play rejected: 3 went up targeting API 35 and is spent.
        versionCode = 4
        versionName = "2026.10.0"
        // build-core.sh builds the Rust core for these two ABIs only, while
        // JNA carries jnidispatch for every ABI. Unfiltered, Play would offer
        // the app to 32-bit phones that cannot load libapp_core.
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildTypes {
        release {
            // R8 on. Signed with the Play upload key when this machine has it
            // (`./gradlew bundleRelease` is then the store build); without
            // it, the debug key, so `run.sh --release` still installs on a
            // personal device. Play rejects a debug-signed upload outright,
            // so the fallback cannot reach the store by mistake.
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.findByName("upload") ?: signingConfigs.getByName("debug")
            // The Rust core's symbols ride in the bundle (BUNDLE-METADATA), so
            // Play symbolicates native crashes without a separate upload.
            ndk { debugSymbolLevel = "SYMBOL_TABLE" }
        }
    }
    buildFeatures {
        compose = true
        // Settings needs BuildConfig.DEBUG for the dev-channel gate.
        buildConfig = true
    }
}

dependencies {
    implementation(project(":core"))
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime)
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.material3)
    // The full Material icon set (PushPin, DarkMode, …). Debug builds carry
    // it whole; R8 strips the unused ones from release.
    implementation("androidx.compose.material:material-icons-extended")
    implementation(libs.compose.ui)
    implementation(libs.security.crypto)
    implementation(libs.coroutines.core)
    // Settings › Screens: drag to reorder (owner decision, MAR-117 Q4).
    implementation(libs.reorderable)
}

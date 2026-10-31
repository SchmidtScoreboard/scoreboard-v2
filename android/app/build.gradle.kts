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
    compileSdk = 35
    defaultConfig {
        // The Play package name, permanent once uploaded; matches the iOS
        // bundle id. `namespace` (the Kotlin package) stays as it was.
        applicationId = "com.markschmidt.openscoreboard"
        minSdk = 26
        targetSdk = 35
        // versionCode must rise with every Play upload.
        versionCode = 3
        versionName = "2026.10.0"
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

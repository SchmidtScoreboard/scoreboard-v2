plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "com.markschmidt.scoreboard"
    compileSdk = 35
    defaultConfig {
        applicationId = "com.markschmidt.scoreboard"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "2026.8.0"
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
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
}

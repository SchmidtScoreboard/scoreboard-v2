plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.android.library) apply false
    alias(libs.plugins.kotlin.android) apply false
    alias(libs.plugins.kotlin.compose) apply false
}

// BUILD OUTPUT LIVES OUTSIDE THE REPO, and this is not a preference.
//
// The repo sits in ~/Documents, which iCloud Drive syncs (deliberately — the
// owner wants the SOURCE synced). iCloud resolves its own races by duplicating
// files with a " 2"/" 3" suffix, and it does that to build intermediates too:
// a sweep found 1,254 of them under android/build. Gradle then dies in ways
// that read as code errors and are not —
//
//   "Type ...$1 is defined multiple times: .../out/...$1 2.dex"
//   "Failed file name validation for file .../xml/file_paths 3.xml"
//
// ~/Library/Caches is never synced, so putting the build tree there ends the
// whole class of failure rather than cleaning up after it. (firmware-rs
// reached for the same escape with its `target.nosync` directory.)
val buildRoot = file("${System.getProperty("user.home")}/Library/Caches/scoreboard-android")
allprojects {
    layout.buildDirectory.set(File(buildRoot, project.path.replace(":", "-").trim('-').ifEmpty { "root" }))
}

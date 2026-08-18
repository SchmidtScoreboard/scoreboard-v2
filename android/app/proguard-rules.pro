# JNA reaches the native library reflectively; uniffi's generated bindings
# are looked up by name from Rust. Strip either and the app dies at first
# FFI call with a message that looks nothing like this file.
-keep class com.sun.jna.** { *; }
-keep class uniffi.app_core.** { *; }
-keepclassmembers class * implements com.sun.jna.Library { *; }
-keepclassmembers class * implements com.sun.jna.Callback { *; }
-dontwarn java.awt.**

# Tink (inside EncryptedSharedPreferences) references errorprone's
# compile-time-only annotations; they never exist at runtime.
-dontwarn com.google.errorprone.annotations.**
-dontwarn javax.annotation.**

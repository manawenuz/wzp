# WZPhone ProGuard rules

# Keep JNI native methods
-keepclasseswithmembernames class * {
    native <methods>;
}

# Keep the WZP engine bridge class
-keep class com.wzp.phone.engine.** { *; }

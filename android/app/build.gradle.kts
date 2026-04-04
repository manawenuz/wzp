plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.wzp.phone"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.wzp.phone"
        minSdk = 26  // AAudio requires API 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    signingConfigs {
        create("release") {
            storeFile = file("${project.rootDir}/keystore/wzp-release.jks")
            storePassword = "wzphone2024"
            keyAlias = "wzp-release"
            keyPassword = "wzphone2024"
        }
        getByName("debug") {
            storeFile = file("${project.rootDir}/keystore/wzp-debug.jks")
            storePassword = "android"
            keyAlias = "wzp-debug"
            keyPassword = "android"
        }
    }

    buildTypes {
        debug {
            signingConfig = signingConfigs.getByName("debug")
            isDebuggable = true
        }
        release {
            signingConfig = signingConfigs.getByName("release")
            isMinifyEnabled = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    kotlinOptions {
        jvmTarget = "1.8"
    }

    buildFeatures { compose = true }
    composeOptions { kotlinCompilerExtensionVersion = "1.5.8" }

    ndkVersion = "26.1.10909125"
}

// cargo-ndk integration: build the Rust native library for Android targets
tasks.register<Exec>("cargoNdkBuild") {
    workingDir = file("${project.rootDir}/..")
    commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-o", "${project.projectDir}/src/main/jniLibs",
        "build", "--release", "-p", "wzp-android"
    )
}

tasks.named("preBuild") { dependsOn("cargoNdkBuild") }

dependencies {
    implementation("androidx.core:core-ktx:1.12.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.7.0")
    implementation("androidx.activity:activity-compose:1.8.2")
    implementation(platform("androidx.compose:compose-bom:2024.01.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
}

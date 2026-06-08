plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.udp2cal.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.udp2cal.app"
        minSdk = 21
        targetSdk = 26
        versionCode = 6
        versionName = "1.1.0"

        ndk {
            // -PtargetAbi=armeabi-v7a | arm64-v8a (default)
            val targetAbi = project.findProperty("targetAbi") as? String ?: "arm64-v8a"
            abiFilters += listOf(targetAbi)
        }

        externalNativeBuild {
            cmake {
                arguments += listOf(
                    "-DANDROID_STL=c++_static",
                    "-DCMAKE_BUILD_TYPE=MinSizeRel"
                )
            }
        }
    }

    signingConfigs {
        create("release") {
            storeFile = file("../lanmic-debug.keystore")
            storePassword = "android"
            keyAlias = "lanmic"
            keyPassword = "android"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            signingConfig = signingConfigs.getByName("release")
        }
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.22.1"
        }
    }

    lint {
        checkReleaseBuilds = false
        abortOnError = false
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }

    kotlinOptions {
        jvmTarget = "11"
    }
}

dependencies {
    implementation("androidx.appcompat:appcompat:1.6.1")
    implementation("androidx.core:core-ktx:1.12.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.7.0")
    implementation("androidx.constraintlayout:constraintlayout:2.1.4")
}

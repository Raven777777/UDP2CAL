import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// ── 发布签名 ─────────────────────────────────────────────────────────
// 密钥库路径与口令一律来自 local.properties 或环境变量，**禁止写进仓库**。
// 历史上此处直接引用了入库的 lanmic-debug.keystore 且口令为 "android"，
// 任何人都能用同一签名构造可覆盖安装的 APK。
val signingProps = Properties().apply {
    val f = rootProject.file("local.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}

fun signingValue(key: String, env: String): String? =
    (signingProps.getProperty(key) ?: System.getenv(env))?.trim()?.takeIf { it.isNotEmpty() }

val releaseStoreFile = signingValue("RELEASE_STORE_FILE", "UDP2CAL_STORE_FILE")
    ?.let { rootProject.file(it) }
    ?.takeIf { it.exists() }

/** android {} 块内无法直接访问 Project.logger，先取出来 */
val scriptLogger = logger

android {
    namespace = "com.udp2cal.app"
    compileSdk = 35
    ndkVersion = "27.0.12077973"

    defaultConfig {
        applicationId = "com.udp2cal.app"
        minSdk = 29
        targetSdk = 35
        versionCode = 8
        versionName = "1.3.0"

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
        if (releaseStoreFile != null) {
            create("release") {
                storeFile = releaseStoreFile
                storePassword = signingValue("RELEASE_STORE_PASSWORD", "UDP2CAL_STORE_PASSWORD")
                keyAlias = signingValue("RELEASE_KEY_ALIAS", "UDP2CAL_KEY_ALIAS")
                keyPassword = signingValue("RELEASE_KEY_PASSWORD", "UDP2CAL_KEY_PASSWORD")
            }
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
            // 发布密钥来自 local.properties / 环境变量；
            // 未配置时回落到**本机**调试密钥（~/.android/debug.keystore，不入库），
            // 保证 `build.bat android --release` 产出的 APK 可直接安装自测。
            val releaseSigning = signingConfigs.findByName("release")
            if (releaseSigning == null) {
                scriptLogger.warn(
                    "UDP2CAL: 未配置发布密钥（RELEASE_STORE_FILE / UDP2CAL_STORE_FILE），" +
                    "本次 Release 使用本机调试签名，仅供自测；正式分发前请先配置密钥库。"
                )
            }
            signingConfig = releaseSigning ?: signingConfigs.getByName("debug")
        }
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.22.1"
        }
    }

    lint {
        // lint 门禁重新启用：此前 checkReleaseBuilds/abortOnError 全关，
        // 正是清单里未使用权限、签名配置等问题没被拦下的原因
        // 恢复"发布构建必跑 lint"；abortOnError 先保持非阻断，
        // 待首次全绿后再改为 true 作为硬门禁
        checkReleaseBuilds = true
        abortOnError = false
        warningsAsErrors = false
        // NullSafeMutableLiveData 在 AGP 8.7 + Kotlin 2.0 下会崩：
        //   IncompatibleClassChangeError: KaCallableMemberCall (interface → class)
        // 属 lint 与 Kotlin 分析 API 的版本不兼容（Google issue 374783344），
        // 与本项目代码无关——工程为纯 Compose，无任何 LiveData 使用。
        // 待 AGP 升级到 8.8.2+（lint 31.8.2+）后删除此行即可恢复该检查。
        disable += "NullSafeMutableLiveData"
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.05.00"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.activity:activity-compose:1.9.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.0")

    // 显式声明：NotificationCompat / ContextCompat / ServiceCompat 均来自 androidx.core，
    // 此前仅靠 Compose 的传递依赖引入，上游一旦裁剪就会直接编译失败。
    implementation("androidx.core:core-ktx:1.16.0")

    // 协议层 JVM 单测（ReorderBuffer 的乱序/重复/跳变时序与跨端常量一致性）
    testImplementation("junit:junit:4.13.2")

    // (TFLite 降噪已移除)
}

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

// ── 协议层：Rust 单源（v1.4.0 之后的 L1） ─────────────────────────────
// Kotlin 不再持有 Udp2CalProtocol 的实现，只通过 JNI 调 protocol crate。
// 两个产物：
//   * Android: libudp2cal_protocol.so（按 -PtargetAbi 交叉编译，NDK clang 做链接器）
//   * 桌面 JVM: udp2cal_protocol.dll（`cargo build --release --features jni`），
//     让现有 59 个 Kotlin 单测能在 JVM 上原样跑——等于给 JNI 层做一致性回归。
// 仓库布局：`android/` 是 Gradle 根，Rust crate 在它的同级 `protocol/`
val protocolDir = rootProject.file("../protocol").canonicalFile
val ndkVersionStr = "27.0.12077973"

/**
 * cargo 可执行文件：Gradle 从 Git Bash 继承的 PATH 是 POSIX 路径（`/c/...`），
 * Java 的 `ProcessBuilder` 无法解析，所以直接定位 `~/.cargo/bin`。
 */
fun cargoExe(): String {
    System.getenv("CARGO")?.takeIf { it.isNotBlank() }?.let { return File(it).absolutePath }
    val home = System.getProperty("user.home")
    listOf("$home/.cargo/bin/cargo.exe", "$home/.cargo/bin/cargo")
        .firstOrNull { File(it).isFile }
        ?.let { return File(it).absolutePath }
    // 自定义 CARGO_HOME / 非常规安装：从 PATH 里逐个目录找
    val exe = if (System.getProperty("os.name").lowercase().contains("win")) "cargo.exe" else "cargo"
    System.getenv("PATH")
        ?.split(File.pathSeparatorChar)
        ?.filter { it.isNotBlank() }
        ?.map { File(it, exe) }
        ?.firstOrNull { it.isFile }
        ?.let { return it.absolutePath }
    throw GradleException(
        "找不到 cargo：请安装 Rust（https://rustup.rs）并把 ~/.cargo/bin 加入 PATH，" +
            "或设 CARGO=<cargo 绝对路径>"
    )
}

/** NDK 根目录：sdk.dir / ANDROID_HOME / LOCALAPPDATA 逐个回退 */
fun cargoBuildArgs(vararg extra: String): List<String> {
    val args = mutableListOf(cargoExe(), "build", "--release", "--features", "jni")
    // 首次构建要拉 jni-sys，默认联网；离线环境（缓存已暖）加 `-PcargoOffline`
    if (project.providers.gradleProperty("cargoOffline").map { it.toBoolean() }.getOrElse(false)) {
        args += "--offline"
    }
    args += extra
    return args
}

fun ndkRoot(): File {
    val sdk = (signingProps.getProperty("sdk.dir")
        ?: System.getenv("ANDROID_HOME")
        ?: System.getenv("ANDROID_SDK_ROOT")
        ?: "${System.getenv("LOCALAPPDATA")}/Android/Sdk")
    val ndk = File(sdk, "ndk/$ndkVersionStr")
    if (!ndk.isDirectory) {
        throw GradleException(
            "找不到 NDK：$ndk\n" +
                "请用 SDK Manager 安装 NDK $ndkVersionStr（与 android { ndkVersion } 一致）"
        )
    }
    return ndk
}

/** 当前主机的 NDK 预编译工具链目录名 */
fun ndkHostTag(): String {
    val os = System.getProperty("os.name").lowercase()
    return when {
        os.contains("win") -> "windows-x86_64"
        os.contains("mac") -> "darwin-x86_64"
        else -> "linux-x86_64"
    }
}

val targetAbiName = (project.findProperty("targetAbi") as? String) ?: "arm64-v8a"
val rustAbi = mapOf(
    "arm64-v8a" to Triple("aarch64-linux-android", "aarch64-linux-android", "arm64-v8a"),
    "armeabi-v7a" to Triple("armv7-linux-androideabi", "armv7a-linux-androideabi", "armeabi-v7a")
)[targetAbiName]
    ?: throw GradleException("targetAbi=$targetAbiName 不受支持（仅 arm64-v8a / armeabi-v7a）")

val cargoBuildAndroidProtocol = tasks.register<Exec>("cargoBuildAndroidProtocol") {
    val ndk = ndkRoot()
    val toolchain = File(ndk, "toolchains/llvm/prebuilt/${ndkHostTag()}")
    val clangName = if (ndkHostTag().startsWith("windows")) "clang.exe" else "clang"
    val clang = File(toolchain, "bin/$clangName")
    // NDK r23+ 的 sysroot 在 toolchain 里，不在 ndk 根下
    val sysroot = File(toolchain, "sysroot")
    if (!clang.isFile || !sysroot.isDirectory) {
        throw GradleException("NDK 工具链不完整：$clang / $sysroot（ndkVersion=$ndkVersionStr）")
    }
    val rustTriple = rustAbi.first
    val clangTriple = rustAbi.second
    // 与 minSdk 对齐的 API 级别（交叉编译的链接目标）
    val androidApi = 29
    workingDir = protocolDir
    // NDK 里的 <triple>29-clang 是 sh 脚本 / .cmd，Rust 无法直接 spawn；
    // 用 clang.exe + --target/--sysroot 等价实现。
    environment(
        "CARGO_TARGET_${rustTriple.uppercase().replace('-', '_')}_LINKER",
        clang.absolutePath
    )
    // 用 per-target 变量，避免覆盖用户自己设的 RUSTFLAGS（那会作用到所有 crate 并丢掉 -C target-cpu 之类）
    environment(
        "CARGO_TARGET_${rustTriple.uppercase().replace('-', '_')}_RUSTFLAGS",
        "-C link-arg=--target=$clangTriple$androidApi" +
            " -C link-arg=--sysroot=${sysroot.absolutePath}"
    )
    commandLine(cargoBuildArgs("--target", rustTriple))
}

val syncRustJniLibs = tasks.register<Sync>("syncRustJniLibs") {
    dependsOn(cargoBuildAndroidProtocol)
    from(File(protocolDir, "target/${rustAbi.first}/release")) {
        include("libudp2cal_protocol.so")
    }
    into(layout.buildDirectory.dir("rustJniLibs/${rustAbi.third}"))
}

val cargoBuildHostProtocol = tasks.register<Exec>("cargoBuildHostProtocol") {
    workingDir = protocolDir
    commandLine(cargoBuildArgs())
}

tasks.withType<Test>().configureEach {
    dependsOn(cargoBuildHostProtocol)
    // System.loadLibrary("udp2cal_protocol") 从 java.library.path 找 dll
    systemProperty("java.library.path", File(protocolDir, "target/release").absolutePath)
}

tasks.named("preBuild").configure { dependsOn(syncRustJniLibs) }

android {
    namespace = "com.udp2cal.app"
    compileSdk = 35
    ndkVersion = ndkVersionStr

    defaultConfig {
        applicationId = "com.udp2cal.app"
        minSdk = 29
        targetSdk = 35
        versionCode = 10
        versionName = "1.5.0"

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

    sourceSets["main"].jniLibs.srcDir(layout.buildDirectory.dir("rustJniLibs").get().asFile)
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

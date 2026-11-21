fn main() {
    // 动态查找 Windows Kits rc.exe 路径
    if let Some(kits_bin) = find_rc_path() {
        let current_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: 构建脚本单线程执行，此处改 PATH 无人并发读取。
        unsafe { std::env::set_var("PATH", format!("{};{}", kits_bin, current_path)) };
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let ico_path = std::path::Path::new(&out_dir).join("icon.ico");
    let png_path = std::path::Path::new("icon.png");

    // PNG -> ICO (256x256)
    if png_path.exists()
        && let Ok(img) = image::open(png_path) {
            let resized = img.resize_exact(256, 256, image::imageops::FilterType::Lanczos3);
            let _ = resized.save(&ico_path);
        }

    // 嵌入 ICO 到 EXE 资源
    if ico_path.exists() {
        let mut res = winres::WindowsResource::new();
        res.set_icon(ico_path.to_str().unwrap());
        let _ = res.compile();
    }

    generate_embedded_langs();
}

/// 扫描 lang/*.lang 生成内嵌语言表：新增语言只需放入文件并重新编译，无需改 Rust 代码。
fn generate_embedded_langs() {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir("lang") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("lang") {
                files.push(path);
            }
        }
    }
    files.sort();

    // 语言文件是源码的一部分；缺失时必须在构建期失败，否则 exe 会直接
    // 把 key（如 notice.port_invalid）显示给用户。
    let has_english = files.iter().any(|path| {
        path.file_stem().and_then(|stem| stem.to_str()) == Some("en-US")
    });
    assert!(has_english, "缺少 lang/en-US.lang：请确认 windows/lang 已随源码提交");

    let mut code = String::from("pub static EMBEDDED_LANGS: &[(&str, &str)] = &[\n");
    for path in &files {
        let code_tag = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        code.push_str(&format!(
            "    ({code_tag:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/lang/{file_name}\"))),\n"
        ));
    }
    code.push_str("];\n");
    std::fs::write(out_dir.join("embedded_langs.rs"), code).unwrap();
    println!("cargo:rerun-if-changed=lang");
}

/// 自动检测 Windows Kits SDK 路径，查找 rc.exe
fn find_rc_path() -> Option<String> {
    // 1. 优先从环境变量获取（VS 开发者命令行自动设置）
    if let (Ok(sdk_dir), Ok(sdk_ver)) = (
        std::env::var("WindowsSdkDir"),
        std::env::var("WindowsSDKVersion"),
    ) {
        let ver = sdk_ver.trim_end_matches('\\');
        let dir = sdk_dir.trim_end_matches('\\');
        for arch in &["x64", "x86", "arm64"] {
            let path = format!("{}\\bin\\{}\\{}", dir, ver, arch);
            if std::path::Path::new(&format!("{}\\rc.exe", path)).exists() {
                return Some(path);
            }
        }
    }

    // 2. 扫描默认安装目录，取最新版本
    let base_dirs = &[
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    for base in base_dirs {
        if let Ok(entries) = std::fs::read_dir(base) {
            let mut versions: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .filter(|s| {
                    s.split('.')
                        .next()
                        .is_some_and(|n| n.parse::<u32>().is_ok())
                })
                .collect();
            // 按版本号降序排列，取最新
            versions.sort_by(|a, b| {
                fn parse_ver(v: &str) -> Vec<u32> {
                    v.split('.').filter_map(|n| n.parse().ok()).collect()
                }
                parse_ver(b).cmp(&parse_ver(a))
            });
            for ver in &versions {
                for arch in &["x64", "x86", "arm64"] {
                    let path = format!("{}\\{}\\{}", base, ver, arch);
                    if std::path::Path::new(&format!("{}\\rc.exe", path)).exists() {
                        return Some(path);
                    }
                }
            }
        }
    }

    None
}

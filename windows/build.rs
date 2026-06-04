fn main() {
    // 添加 Windows Kits rc.exe 路径到 PATH
    let kits_bin = r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64";
    if std::path::Path::new(&format!("{}\\rc.exe", kits_bin)).exists() {
        let current_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{};{}", kits_bin, current_path));
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let ico_path = std::path::Path::new(&out_dir).join("icon.ico");
    let png_path = std::path::Path::new("icon.png");

    // PNG -> ICO (256x256)
    if png_path.exists() {
        if let Ok(img) = image::open(png_path) {
            let resized = img.resize_exact(256, 256, image::imageops::FilterType::Lanczos3);
            let _ = resized.save(&ico_path);
        }
    }

    // 嵌入 ICO 到 EXE 资源
    if ico_path.exists() {
        let mut res = winres::WindowsResource::new();
        res.set_icon(ico_path.to_str().unwrap());
        let _ = res.compile();
    }
}

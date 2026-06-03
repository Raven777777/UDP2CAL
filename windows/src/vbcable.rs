// VB-Cable检测
pub fn is_installed() -> bool {
    crate::audio::detect_vb_cable().is_some()
}

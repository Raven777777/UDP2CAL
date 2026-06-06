// 配置管理 - 注册表读写
use winreg::enums::*;
use winreg::RegKey;

const REG_PATH: &str = "Software\\UDP2Mic";

#[derive(Debug, Clone)]
pub struct Config {
    pub listen_ip: String,
    pub listen_port: u32,
    pub auto_start: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_ip: "0.0.0.0".into(),
            listen_port: 44044,
            auto_start: 0,
        }
    }
}

impl Config {
    pub fn is_auto_start(&self) -> bool {
        self.auto_start != 0
    }

    pub fn load() -> Self {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path = match hkcu.open_subkey_with_flags(REG_PATH, KEY_READ) {
            Ok(k) => k,
            Err(_) => return Self::default(),
        };
        Self {
            listen_ip: get_string(&path, "listen_ip", "0.0.0.0"),
            listen_port: get_dword(&path, "listen_port", 44044),
            auto_start: get_dword(&path, "auto_start", 0),
        }
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(REG_PATH)?;
        key.set_value("listen_ip", &self.listen_ip)?;
        key.set_value("listen_port", &self.listen_port)?;
        key.set_value("auto_start", &self.auto_start)?;
        Ok(())
    }

    pub fn set_auto_start(&mut self, enable: bool) {
        self.auto_start = if enable { 1 } else { 0 };
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(run_key) = hkcu.open_subkey_with_flags(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            KEY_SET_VALUE,
        ) {
            if enable {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = run_key.set_value("UDP2Mic", &exe.to_string_lossy().to_string());
                }
            } else {
                let _ = run_key.delete_value("UDP2Mic");
            }
        }
    }
}

fn get_string(key: &RegKey, name: &str, default: &str) -> String {
    key.get_value(name).unwrap_or_else(|_| default.to_string())
}

fn get_dword(key: &RegKey, name: &str, default: u32) -> u32 {
    key.get_value(name).unwrap_or(default)
}
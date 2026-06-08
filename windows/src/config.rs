// 配置管理 - 注册表读写
use winreg::enums::*;
use winreg::RegKey;

const REG_PATH: &str = "Software\\UDP2CAL";

#[derive(Debug, Clone)]
pub struct Config {
    pub listen_ip: String,
    pub listen_port: u32,
    pub auto_start: u32,
    pub device_id: u64,
    pub reverse_enabled: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_ip: "0.0.0.0".into(),
            listen_port: 44044,
            auto_start: 0,
            device_id: 0,
            reverse_enabled: 0,
        }
    }
}

impl Config {
    pub fn set_reverse_enabled(&mut self, enable: bool) {
        self.reverse_enabled = if enable { 1 } else { 0 };
    }

    pub fn is_auto_start(&self) -> bool {
        self.auto_start != 0
    }

    pub fn get_device_id(&self) -> u64 {
        if self.device_id == 0 {
            // 首次运行，生成设备ID
            let id_bytes = udp2cal_protocol::generate_device_id();
            u64::from_be_bytes(id_bytes)
        } else {
            self.device_id
        }
    }

    pub fn get_device_id_bytes(&self) -> [u8; 8] {
        self.get_device_id().to_be_bytes()
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
            device_id: get_qword(&path, "device_id", 0),
            reverse_enabled: get_dword(&path, "reverse_enabled", 0),
        }
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(REG_PATH)?;
        key.set_value("listen_ip", &self.listen_ip)?;
        key.set_value("listen_port", &self.listen_port)?;
        key.set_value("auto_start", &self.auto_start)?;
        key.set_value("device_id", &self.get_device_id())?;
        key.set_value("reverse_enabled", &self.reverse_enabled)?;
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
                    let _ = run_key.set_value("UDP2CAL", &exe.to_string_lossy().to_string());
                }
            } else {
                let _ = run_key.delete_value("UDP2CAL");
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

fn get_qword(key: &RegKey, name: &str, default: u64) -> u64 {
    key.get_value(name).unwrap_or(default)
}

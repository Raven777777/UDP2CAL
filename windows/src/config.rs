// 配置管理 - 注册表读写
use std::sync::OnceLock;
use winreg::enums::*;
use winreg::RegKey;

const REG_PATH: &str = "Software\\UDP2CAL";
const RUN_VALUE: &str = "UDP2CAL";

#[derive(Debug, Clone)]
pub struct Config {
    pub listen_ip: String,
    pub listen_port: u32,
    pub auto_start: u32,
    pub device_id: u64,
    pub reverse_enabled: u32,
}

/// 进程级设备 ID 缓存。
/// 生成动作必须**只发生一次**：历史上 `get_device_id()` 每次遇到 device_id==0
/// 都会生成新的随机值，导致每次 save() 都在改写设备身份。
static DEVICE_ID: OnceLock<u64> = OnceLock::new();

/// 默认监听端口（与 protocol::DEFAULT_LISTEN_PORT 一致）
const DEFAULT_LISTEN_PORT: u32 = 44044;

/// 端口合法区间。注册表里的 DWORD 是外部输入（可手改、可被旧版本写入）：
/// 超出 1..=65535 的值必须在加载时归一，因为下游有多处 `as u16` 截断
/// （DISCOVER_REPLY 广播、防火墙规则），截断后的回绕端口与实际 bind 用值
/// 不一致：广播出去的是错端口、防火墙放行的是错端口，而 bind 反而失败。
fn sanitize_port(v: u32) -> u32 {
    if (1..=65535).contains(&v) {
        v
    } else {
        DEFAULT_LISTEN_PORT
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_ip: "0.0.0.0".into(),
            listen_port: DEFAULT_LISTEN_PORT,
            auto_start: 0,
            device_id: 0,
            reverse_enabled: 0,
        }
    }
}

/// 读取或首次生成并落盘设备 ID（线程安全、幂等）
fn resolve_device_id(stored: u64) -> u64 {
    *DEVICE_ID.get_or_init(|| {
        if stored != 0 {
            return stored;
        }
        let id = u64::from_be_bytes(udp2cal_protocol::generate_device_id());
        // 首次生成立即持久化，避免多个线程各自生成不同 ID
        if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(REG_PATH) {
            let _ = key.set_value("device_id", &id);
        }
        id
    })
}

impl Config {
    pub fn set_reverse_enabled(&mut self, enable: bool) {
        self.reverse_enabled = if enable { 1 } else { 0 };
    }

    pub fn is_auto_start(&self) -> bool {
        self.auto_start != 0
    }

    pub fn get_device_id(&self) -> u64 {
        if self.device_id != 0 {
            self.device_id
        } else {
            resolve_device_id(0)
        }
    }

    pub fn get_device_id_bytes(&self) -> [u8; 8] {
        self.get_device_id().to_be_bytes()
    }

    pub fn load() -> Self {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path = match hkcu.open_subkey_with_flags(REG_PATH, KEY_READ) {
            Ok(k) => k,
            Err(_) => {
                // 无注册表项时仍需返回一个稳定的设备 ID
                return Self {
                    device_id: resolve_device_id(0),
                    ..Self::default()
                };
            }
        };
        let stored_id = get_qword(&path, "device_id", 0);
        Self {
            listen_ip: get_string(&path, "listen_ip", "0.0.0.0"),
            listen_port: sanitize_port(get_dword(&path, "listen_port", DEFAULT_LISTEN_PORT)),
            auto_start: get_dword(&path, "auto_start", 0),
            device_id: resolve_device_id(stored_id),
            reverse_enabled: get_dword(&path, "reverse_enabled", 0),
        }
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(REG_PATH)?;
        key.set_value("listen_ip", &self.listen_ip)?;
        key.set_value("listen_port", &self.listen_port)?;
        key.set_value("auto_start", &self.auto_start)?;
        // 注意：写回的是已缓存的稳定 ID，不会重新生成
        key.set_value("device_id", &self.get_device_id())?;
        key.set_value("reverse_enabled", &self.reverse_enabled)?;
        Ok(())
    }

    /// 写入/删除开机自启项。
    /// 返回 Err 表示注册表写入失败（调用方必须据此回滚 UI 开关）。
    pub fn set_auto_start(&mut self, enable: bool) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run_key = hkcu
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                KEY_SET_VALUE,
            )
            .map_err(|e| format!("打开自启注册表失败: {e}"))?;

        if enable {
            let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败: {e}"))?;
            // 必须加引号：路径含空格时未加引号的 Run 项会被系统截断解析
            let quoted = format!("\"{}\"", exe.display());
            run_key
                .set_value(RUN_VALUE, &quoted)
                .map_err(|e| format!("写入自启注册表失败（可能被杀毒软件拦截）: {e}"))?;
            self.auto_start = 1;
        } else {
            match run_key.delete_value(RUN_VALUE) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("删除自启注册表失败: {e}")),
            }
            self.auto_start = 0;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_port_keeps_valid_and_normalizes_invalid() {
        for p in [1u32, 80, 44044, 65535] {
            assert_eq!(sanitize_port(p), p, "合法端口必须原样保留");
        }
        // 0 / 越界 / 旧版本可能写入的怪值 → 回默认值
        for p in [0u32, 65536, 70000, u32::MAX] {
            assert_eq!(
                sanitize_port(p),
                DEFAULT_LISTEN_PORT,
                "非法端口 {p} 必须归一为默认值"
            );
        }
        // 归一后的值必须能安全地截断成 u16
        assert_eq!(sanitize_port(u32::MAX) as u16, DEFAULT_LISTEN_PORT as u16);
    }
}

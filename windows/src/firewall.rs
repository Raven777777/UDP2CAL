// Windows防火墙自动放行 (静默，无CMD窗口)
use std::os::windows::process::CommandExt;
const CREATE_NO_WINDOW: u32 = 0x08000000;
const RULE_NAME: &str = "UDP2Mic 局域网麦克风";

pub fn add_firewall_rule() -> Result<(), String> {
    let check = std::process::Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", &format!("name={RULE_NAME}")])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("netsh: {e}"))?;

    if String::from_utf8_lossy(&check.stdout).contains("Enabled:") {
        return Ok(());
    }

    let result = std::process::Command::new("netsh")
        .args([
            "advfirewall", "firewall", "add", "rule",
            &format!("name={RULE_NAME}"),
            "dir=in", "action=allow", "protocol=UDP",
            "localport=8899", "enable=yes", "profile=any",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("防火墙: {e}"))?;

    if result.status.success() {
        Ok(())
    } else {
        Err("防火墙规则添加失败，可能需要管理员权限".into())
    }
}

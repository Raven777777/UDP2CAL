// Windows 防火墙自动放行（静默，无 cmd 窗口）
//
// 安全要点：规则限定 program + 具体端口 + Private profile；不向公共网络开放。
// 历史实现为整机开放所有入站 UDP，远超功能所需。
//
// 命名要点：netsh 使用自己的命令行解析器，含空格的 name 需要额外引号，
// 极易被截断成另一条规则（进而每次启动都"删不掉旧的、再加一条新的"）。
// 因此规则名保持 ASCII 且不含空格。

use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const RULE_NAME: &str = "UDP2CAL-In-UDP";
/// 发现服务固定端口
const DISCOVERY_PORT: u16 = 44043;

fn netsh(args: &[String]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("netsh")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
}

/// 添加/更新放行规则。
/// `audio_port` 为实际监听端口；`program` 为当前可执行文件路径。
pub fn add_firewall_rule(audio_port: u16) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败: {e}"))?;
    let program = format!("program=\"{}\"", exe.display());
    let ports = format!("localport={DISCOVERY_PORT},{audio_port}");

    // 先删除旧规则（忽略失败，可能本来就不存在）
    let _ = netsh(&[
        "advfirewall".into(), "firewall".into(), "delete".into(), "rule".into(),
        format!("name={RULE_NAME}"),
    ]);

    let result = netsh(&[
        "advfirewall".into(), "firewall".into(), "add".into(), "rule".into(),
        format!("name={RULE_NAME}"),
        "dir=in".into(), "action=allow".into(), "protocol=UDP".into(),
        ports,
        program,
        "enable=yes".into(), "profile=private".into(),
    ])
    .map_err(|e| format!("防火墙命令执行失败: {e}"))?;

    if result.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&result.stderr);
        let detail = detail.trim();
        Err(if detail.is_empty() {
            "防火墙规则添加失败（需要管理员权限）".to_string()
        } else {
            format!("防火墙规则添加失败（需要管理员权限）: {detail}")
        })
    }
}

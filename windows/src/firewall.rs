// Windows 防火墙自动放行（静默，无 cmd 窗口）
//
// 安全要点：规则限定 program + 具体端口 + 仅本地子网（remoteip=localsubnet）；
// profile=any 覆盖专用/公用/域网络（新 Wi-Fi 默认是公用），但仍不向子网外开放。
// 历史实现为整机开放所有入站 UDP，远超功能所需。
//
// 命名要点：netsh 使用自己的命令行解析器，含空格的 name 需要额外引号，
// 极易被截断成另一条规则（进而每次启动都"删不掉旧的、再加一条新的"）。
// 因此规则名保持 ASCII 且不含空格。
//
// 检查要点：增删规则需要管理员权限。历史实现每次启动都无条件"先删后加"，
// 普通权限下两次 netsh 都会失败——第一次管理员启动建好的规则明明有效，
// 第二次普通启动却报"添加失败"。因此先把检查前置：规则已完全匹配就
// 完全不动防火墙。检查用防火墙 COM API（`INetFwPolicy2`）：毫秒级、
// 不受系统语言影响、不启动额外进程；曾试过 PowerShell
// `Get-NetFirewallRule`，仅进程启动就要数秒，不可接受。

use crate::i18n;
use crate::logf;
use std::os::windows::process::CommandExt;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::NetworkManagement::WindowsFirewall::{
    NetFwPolicy2, INetFwPolicy2, INetFwRule, NET_FW_ACTION_ALLOW, NET_FW_PROFILE2_ALL,
    NET_FW_RULE_DIR_IN,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Ole::IEnumVARIANT;
use windows::Win32::System::Threading::CREATE_NO_WINDOW;
use windows::Win32::System::Variant::{VariantClear, VT_DISPATCH, VARIANT};
use windows::core::Interface;

const RULE_NAME: &str = "UDP2CAL-In-UDP";
/// 发现服务固定端口
const DISCOVERY_PORT: u16 = udp2cal_protocol::DISCOVER_PORT;
/// UDP 的 IANA 协议号（COM 层存数字）
const PROTOCOL_UDP: i32 = 17;

fn netsh(args: &[String]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("netsh")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
}

/// 检查现有规则是否已完全覆盖本次需求：
/// 同名规则存在，且 程序路径 / 两个端口 / UDP / 入站方向 / 允许动作 / 已启用 全部一致。
/// 返回 false 表示"未确认匹配"（不存在、不匹配或检查失败），调用方走增删路径。
fn matching_rule_exists(exe: &str, audio_port: u16) -> bool {
    // COM 可能已被本线程初始化（S_FALSE）或初始化为其它模型
    // （RPC_E_CHANGED_MODE），两种情况 COM 都可用；真正的失败会由
    // CoCreateInstance 暴露并走回退路径。成功初始化后不调用 CoUninitialize：
    // 本进程主线程刻意保持 COM 已初始化到退出（winit 随后 OleInitialize
    // 只会得到 S_FALSE），无需在此平衡。
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        logf!("[Firewall] COM 初始化失败（{hr}），回退为先删后加");
        return false;
    }

    let checked = try_matching_rule(exe, audio_port);
    match checked {
        Some(true) => {
            logf!("[Firewall] 现有规则已匹配（程序+端口），跳过 netsh");
            true
        }
        Some(false) => false,
        // 防火墙服务不可用、规则属性读取失败等：与"不存在"同等对待。
        // 普通权限下随后的增删会失败并提示，行为与历史实现一致。
        None => {
            logf!("[Firewall] 规则检查不可用或未命中，回退为先删后加");
            false
        }
    }
}

/// 实际的 COM 检查。任何一步失败都返回 None（视为未确认匹配）。
///
/// 枚举全部规则而非 `Item(name)` 直取：netsh 容忍同名重复规则，`Item` 只返回
/// 其中一个——若同名存在一条 block 规则（block 优先于 allow），直取可能返回
/// allow 规则而误判"匹配"，入站实际仍被拦。因此要求"恰好一条同名规则且完全
/// 匹配"；同名重复一律回退增删路径（`netsh delete` 会清掉全部同名规则，自愈）。
fn try_matching_rule(exe: &str, audio_port: u16) -> Option<bool> {
    unsafe {
        let policy: INetFwPolicy2 =
            CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER).ok()?;
        let rules = policy.Rules().ok()?;
        let ev: IEnumVARIANT = rules._NewEnum().ok()?.cast().ok()?;

        let mut same_name = 0usize;
        let mut matched = false;
        let mut complete = false;
        loop {
            let mut var = [VARIANT::default()];
            let mut fetched = 0u32;
            if ev.Next(&mut var, &mut fetched).is_err() {
                break;
            }
            if fetched == 0 {
                complete = true;
                break;
            }
            let rule: Option<INetFwRule> =
                if var[0].Anonymous.Anonymous.vt == VT_DISPATCH {
                    (*var[0].Anonymous.Anonymous.Anonymous.pdispVal)
                        .as_ref()
                        .and_then(|disp| disp.cast::<INetFwRule>().ok())
                } else {
                    None
                };
            let _ = VariantClear(&mut var[0]);
            let Some(rule) = rule else {
                continue;
            };
            let Ok(name) = rule.Name() else {
                continue;
            };
            // 防火墙规则名按大小写不敏感匹配（netsh 用任意大小写都能查到同一条），
            // 同名判定必须同样不敏感，否则大小写变体的重复规则会绕过防重复保护
            if !name.to_string().eq_ignore_ascii_case(RULE_NAME) {
                continue;
            }
            same_name += 1;
            matched |= rule_matches(&rule, exe, audio_port).unwrap_or(false);
        }
        rule_confirmed(same_name, matched, complete)
    }
}

/// "确认命中"的判定：恰好一条同名规则、它完全匹配、且枚举完整跑完。
/// 单独提成纯函数并配表测试：这条判定保护的是"同名重复/枚举中断不得误判
/// 匹配"，回归（如改成 `>=1` 或删掉 complete 检查）会让 block 规则永远
/// 得不到自愈，所以 CI 必须能抓住它。
fn rule_confirmed(same_name: usize, matched: bool, complete: bool) -> Option<bool> {
    if !complete {
        // 枚举中断：无法确认没有更多同名规则，不能断言"匹配"。
        return None;
    }
    Some(same_name == 1 && matched)
}

/// 单条规则是否完全覆盖本次需求。
/// 审查结论：除程序/端口/协议/方向/动作/启用外，还必须核对 Profiles 与
/// 地址范围——历史版本曾用 `profile=private`、无 `remoteip` 写过同名规则，
/// 升级后若只看前者会把不适用的旧规则误判为匹配（公用网络下静默无声音）。
fn rule_matches(rule: &INetFwRule, exe: &str, audio_port: u16) -> Option<bool> {
    unsafe {
        if !rule.Enabled().ok()?.as_bool() {
            return Some(false);
        }
        if rule.Action().ok()? != NET_FW_ACTION_ALLOW {
            return Some(false);
        }
        if rule.Direction().ok()? != NET_FW_RULE_DIR_IN {
            return Some(false);
        }
        if rule.Protocol().ok()? != PROTOCOL_UDP {
            return Some(false);
        }
        // profile=any → 所有配置文件（实机规则为 2147483647）
        if rule.Profiles().ok()? != NET_FW_PROFILE2_ALL.0 {
            return Some(false);
        }
        // 程序路径：Windows 路径大小写不敏感
        let app = rule.ApplicationName().ok()?.to_string();
        if !program_matches(&app, exe) {
            return Some(false);
        }
        // 端口集合：要求两个端口都精确存在且无多余（范围/乱序都算不匹配）
        let ports = rule.LocalPorts().ok()?.to_string();
        if !ports_match(&ports, [DISCOVERY_PORT, audio_port]) {
            return Some(false);
        }
        // remoteip=localsubnet（COM 存 "LocalSubnet"）
        if !rule
            .RemoteAddresses()
            .ok()?
            .to_string()
            .eq_ignore_ascii_case("LocalSubnet")
        {
            return Some(false);
        }
        // 未限定本地地址 → COM 存 "*"
        if !rule.LocalAddresses().ok()?.to_string().eq_ignore_ascii_case("*") {
            return Some(false);
        }
        Some(true)
    }
}

fn program_matches(stored: &str, exe: &str) -> bool {
    stored.eq_ignore_ascii_case(exe)
}

fn ports_match(stored: &str, want: [u16; 2]) -> bool {
    let Ok(mut got) = stored
        .split(',')
        .map(|s| s.trim().parse::<u16>())
        .collect::<Result<Vec<_>, _>>()
    else {
        return false;
    };
    got.sort_unstable();
    let mut want = want.to_vec();
    want.sort_unstable();
    got == want
}

/// 添加/更新放行规则。
/// `audio_port` 为实际监听端口；`program` 为当前可执行文件路径。
pub fn add_firewall_rule(audio_port: u16) -> Result<(), String> {
    let exe =
        std::env::current_exe().map_err(|e| i18n::tf("error.exe_path", &[("err", &e)]))?;
    let program = format!("program=\"{}\"", exe.display());

    // 规则已完全匹配时不动防火墙：普通权限下 netsh 增删必然失败，
    // 但第一次管理员启动留下的规则仍然有效，报错只会误导用户。
    if matching_rule_exists(&exe.to_string_lossy(), audio_port) {
        return Ok(());
    }
    let ports = format!("localport={DISCOVERY_PORT},{audio_port}");

    // 先删除旧规则（忽略失败，可能本来就不存在）
    let _ = netsh(&[
        "advfirewall".into(),
        "firewall".into(),
        "delete".into(),
        "rule".into(),
        format!("name={RULE_NAME}"),
    ]);

    let result = netsh(&[
        "advfirewall".into(),
        "firewall".into(),
        "add".into(),
        "rule".into(),
        format!("name={RULE_NAME}"),
        "dir=in".into(),
        "action=allow".into(),
        "protocol=UDP".into(),
        ports,
        program,
        "enable=yes".into(),
        "profile=any".into(),
        "remoteip=localsubnet".into(),
    ])
    .map_err(|e| i18n::tf("error.firewall_cmd", &[("err", &e)]))?;

    if result.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&result.stderr);
        let detail = detail.trim();
        Err(if detail.is_empty() {
            i18n::t("error.firewall_rule")
        } else {
            i18n::tf("error.firewall_rule_detail", &[("detail", &detail)])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端口集合比较：正序/乱序命中；多余端口、范围写法、非数字、空串不命中。
    #[test]
    fn ports_match_exact_set() {
        assert!(ports_match("44043,44044", [44043, 44044]));
        assert!(ports_match("44044,44043", [44043, 44044]));
        assert!(ports_match(" 44043 , 44044 ", [44043, 44044]));
        assert!(!ports_match("44043", [44043, 44044]));
        assert!(!ports_match("44043,44044,44045", [44043, 44044]));
        assert!(!ports_match("44043-44044", [44043, 44044]));
        assert!(!ports_match("", [44043, 44044]));
        assert!(!ports_match("x,y", [44043, 44044]));
    }

    /// 程序路径比较：仅大小写差异命中；路径不同不命中。
    #[test]
    fn program_matches_case_insensitive() {
        assert!(program_matches(
            r"C:\Users\21248\Documents\GitHub\UDP2CAL\udp2cal.exe",
            r"c:\users\21248\documents\github\udp2cal\UDP2CAL.exe"
        ));
        assert!(!program_matches(
            r"C:\Program Files\UDP2CAL\udp2cal.exe",
            r"C:\Users\21248\Documents\GitHub\UDP2CAL\udp2cal.exe"
        ));
    }

    /// "确认命中"判定：恰好一条同名规则且完全匹配且枚举完整。
    /// 重复规则（2 条）与枚举中断（complete=false）都不得误判命中。
    #[test]
    fn rule_confirmed_requires_unique_match_and_complete_enum() {
        assert_eq!(rule_confirmed(1, true, true), Some(true));
        assert_eq!(rule_confirmed(2, true, true), Some(false)); // 同名重复
        assert_eq!(rule_confirmed(0, true, true), Some(false)); // 规则不存在
        assert_eq!(rule_confirmed(1, false, true), Some(false)); // 属性不符
        assert_eq!(rule_confirmed(1, true, false), None); // 枚举中断
        assert_eq!(rule_confirmed(2, true, false), None);
    }

    /// 机器相关测试：需要本机存在一条 `UDP2CAL-In-UDP` 规则。
    /// 通过环境变量指定该规则记录的程序路径，未设置时跳过（CI 无防火墙前提）。
    #[test]
    fn com_check_hits_stored_rule() {
        let Ok(exe) = std::env::var("UDP2CAL_TEST_RULE_EXE") else {
            return;
        };
        // 现有规则的端口为 发现端口+44044，程序路径一致 → 必须命中
        assert!(matching_rule_exists(&exe, 44044));
        // 程序路径不一致（如调试 exe）→ 不得命中，必须走增删路径
        assert!(!matching_rule_exists(r"C:\definitely-not-this-program.exe", 44044));
    }
}

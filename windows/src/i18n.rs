//! 编译期内嵌的多语言文案。
//!
//! `build.rs` 扫描 `lang/*.lang` 生成 `EMBEDDED_LANGS`，这里只做解析、查找与
//! 当前语言管理。新增语言只需放入一个 `.lang` 文件并重新编译。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_langs.rs"));
}

struct Lang {
    code: String,
    name: String,
    map: HashMap<String, String>,
}

struct State {
    langs: Vec<Lang>,
    active: usize,
    english: usize,
}

static STATE: OnceLock<RwLock<State>> = OnceLock::new();

fn state() -> &'static RwLock<State> {
    STATE.get_or_init(|| RwLock::new(State::load()))
}

fn read() -> std::sync::RwLockReadGuard<'static, State> {
    state().read().unwrap_or_else(|e| e.into_inner())
}

fn write() -> std::sync::RwLockWriteGuard<'static, State> {
    state().write().unwrap_or_else(|e| e.into_inner())
}

impl State {
    fn load() -> Self {
        let mut langs: Vec<Lang> = embedded::EMBEDDED_LANGS
            .iter()
            .map(|(code, text)| {
                let map = parse(text);
                let name = map
                    .get("language.name")
                    .cloned()
                    .unwrap_or_else(|| (*code).to_string());
                Lang {
                    code: (*code).to_string(),
                    name,
                    map,
                }
            })
            .collect();
        langs.sort_by(|a, b| a.code.cmp(&b.code));
        if langs.is_empty() {
            langs.push(Lang {
                code: "en-US".to_string(),
                name: "English".to_string(),
                map: HashMap::new(),
            });
        }
        let english = langs
            .iter()
            .position(|lang| lang.code == "en-US")
            .unwrap_or(0);
        let mut state = State {
            langs,
            active: english,
            english,
        };
        state.active = state.resolve(&system_tag());
        state
    }

    /// 精确匹配或主语言前缀匹配（zh-CN → zh）。
    fn find(&self, wanted: &str) -> Option<usize> {
        if wanted.is_empty() {
            return None;
        }
        if let Some(index) = self
            .langs
            .iter()
            .position(|lang| lang.code.eq_ignore_ascii_case(wanted))
        {
            return Some(index);
        }
        let primary = wanted.split(['-', '_']).next().unwrap_or(wanted);
        self.langs.iter().position(|lang| {
            lang.code
                .split(['-', '_'])
                .next()
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(primary))
        })
    }

    /// 匹配失败时回退英文。
    fn resolve(&self, wanted: &str) -> usize {
        self.find(wanted).unwrap_or(self.english)
    }
}

/// 解析 `key = value`。值内部允许出现 `=`；`#` / `;` 开头的行是注释。
fn parse(text: &str) -> HashMap<String, String> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty() && !line.starts_with('#') && !line.starts_with(';')
        })
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

fn system_tag() -> String {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::GetUserDefaultLocaleName;
        let mut buffer = [0u16; 85]; // LOCALE_NAME_MAX_LENGTH
        // SAFETY: `buffer` 是可写切片，函数最多写入 `buffer.len()` 个 UTF-16 单元。
        let len = unsafe { GetUserDefaultLocaleName(&mut buffer) };
        if len > 1 {
            return String::from_utf16_lossy(&buffer[..len as usize - 1]);
        }
    }
    "en-US".to_string()
}

/// 初始化（幂等）。启动时调用一次即可，其余接口也会按需惰性初始化。
pub fn init() {
    let _ = state();
}

/// 可选语言：(代码, 语言自称)，按代码排序。
pub fn available() -> Vec<(String, String)> {
    read()
        .langs
        .iter()
        .map(|lang| (lang.code.clone(), lang.name.clone()))
        .collect()
}

/// 当前生效的语言代码（用于菜单勾选态）。
pub fn active_code() -> String {
    let state = read();
    state.langs[state.active].code.clone()
}

/// 切换语言。空串表示跟随系统；未知代码回退到系统语言，再回退到英文。
pub fn set_active(code: &str) {
    let mut state = write();
    let wanted = if code.is_empty() {
        system_tag()
    } else {
        code.to_string()
    };
    state.active = state
        .find(&wanted)
        .or_else(|| state.find(&system_tag()))
        .unwrap_or(state.english);
}

/// 取当前语言文案：当前语言 → 英文 → key 本身。
pub fn t(key: &str) -> String {
    let state = read();
    if let Some(value) = state.langs[state.active].map.get(key) {
        return value.clone();
    }
    if let Some(value) = state.langs[state.english].map.get(key) {
        return value.clone();
    }
    key.to_string()
}

/// 取文案并替换 `{name}` 占位符。
pub fn tf(key: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    fill(&t(key), args)
}

fn fill(template: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    debug_assert!(
        !missing_arg(template, args),
        "tf 参数缺少模板占位符: {template}"
    );
    let mut text = template.to_string();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), &value.to_string());
    }
    text
}

/// 只扫描模板本身，避免把替换值里的 `{GUID}` 之类误判为未替换占位符。
fn missing_arg(template: &str, args: &[(&str, &dyn std::fmt::Display)]) -> bool {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let name = &after[..end];
        if !name.is_empty() && !args.iter().any(|(arg, _)| *arg == name) {
            return true;
        }
        rest = &after[end + 1..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ignores_comments_blank_lines_and_bom() {
        let map = parse("\u{feff}# comment\n\n a = 1 \n; skip\nb = x=y\n");
        assert_eq!(map.get("a").map(String::as_str), Some("1"));
        assert_eq!(map.get("b").map(String::as_str), Some("x=y"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn fill_replaces_named_placeholders() {
        let text = fill("{a}-{b}-{a}", &[("a", &1), ("b", &"x")]);
        assert_eq!(text, "1-x-1");
    }

    #[test]
    fn fill_detects_missing_argument() {
        assert!(missing_arg("{a}", &[("b", &1)]));
        assert!(!missing_arg("{a}", &[("a", &1)]));
    }

    fn placeholders(text: &str) -> std::collections::BTreeSet<String> {
        let mut found = std::collections::BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('}') else { break };
            let name = &after[..end];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                found.insert(name.to_string());
            }
            rest = &after[end + 1..];
        }
        found
    }

    #[test]
    fn embedded_languages_share_placeholders() {
        let english = parse(
            embedded::EMBEDDED_LANGS
                .iter()
                .find(|(code, _)| *code == "en-US")
                .expect("缺少 en-US.lang")
                .1,
        );
        for (code, text) in embedded::EMBEDDED_LANGS {
            if *code == "en-US" {
                continue;
            }
            let map = parse(text);
            for (key, en_value) in &english {
                let value = map.get(key).expect("key 集合测试已覆盖缺失");
                assert_eq!(
                    placeholders(en_value),
                    placeholders(value),
                    "{code} 的占位符不一致: {key}"
                );
            }
        }
    }

    #[test]
    fn embedded_languages_share_the_same_keys() {
        let english = parse(
            embedded::EMBEDDED_LANGS
                .iter()
                .find(|(code, _)| *code == "en-US")
                .expect("缺少 en-US.lang")
                .1,
        );
        assert!(english.contains_key("language.name"));
        for (code, text) in embedded::EMBEDDED_LANGS {
            if *code == "en-US" {
                continue;
            }
            let map = parse(text);
            for key in english.keys() {
                assert!(map.contains_key(key), "{code} 缺少 key: {key}");
            }
            for key in map.keys() {
                assert!(english.contains_key(key), "{code} 多出 key: {key}");
            }
        }
    }
}

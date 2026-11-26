//! 流量接管：Windows 系统代理 + 本地 PAC 文件。
//!
//! 相比 TUN 不需要管理员权限和虚拟网卡：启动时把系统代理指向一个本地 PAC，
//! PAC 里只把「勾选站点」的域名返回 PROXY 127.0.0.1:7890，其余一律 DIRECT。
//! 因此未勾选网站完全不经过本软件。退出时恢复用户原来的代理设置。
//!
//! 只写当前用户（HKCU）的 Internet Settings，不碰系统级设置。

use crate::core::split_domains;
use crate::{AppState, PROXY_PORT};
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 用户原来的代理设置，退出时据此恢复。
#[derive(Default)]
struct SavedProxy {
    auto_config_url: Option<String>,
    proxy_enable: Option<u32>,
    proxy_server: Option<String>,
}

/// 当前生效的接管状态：保存原设置以便恢复。None 表示未接管。
static SAVED: Mutex<Option<SavedProxy>> = Mutex::new(None);

fn pac_path() -> Result<PathBuf> {
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("APPDATA"))
        .unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(base).join("SteamPilot");
    fs::create_dir_all(&dir).context("create SteamPilot dir")?;
    Ok(dir.join("proxy.pac"))
}

/// 根据启用的站点生成 PAC 脚本。命中白名单走本地代理，否则直连。
pub fn build_pac(state: &AppState) -> String {
    let mut exact: Vec<String> = Vec::new();
    let mut suffix: Vec<String> = Vec::new();
    for site in &state.sites {
        if !site.enabled {
            continue;
        }
        for d in split_domains(&site.domains) {
            if let Some(base) = d.strip_prefix("*.") {
                suffix.push(base.to_ascii_lowercase());
            } else {
                exact.push(d.to_ascii_lowercase());
            }
        }
    }
    exact.sort();
    exact.dedup();
    suffix.sort();
    suffix.dedup();

    let js_string = |v: &Vec<String>| {
        v.iter()
            .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",")
    };

    format!(
        "// SteamPilot PAC — 仅勾选的网站走加速，其余直连\n\
         var EXACT = [{exact}];\n\
         var SUFFIX = [{suffix}];\n\
         function FindProxyForURL(url, host) {{\n\
         \x20   host = host.toLowerCase();\n\
         \x20   for (var i = 0; i < EXACT.length; i++) if (host === EXACT[i]) return \"PROXY 127.0.0.1:{port}\";\n\
         \x20   for (var j = 0; j < SUFFIX.length; j++) if (host === SUFFIX[j] || shExpMatch(host, \"*.\" + SUFFIX[j])) return \"PROXY 127.0.0.1:{port}\";\n\
         \x20   return \"DIRECT\";\n\
         }}\n",
        exact = js_string(&exact),
        suffix = js_string(&suffix),
        port = PROXY_PORT,
    )
}

/// 诊断：打印当前 PAC 文件是否存在、以及系统代理注册表值。
pub fn show_status() {
    let pac = pac_path().ok();
    match &pac {
        Some(p) => {
            let exists = p.exists();
            let len = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            println!("[PAC] {} 存在={} 大小={}B", p.display(), exists, len);
        }
        None => println!("[PAC] 无法确定路径"),
    }
    #[cfg(windows)]
    {
        match sys::read_current() {
            Ok(v) => println!(
                "[系统代理] AutoConfigURL={:?} ProxyEnable={:?} ProxyServer={:?}",
                v.auto_config_url, v.proxy_enable, v.proxy_server
            ),
            Err(e) => println!("[系统代理] 读取失败：{}", e),
        }
    }
}

/// 判断一个 AutoConfigURL 是否是我们自己写的 PAC（程序被强杀/崩溃后的残留）。
#[cfg(windows)]
fn is_our_pac(url: &str) -> bool {
    let u = url.to_ascii_lowercase();
    u.starts_with("file:///") && u.contains("steampilot") && u.ends_with("proxy.pac")
}

/// 应用系统代理（写 PAC 路径到 AutoConfigURL）。返回是否成功。
pub fn apply(state: &AppState) -> Result<()> {
    let path = pac_path()?;
    fs::write(&path, build_pac(state)).context("write PAC file")?;
    let url = format!("file:///{}", path.to_string_lossy().replace('\\', "/"));

    #[cfg(windows)]
    {
        // 幂等：已在接管中（SAVED 有值）时不重复读取当前配置，否则二次启动会把
        // 自己写的 PAC 当成「用户原配置」存下，退出时无法恢复 → 勾选网站断网。
        let mut guard = SAVED.lock().unwrap();
        if guard.is_none() {
            let mut saved = sys::read_current()?;
            // 崩溃/强杀后 PAC 会残留在注册表里、而内存中的原配置已丢失。
            // 不能把这条残留 PAC 当成用户原值，否则下次退出会「恢复」成它，
            // 导致系统代理永久指向一个已不存在的本地代理。
            if saved
                .auto_config_url
                .as_deref()
                .map(is_our_pac)
                .unwrap_or(false)
            {
                saved.auto_config_url = None;
            }
            *guard = Some(saved);
        }
        sys::set_auto_config_url(Some(&url))?;
        sys::refresh();
    }
    #[cfg(not(windows))]
    let _ = url;

    Ok(())
}

/// 撤销接管：恢复用户原来的代理设置。
pub fn restore() -> Result<()> {
    #[cfg(windows)]
    {
        if let Some(saved) = SAVED.lock().unwrap().take() {
            sys::set_auto_config_url(saved.auto_config_url.as_deref())?;
            if let Some(v) = saved.proxy_enable {
                sys::set_proxy_enable(v)?;
            }
            if let Some(v) = saved.proxy_server.as_deref() {
                sys::set_proxy_server(Some(v))?;
            }
            sys::refresh();
        }
    }
    Ok(())
}

#[cfg(windows)]
mod sys {
    use super::*;
    use windows_sys::Win32::Networking::WinInet::{
        InternetSetOptionW, INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_WRITE, REG_DWORD, REG_SZ,
    };

    const SETTINGS: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";

    fn open() -> Result<HKEY> {
        let wide: Vec<u16> = SETTINGS.encode_utf16().chain(std::iter::once(0)).collect();
        let mut key: HKEY = 0;
        let rc = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide.as_ptr(),
                0,
                KEY_READ | KEY_WRITE,
                &mut key,
            )
        };
        if rc != 0 {
            anyhow::bail!("打开 Internet Settings 失败: {}", rc);
        }
        Ok(key)
    }

    unsafe fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn get_str(name: &str) -> Option<String> {
        let key = open().ok()?;
        let n = unsafe { wide(name) };
        let mut ty = 0u32;
        let mut len = 0u32;
        unsafe {
            RegQueryValueExW(
                key,
                n.as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if ty != REG_SZ || len == 0 {
            unsafe { RegCloseKey(key) };
            return None;
        }
        let mut buf = vec![0u16; len as usize / 2 + 1];
        let rc = unsafe {
            RegQueryValueExW(
                key,
                n.as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                buf.as_mut_ptr() as *mut u8,
                &mut len,
            )
        };
        unsafe { RegCloseKey(key) };
        if rc != 0 {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    fn get_dword(name: &str) -> Option<u32> {
        let key = open().ok()?;
        let n = unsafe { wide(name) };
        let mut ty = 0u32;
        let mut len = 4u32;
        let mut v = 0u32;
        let rc = unsafe {
            RegQueryValueExW(
                key,
                n.as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                &mut v as *mut u32 as *mut u8,
                &mut len,
            )
        };
        unsafe { RegCloseKey(key) };
        if rc == 0 && ty == REG_DWORD {
            Some(v)
        } else {
            None
        }
    }

    fn set_str(name: &str, value: Option<&str>) -> Result<()> {
        let key = open()?;
        let n = unsafe { wide(name) };
        let rc = if let Some(v) = value {
            let data: Vec<u16> = v.encode_utf16().chain(std::iter::once(0)).collect();
            let len = (data.len() * 2) as u32;
            unsafe { RegSetValueExW(key, n.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, len) }
        } else {
            // 删除值：设空串等价于清空 AutoConfigURL；这里写空串更稳妥，避免删键权限问题
            let data: Vec<u16> = std::iter::once(0).collect();
            unsafe { RegSetValueExW(key, n.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, 2) }
        };
        unsafe { RegCloseKey(key) };
        if rc != 0 {
            anyhow::bail!("写 {} 失败: {}", name, rc);
        }
        Ok(())
    }

    pub(super) fn set_auto_config_url(url: Option<&str>) -> Result<()> {
        set_str("AutoConfigURL", url)
    }

    pub(super) fn set_proxy_server(url: Option<&str>) -> Result<()> {
        set_str("ProxyServer", url)
    }

    pub(super) fn set_proxy_enable(v: u32) -> Result<()> {
        let key = open()?;
        let n = unsafe { wide("ProxyEnable") };
        let rc = unsafe {
            RegSetValueExW(
                key,
                n.as_ptr(),
                0,
                REG_DWORD,
                &v as *const u32 as *const u8,
                4,
            )
        };
        unsafe { RegCloseKey(key) };
        if rc != 0 {
            anyhow::bail!("写 ProxyEnable 失败: {}", rc);
        }
        Ok(())
    }

    pub(super) fn read_current() -> Result<SavedProxy> {
        Ok(SavedProxy {
            auto_config_url: get_str("AutoConfigURL"),
            proxy_enable: get_dword("ProxyEnable"),
            proxy_server: get_str("ProxyServer"),
        })
    }

    /// 通知系统/浏览器代理设置已变更，立即生效。
    pub(super) fn refresh() {
        unsafe {
            InternetSetOptionW(
                std::ptr::null(),
                INTERNET_OPTION_SETTINGS_CHANGED,
                std::ptr::null(),
                0,
            );
            InternetSetOptionW(
                std::ptr::null(),
                INTERNET_OPTION_REFRESH,
                std::ptr::null(),
                0,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(sites: Vec<crate::Site>) -> AppState {
        let mut s = AppState::default();
        s.sites = sites;
        s
    }

    #[test]
    fn pac_routes_only_enabled_sites() {
        let mut a = AppState::default().sites[0].clone(); // Steam
        a.enabled = true;
        let mut b = AppState::default().sites[3].clone(); // GitHub
        b.enabled = false;
        let pac = build_pac(&state_with(vec![a, b]));
        assert!(pac.contains("steamcommunity.com"), "PAC 应含启用站点的域名");
        assert!(
            !pac.contains("raw.githubusercontent.com"),
            "PAC 不应含停用站点域名"
        );
        assert!(pac.contains("return \"DIRECT\""), "PAC 必须有直连兜底");
        assert!(
            pac.contains(&format!("127.0.0.1:{}", PROXY_PORT)),
            "PAC 应指向本地代理"
        );
    }

    #[test]
    fn pac_wildcard_uses_suffix() {
        let mut a = AppState::default().sites[0].clone();
        a.domains = "*.steamstatic.com".into();
        a.enabled = true;
        let pac = build_pac(&state_with(vec![a]));
        assert!(pac.contains("steamstatic.com"));
        assert!(
            pac.contains("shExpMatch"),
            "通配域名应用 shExpMatch 匹配子域"
        );
    }

    #[cfg(windows)]
    #[test]
    fn identifies_our_own_stale_pac() {
        // 崩溃残留：自身写的 PAC 必须被识别为「不是用户原配置」，否则退出时无法恢复。
        assert!(is_our_pac(
            "file:///C:/Users/x/AppData/Local/SteamPilot/proxy.pac"
        ));
        assert!(is_our_pac("FILE:///C:/steampilot/PROXY.PAC"));
        // 用户自己配的其它 PAC 不能误识别，否则会破坏用户原有代理。
        assert!(!is_our_pac("file:///C:/proxy/user.pac"));
        assert!(!is_our_pac("http://example.com/proxy.pac"));
        assert!(!is_our_pac(""));
    }
}

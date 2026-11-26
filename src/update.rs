//! 自更新：只做「检查 GitHub Releases + 提示打开下载页」，不自动下载/替换 exe。
//! 自动替换需要签名、杀软白名单、占用中进程处理，远超「简洁小工具」的范围。
//! 复用项目已有的 rustls + tokio 栈，不引入 tauri-plugin-updater / reqwest。

use rustls::pki_types::ServerName;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

const HOST: &str = "api.github.com";
const PATH: &str = "/repos/Linyumoe/SteamPilot/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/Linyumoe/SteamPilot/releases/latest";
const REPO: &str = "Linyumoe/SteamPilot";
/// 第三方反代前缀：目标用户常无法直连 GitHub，镜像入口必须与官方入口并列给出。
/// 只用于「打开下载页」，更新检查本身仍走官方 API；sha256 见 Release notes，供用户自行校验。
const MIRROR_PREFIX: &str = "https://gh-proxy.org/";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// 当前程序版本（来自 Cargo.toml）
    pub current: String,
    /// 最新版本号（去掉 v 前缀），检查失败时为空
    pub latest: String,
    /// 是否有新版本
    pub available: bool,
    /// 发布页 URL，供界面打开
    pub url: String,
    /// Windows 安装包官方直链（无则空串）
    pub download_url: String,
    /// 同一安装包的镜像直链（无则空串）
    pub mirror_url: String,
    /// 更新说明（release body 的前若干字）
    pub notes: String,
    /// 错误提示（网络失败/无发布），空串表示正常
    pub error: String,
}

impl Default for UpdateInfo {
    fn default() -> Self {
        Self {
            current: CURRENT_VERSION.into(),
            latest: String::new(),
            available: false,
            url: RELEASES_PAGE.into(),
            download_url: String::new(),
            mirror_url: String::new(),
            notes: String::new(),
            error: String::new(),
        }
    }
}

fn tls_connector() -> TlsConnector {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    TlsConnector::from(Arc::new(cfg))
}

/// 把 tag_name（如 v0.2.0）解析成可比较的数字三段，失败返回 None。
fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let t = tag.trim().trim_start_matches(|c| c == 'v' || c == 'V');
    let mut p = t.split('.');
    let major = p.next()?.parse().ok()?;
    let minor = p.next().unwrap_or("0").parse().ok()?;
    let patch = p.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// latest > current 才提示更新。
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// 极简 HTTPS GET：连域名走系统解析，标准验签，够读一个 releases JSON。
async fn https_get(host: &str, path: &str) -> anyhow::Result<String> {
    let connector = tls_connector();
    let addr = format!("{}:443", host);
    let tcp = timeout(Duration::from_secs(8), TcpStream::connect(&addr)).await??;
    let name = ServerName::try_from(host.to_string())?;
    let mut tls = timeout(Duration::from_secs(8), connector.connect(name, tcp)).await??;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: SteamPilot/{}\r\nAccept: application/vnd.github+json\r\nConnection: close\r\n\r\n",
        path, host, CURRENT_VERSION
    );
    timeout(Duration::from_secs(8), tls.write_all(req.as_bytes())).await??;

    let mut buf = Vec::new();
    timeout(Duration::from_secs(8), tls.read_to_end(&mut buf)).await??;
    let raw = String::from_utf8_lossy(&buf);
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");

    // GitHub 可能回 chunked，去掉块长度行拼回正文。
    if raw.lines().any(|l| {
        l.to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
    }) {
        Ok(dechunk(body))
    } else {
        Ok(body.to_string())
    }
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some((len_line, after)) = rest.split_once("\r\n") else {
            break;
        };
        let len = usize::from_str_radix(len_line.trim().split(';').next().unwrap_or("0"), 16)
            .unwrap_or(0);
        if len == 0 || after.len() < len {
            break;
        }
        out.push_str(&after[..len]);
        rest = &after[len..];
        rest = rest.strip_prefix("\r\n").unwrap_or(rest);
    }
    out
}

/// 检查是否有新版本。任何失败都返回带 error 的正常结构，不抛错给界面。
pub async fn check() -> UpdateInfo {
    let mut info = UpdateInfo::default();
    let body = match https_get(HOST, PATH).await {
        Ok(b) => b,
        Err(e) => {
            info.error = format!("检查更新失败：{}", e);
            return info;
        }
    };
    let json: serde_json::Value = match serde_json::from_str(body.trim()) {
        Ok(v) => v,
        Err(_) => {
            info.error = "检查更新失败：发布信息格式异常".into();
            return info;
        }
    };
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if tag.is_empty() {
        info.error = "仓库还没有发布版本".into();
        return info;
    }
    info.latest = tag.trim_start_matches(|c| c == 'v' || c == 'V').to_string();
    info.available = is_newer(&info.latest, CURRENT_VERSION);
    info.url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or(RELEASES_PAGE)
        .to_string();
    // 取 win-x64 安装包直链；镜像只是同一 URL 加反代前缀，便于直连困难的用户下载。
    if let Some(asset) = json.get("assets").and_then(|v| v.as_array()).and_then(|a| {
        a.iter().find(|x| {
            x.get("name")
                .and_then(|n| n.as_str())
                .map_or(false, |n| n.ends_with("-win-x64.zip"))
        })
    }) {
        if let Some(url) = asset.get("browser_download_url").and_then(|v| v.as_str()) {
            info.download_url = url.to_string();
            info.mirror_url = format!("{}{}", MIRROR_PREFIX, url);
        }
    }
    info.notes = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(180)
        .collect();
    info
}

/// 用系统默认浏览器打开发布页（explorer 跟随 URL 协议，不弹控制台窗口）。
pub fn open_releases() -> anyhow::Result<()> {
    open_url(RELEASES_PAGE)
}

/// 只允许本仓库安装包直链或其 gh-proxy 镜像，避免命令被当成任意 URL 打开器。
pub fn validate_download_url(url: &str) -> bool {
    let direct = format!("https://github.com/{}/releases/download/", REPO);
    if url.starts_with(&direct) {
        return true;
    }
    url.strip_prefix(MIRROR_PREFIX)
        .map_or(false, |rest| rest.starts_with(&direct))
}

pub fn open_url(url: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer").arg(url).spawn()?;
    }
    #[cfg(not(windows))]
    {
        let _ = url;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn chunked_decode() {
        let body = "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(dechunk(body), "hello world");
    }

    #[test]
    fn download_url_allowlist() {
        let direct = "https://github.com/Linyumoe/SteamPilot/releases/download/v0.1.1/SteamPilot-v0.1.1-win-x64.zip";
        assert!(validate_download_url(direct));
        assert!(validate_download_url(&format!(
            "{}{}",
            MIRROR_PREFIX, direct
        )));
        assert!(!validate_download_url("https://evil.com/x.zip"));
        assert!(!validate_download_url(&format!(
            "{}https://evil.com/x.zip",
            MIRROR_PREFIX
        )));
        assert!(!validate_download_url(
            "file:///C:/Windows/system32/calc.exe"
        ));
    }
}

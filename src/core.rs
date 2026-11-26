//! 域名白名单与请求上下文：连接下游 SNI、上游 SNI、HTTP Host 三者分离。
//!
//! 分流决策：
//!   - 命中的站点（且启用）→ Accelerate，走优选节点
//!   - 页面修复子功能开启 → 打标记，代理层在响应里注入脚本
//!   - 未命中 → Direct，本地直连，不经过任何伪装

use crate::Site;

#[derive(Debug, Clone)]
pub enum Decision {
    /// 走优选节点：provider 决定用 Akamai / Fastly 池，sni 是该池实测可用的上游别名。
    Accelerate {
        provider: String,
        upstream_sni: String,
        page_fix: bool,
    },
    /// 不加速，直连真实域名。
    Direct,
}

/// 域名匹配：精确、前缀通配（*.example.com）、中点/逗号/空格分隔的域名列表。
pub fn domain_matches(pattern: &str, host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    if pattern.is_empty() {
        return false;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host == suffix || host.ends_with(&format!(".{}", suffix))
    } else {
        host == pattern
    }
}

pub fn split_domains(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == '·' || c == ',' || c == ';' || c == ' ' || c == '\n')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 每个 provider 实测可用的上游别名 SNI（严格按该别名验签）。
/// 与节点池里记录的 SNI 保持一致；这里作为兜底，节点自带 SNI 优先。
fn default_sni(provider: &str) -> &'static str {
    match provider {
        "akamai" => "steamcommunity-a.akamaihd.net",
        "fastly" => "desktop.githubusercontent.com",
        // 真域名 SNI（Epic / GitHub A 组）：域名本身不被阻断，证书天然匹配，
        // 不需要别名伪装；节点池里记录的 sni 也是真域名。
        "github-ms" | "epic-fastly" | "epic-akamai" => "",
        _ => "",
    }
}

/// GitHub 按域名分片：主站/API/codeload/gist/ssh 走微软自有 IP（A 组，
/// SNI=真域名、标准验签）；raw/assets/collections 等走 Fastly（B 组，别名 SNI）。
/// 返回 Some(provider) 表示需要覆盖站点级别的 provider。
fn github_shard(host: &str) -> Option<&'static str> {
    const MS_GROUP: &[&str] = &[
        "github.com",
        "api.github.com",
        "codeload.github.com",
        "gist.github.com",
        "ssh.github.com",
        "www.github.com",
    ];
    const FASTLY_GROUP: &[&str] = &[
        "raw.githubusercontent.com",
        "avatars.githubusercontent.com",
        "user-images.githubusercontent.com",
        "camo.githubusercontent.com",
        "objects.githubusercontent.com",
        "github.githubassets.com",
        "gist.githubusercontent.com",
    ];
    if MS_GROUP.iter().any(|d| domain_matches(d, host)) {
        Some("github-ms")
    } else if FASTLY_GROUP.iter().any(|d| domain_matches(d, host)) || host.ends_with(".github.io") {
        Some("fastly")
    } else {
        None
    }
}

/// Epic 按域名分片：只有两个下载域名需要走节点池（真域名 SNI，域名未被阻断，
/// 优选节点只是选低延迟线路）；store/launcher 等网页域名直连即可。
/// 返回 None 表示该域名应直连。
fn epic_shard(host: &str) -> Option<&'static str> {
    if domain_matches("fastly-download.epicgames.com", host) {
        Some("epic-fastly")
    } else if domain_matches("epicgames-download1.akamaized.net", host) {
        Some("epic-akamai")
    } else {
        None
    }
}

/// 根据 Host 与启用的站点列表做分流决策。
pub fn decide(host: &str, sites: &[Site]) -> Decision {
    let host_lower = host.trim().trim_end_matches('.').to_ascii_lowercase();
    for site in sites {
        if !site.enabled {
            continue;
        }
        if split_domains(&site.domains)
            .iter()
            .any(|d| domain_matches(d, &host_lower))
        {
            // Epic 的网页域名（store/launcher）本身未被阻断，直连即可；
            // 只有下载域名走对应节点池。避免误用 GitHub 别名导致路由到 GitHub Pages。
            if site.id == "epic" && epic_shard(&host_lower).is_none() {
                continue;
            }
            // 站点内部按域名再分片：GitHub 分 A/B 组，Epic 分 Fastly/Akamai 下载池。
            let provider = if site.id == "github" {
                github_shard(&host_lower).unwrap_or("github-ms").to_string()
            } else if site.id == "epic" {
                epic_shard(&host_lower).unwrap_or("epic-fastly").to_string()
            } else if site.provider.is_empty() {
                "akamai".to_string()
            } else {
                site.provider.clone()
            };
            let page_fix = site
                .features
                .iter()
                .any(|f| f.id == "workshop-fix" && f.enabled);
            return Decision::Accelerate {
                upstream_sni: default_sni(&provider).to_string(),
                provider,
                page_fix,
            };
        }
    }
    Decision::Direct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_and_plain() {
        assert!(domain_matches(
            "*.steamstatic.com",
            "avatars.steamstatic.com"
        ));
        assert!(domain_matches("*.steamstatic.com", "steamstatic.com"));
        assert!(!domain_matches("*.steamstatic.com", "evilsteamstatic.com"));
        assert!(domain_matches("steamcommunity.com.", "SteamCommunity.com"));
    }

    #[test]
    fn cloudflare_site_routes_to_cloudflare_provider() {
        let sites = vec![Site {
            id: "nexus".into(),
            name: "Nexus Mods".into(),
            group: "game".into(),
            domains: "www.nexusmods.com · staticdelivery.nexusmods.com".into(),
            enabled: true,
            provider: "cloudflare".into(),
            note: String::new(),
            custom: false,
            features: vec![],
        }];
        match decide("www.nexusmods.com", &sites) {
            Decision::Accelerate { provider, .. } => assert_eq!(provider, "cloudflare"),
            Decision::Direct => panic!("Nexus Mods 应走 Cloudflare 池"),
        }
        match decide("staticdelivery.nexusmods.com", &sites) {
            Decision::Accelerate { provider, .. } => assert_eq!(provider, "cloudflare"),
            Decision::Direct => panic!("Nexus 静态域名应走 Cloudflare 池"),
        }
    }

    #[test]
    fn split_mixed_separators() {
        let v = split_domains("github.com · api.github.com, raw.githubusercontent.com");
        assert_eq!(
            v,
            vec!["github.com", "api.github.com", "raw.githubusercontent.com"]
        );
    }

    #[test]
    fn epic_shards_download_only() {
        // 下载域名走各自独立池（真域名 SNI，不再误用 GitHub 别名）
        assert_eq!(
            epic_shard("fastly-download.epicgames.com"),
            Some("epic-fastly")
        );
        assert_eq!(
            epic_shard("epicgames-download1.akamaized.net"),
            Some("epic-akamai")
        );
        // 网页域名未阻断，返回 None 表示直连
        assert_eq!(epic_shard("store.epicgames.com"), None);
        assert_eq!(epic_shard("launcher.epicgames.com"), None);
    }

    #[test]
    fn epic_sni_is_real_domain() {
        // Epic 两池不伪装别名：SNI 就是真域名，由节点自带 sni 提供
        assert_eq!(default_sni("epic-fastly"), "");
        assert_eq!(default_sni("epic-akamai"), "");
    }
}

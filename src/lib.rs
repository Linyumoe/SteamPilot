pub mod ca;
pub mod capture;
pub mod core;
pub mod proxy;
pub mod selector;
pub mod tray;

use ca::CaStore;
use proxy::{ProxyHandle, SharedState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, State};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub status: String,
    pub nodes: usize,
    pub latency_ms: u32,
    pub note: String,
}

/// 站点内置的可选增强（例如 Steam 的创意工坊预览修复）。
/// 它是站点的一部分，不是独立条目——界面上只在站点详情里出现。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Feature {
    pub id: String,
    pub name: String,
    pub note: String,
    pub enabled: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub id: String,
    pub name: String,
    pub group: String,
    pub domains: String,
    pub enabled: bool,
    pub provider: String,
    pub note: String,
    pub custom: bool,
    pub features: Vec<Feature>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub ip: String,
    pub provider: String,
    pub group: String,
    pub sni: String,
    pub host: String,
    pub latency_ms: u32,
    pub status: String,
}

/// 优选漏斗的三段保留数 + 探测参数。界面上直接暴露这三段，因为它们决定最终节点数与稳定性。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub stage1_keep: usize,
    pub stage2_keep: usize,
    pub stage3_keep: usize,
    pub concurrency: usize,
    pub handshake_timeout_ms: u32,
    pub sample_bytes: u32,
    pub alias: String,
    /// 复用 5 分钟内的优选结果，减少重复测速
    pub reuse_recent: bool,
    /// 延迟检测前将每个 IPv4 的末段随机替换为 10~254
    pub randomize_tail: bool,
    /// 每批测速 IP 上限，超过后分批测速统一排名
    pub batch_size: usize,
    /// 低于该速率的节点不进入结果（Mbps）
    pub min_speed_mbps: u32,
    /// 手动与启动测速总限速（Mbps）
    pub total_limit_mbps: u32,
    /// 每条线路各自的下载测速标的：provider id -> HTTPS URL。
    /// 四家 CDN 的测速链接不能共用：拿 Akamai 的下载链接去测 Cloudflare 节点，
    /// Host / SNI 对不上，测出来的速率是假的。加新线路时只加这里的映射。
    #[serde(default = "default_speed_urls")]
    pub speed_test_urls: HashMap<String, String>,
}

/// 默认测速标的：每条线路用自己 CDN 上的真实大文件，避免串线测速。
fn default_speed_urls() -> HashMap<String, String> {
    let mut m = HashMap::new();
    // 必须是各 CDN 上真实存在、支持 Range 的大文件；路径残缺会拿到 404 短响应，
    // 算出的速率远低于门槛，把好节点全误杀（曾踩坑）。
    m.insert(
        "akamai".into(),
        "https://origin-a.akamaihd.net/EA-Desktop-Client-Download/installer-releases/EA%20app.dmg"
            .into(),
    );
    m.insert(
        "fastly".into(),
        "https://download.scdn.co/SpotifySetup.exe".into(),
    );
    // Epic 两池各用自己域名的安装包（2026-11-26 实测 Range 206 可用），
    // 真域名 SNI + 标准验签，和节点池里记录的 SNI 一致。
    let epic_dl = "https://epicgames-download1.akamaized.net/Builds/UnrealEngineLauncher/Installers/Windows/EpicInstaller-20.3.3.msi?launcherfilename=EpicInstaller-20.3.3.msi";
    m.insert(
        "epic-fastly".into(),
        epic_dl.replace("epicgames-download1", "fastly-download"),
    );
    m.insert("epic-akamai".into(), epic_dl.into());
    m.insert(
        "cloudflare".into(),
        "https://speed.cloudflare.com/__down?bytes=8388608".into(),
    );
    m.insert(
        "cloudfront".into(),
        "https://images-assets.nasa.gov/video/GSFC_20171208_Archive_e000380/GSFC_20171208_Archive_e000380.mp4".into(),
    );
    m
}

/// 流量接管方式：系统代理 + 本地 PAC（不装驱动、不需要管理员权限）。
/// 只把勾选站点的域名指到本地代理，其余直连；退出时恢复用户原代理设置。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capture {
    /// 退出时自动恢复用户原来的系统代理设置
    pub auto_restore: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    pub running: bool,
    pub ca_installed: bool,
    /// 是否已跑过一次 IP 优选（首页四步引导的第 2 步）
    pub nodes_optimized: bool,
    pub startup: bool,
    pub silent_start: bool,
    pub theme: String,
    pub providers: Vec<Provider>,
    pub sites: Vec<Site>,
    pub nodes: Vec<Node>,
    pub logs: Vec<String>,
    pub policy: Policy,
    pub capture: Capture,
}

fn parse_ms(s: &str) -> u32 {
    s.trim().trim_end_matches("ms").parse::<u32>().unwrap_or(0)
}

fn parse_nodes() -> Vec<Node> {
    let mut out = Vec::new();

    // Akamai: "23.220.68.149  471ms  steamvideo-a.akamaihd.net"
    for line in include_str!("../resources/nodes/akamai_stable.txt").lines() {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() >= 3 && p[0].contains('.') {
            out.push(Node {
                ip: p[0].to_string(),
                provider: "akamai".into(),
                group: "Steam 稳定池".into(),
                sni: p[2].to_string(),
                host: "steamcommunity.com".into(),
                latency_ms: parse_ms(p[1]),
                status: "稳定".into(),
            });
        }
    }

    // GitHub A: "20.6.7.17  617ms  4731B HTTP/1.1 200 OK"
    // 微软自有 IP：SNI 就是真域名、标准验签，不需要别名伪装。
    for line in include_str!("../resources/nodes/github_a_good.txt").lines() {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() >= 2 && p[0].contains('.') {
            out.push(Node {
                ip: p[0].to_string(),
                provider: "github-ms".into(),
                group: "GitHub A / 微软".into(),
                sni: "github.com".into(),
                host: "github.com".into(),
                latency_ms: parse_ms(p[1]),
                status: "可用".into(),
            });
        }
    }

    // GitHub B: "185.199.110.154  329ms  4731B HTTP/1.1 200 OK"
    for line in include_str!("../resources/nodes/github_b_good.txt").lines() {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() >= 2 && p[0].contains('.') {
            out.push(Node {
                ip: p[0].to_string(),
                provider: "fastly".into(),
                group: "GitHub B / Fastly".into(),
                sni: "desktop.githubusercontent.com".into(),
                host: "raw.githubusercontent.com".into(),
                latency_ms: parse_ms(p[1]),
                status: "稳定".into(),
            });
        }
    }

    // Epic 双池：下载域名未被 SNI 阻断，真域名 SNI + 标准验签即可。
    // 格式: "IP  延迟ms  SNI(真域名)"
    fn push_epic(out: &mut Vec<Node>, data: &str, provider: &str, group: &str, host: &str) {
        for line in data.lines() {
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() >= 3 && p[0].contains('.') && !p[0].starts_with('#') {
                out.push(Node {
                    ip: p[0].to_string(),
                    provider: provider.into(),
                    group: group.into(),
                    sni: p[2].to_string(),
                    host: host.into(),
                    latency_ms: p[1].trim_end_matches("ms").parse().unwrap_or(0),
                    status: "稳定".into(),
                });
            }
        }
    }
    push_epic(
        &mut out,
        include_str!("../resources/nodes/epic_fastly.txt"),
        "epic-fastly",
        "Epic / Fastly",
        "fastly-download.epicgames.com",
    );
    push_epic(
        &mut out,
        include_str!("../resources/nodes/epic_akamai.txt"),
        "epic-akamai",
        "Epic / Akamai",
        "epicgames-download1.akamaized.net",
    );
    out
}

fn feature(id: &str, name: &str, note: &str, enabled: bool) -> Feature {
    Feature {
        id: id.into(),
        name: name.into(),
        note: note.into(),
        enabled,
    }
}

#[allow(clippy::too_many_arguments)]
fn site(
    id: &str,
    name: &str,
    group: &str,
    domains: &str,
    provider: &str,
    note: &str,
    enabled: bool,
    features: Vec<Feature>,
) -> Site {
    Site {
        id: id.into(),
        name: name.into(),
        group: group.into(),
        domains: domains.into(),
        provider: provider.into(),
        note: note.into(),
        enabled,
        custom: false,
        features,
    }
}

impl Default for AppState {
    fn default() -> Self {
        let nodes = parse_nodes();
        let count = |p: &str| nodes.iter().filter(|n| n.provider == p).count();
        let best = |p: &str| {
            nodes
                .iter()
                .filter(|n| n.provider == p)
                .map(|n| n.latency_ms)
                .min()
                .unwrap_or(0)
        };
        Self {
            running: false,
            ca_installed: false,
            nodes_optimized: false,
            startup: false,
            silent_start: true,
            theme: "light".into(),
            providers: vec![
                Provider {
                    id: "akamai".into(),
                    name: "Akamai".into(),
                    enabled: true,
                    status: "已优选".into(),
                    nodes: count("akamai"),
                    latency_ms: best("akamai"),
                    note: "Steam 商店 / 社区 / 创意工坊".into(),
                },
                Provider {
                    id: "fastly".into(),
                    name: "Fastly".into(),
                    enabled: true,
                    status: "已优选".into(),
                    nodes: count("fastly"),
                    latency_ms: best("fastly"),
                    note: "GitHub 静态资源".into(),
                },
                Provider {
                    id: "epic-fastly".into(),
                    name: "Epic / Fastly".into(),
                    enabled: true,
                    status: "已优选".into(),
                    nodes: count("epic-fastly"),
                    latency_ms: best("epic-fastly"),
                    note: "Epic 下载（真域名，未阻断）".into(),
                },
                Provider {
                    id: "epic-akamai".into(),
                    name: "Epic / Akamai".into(),
                    enabled: true,
                    status: "已优选".into(),
                    nodes: count("epic-akamai"),
                    latency_ms: best("epic-akamai"),
                    note: "Epic 下载备用（真域名）".into(),
                },
                Provider {
                    id: "cloudflare".into(),
                    name: "Cloudflare".into(),
                    enabled: false,
                    status: "待接入".into(),
                    nodes: 0,
                    latency_ms: 0,
                    note: "Nexus Mods（v1 后）".into(),
                },
                Provider {
                    id: "cloudfront".into(),
                    name: "CloudFront".into(),
                    enabled: false,
                    status: "待接入".into(),
                    nodes: 0,
                    latency_ms: 0,
                    note: "Twitch / AWS 资源（v1 后）".into(),
                },
            ],
            sites: vec![
                // ---- 游戏平台 ----
                site(
                    "steam",
                    "Steam 商店与社区",
                    "game",
                    "store.steampowered.com · steamcommunity.com · api.steampowered.com · avatars.steamstatic.com",
                    "akamai",
                    "别名 SNI + 真实 Host，证书按别名严格校验",
                    true,
                    vec![feature(
                        "workshop-fix",
                        "创意工坊预览大图修复",
                        "工坊页 iframe_api 加载失败导致预览不显示，注入一次 onYouTubeIframeAPIReady() 调用即可",
                        false,
                    )],
                ),
                site(
                    "epic",
                    "Epic Games",
                    "game",
                    "store.epicgames.com · fastly-download.epicgames.com · epicgames-download1.akamaized.net",
                    "epic-fastly",
                    "下载走 Epic 独立 Fastly/Akamai 池（真域名）；网页直连",
                    true,
                    vec![],
                ),
                site(
                    "nexus",
                    "Nexus Mods",
                    "game",
                    "www.nexusmods.com · staticdelivery.nexusmods.com",
                    "cloudflare",
                    "Cloudflare 线路上线后开放",
                    false,
                    vec![],
                ),
                // ---- 开发者工具 ----
                site(
                    "github",
                    "GitHub",
                    "dev",
                    "github.com · api.github.com · raw.githubusercontent.com · github.githubassets.com",
                    "fastly",
                    "B 组 Fastly 优选为主，A 组微软自有点位备份",
                    true,
                    vec![],
                ),
            ],
            nodes,
            logs: vec![
                "[系统] SteamPilot v0.1.0 已加载".into(),
                "[优选] 已载入 Akamai / GitHub 实测节点池".into(),
                "[提示] 代理与 TUN 链路将在 M1 接入".into(),
            ],
            policy: Policy {
                stage1_keep: 8,
                stage2_keep: 4,
                stage3_keep: 2,
                concurrency: 8,
                handshake_timeout_ms: 4000,
                sample_bytes: 8,
                alias: "auto".into(),
                reuse_recent: true,
                randomize_tail: true,
                batch_size: 1,
                min_speed_mbps: 10,
                total_limit_mbps: 1000,
                speed_test_urls: default_speed_urls(),
            },
            capture: Capture {
                auto_restore: true,
            },
        }
    }
}

const PROXY_PORT: u16 = 7890;

/// 运行时资源：CA、代理句柄。不序列化，只由 Tauri 管理。
pub struct Backend {
    pub ca: Arc<CaStore>,
    pub proxy: Mutex<Option<ProxyHandle>>,
}

fn commit(app: &tauri::AppHandle, mutex: &SharedState) -> AppState {
    let snapshot = {
        let guard = mutex.lock().unwrap();
        guard.clone()
    };
    let _ = app.emit("state-changed", &snapshot);
    snapshot
}

#[tauri::command]
fn get_state(state: State<'_, SharedState>) -> AppState {
    state.lock().unwrap().clone()
}

#[tauri::command]
async fn set_running(
    running: bool,
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
    backend: State<'_, Backend>,
) -> Result<AppState, String> {
    if running {
        // 未装 CA 则下游 TLS 无法被信任，拒绝启动并提示。
        let ca_ok = state.lock().unwrap().ca_installed;
        if !ca_ok {
            state
                .lock()
                .unwrap()
                .logs
                .push("[加速] 启动失败：请先安装本地根证书".into());
            return Ok(commit(&app, &state));
        }
        let addr: SocketAddr = format!("127.0.0.1:{}", PROXY_PORT).parse().unwrap();
        let started = proxy::start(addr, backend.ca.clone(), state.inner().clone()).await;
        match started {
            Ok(handle) => {
                // 代理起来后再接管：写 PAC + 系统代理。失败则回滚代理，不留半开状态。
                let applied = capture::apply(&state.lock().unwrap());
                match applied {
                    Ok(()) => {
                        *backend.proxy.lock().unwrap() = Some(handle);
                        let mut s = state.lock().unwrap();
                        s.running = true;
                        s.logs
                            .push(format!("[加速] 已启动，系统代理指向本地 PAC（{}）", addr));
                    }
                    Err(e) => {
                        handle.stop();
                        state
                            .lock()
                            .unwrap()
                            .logs
                            .push(format!("[加速] 设置系统代理失败：{}", e));
                    }
                }
            }
            Err(e) => {
                state
                    .lock()
                    .unwrap()
                    .logs
                    .push(format!("[加速] 启动失败：{}", e));
            }
        }
    } else {
        if let Some(h) = backend.proxy.lock().unwrap().take() {
            h.stop();
        }
        // 恢复用户原来的系统代理设置（auto_restore 关闭时保留接管供排查）。
        let auto_restore = state.lock().unwrap().capture.auto_restore;
        if auto_restore {
            if let Err(e) = capture::restore() {
                state
                    .lock()
                    .unwrap()
                    .logs
                    .push(format!("[加速] 恢复系统代理失败：{}", e));
            }
        }
        let mut s = state.lock().unwrap();
        s.running = false;
        s.logs.push("[加速] 已停止，系统代理已恢复".into());
    }
    Ok(commit(&app, &state))
}

#[tauri::command]
fn toggle_provider(id: String, app: tauri::AppHandle, state: State<'_, SharedState>) -> AppState {
    {
        let mut s = state.lock().unwrap();
        let changed = if let Some(p) = s.providers.iter_mut().find(|p| p.id == id) {
            p.enabled = !p.enabled;
            Some((p.name.clone(), p.enabled))
        } else {
            None
        };
        if let Some((name, enabled)) = changed {
            s.logs.push(format!(
                "[CDN] {} 已{}",
                name,
                if enabled { "启用" } else { "停用" }
            ));
        }
    }
    commit(&app, &state)
}

#[tauri::command]
fn toggle_site(id: String, app: tauri::AppHandle, state: State<'_, SharedState>) -> AppState {
    {
        let mut s = state.lock().unwrap();
        let changed = if let Some(site) = s.sites.iter_mut().find(|x| x.id == id) {
            site.enabled = !site.enabled;
            Some((site.name.clone(), site.enabled))
        } else {
            None
        };
        if let Some((name, enabled)) = changed {
            s.logs.push(format!(
                "[网站] {} 已{}",
                name,
                if enabled {
                    "加入加速"
                } else {
                    "移出加速"
                }
            ));
        }
    }
    commit(&app, &state)
}

#[tauri::command]
fn set_sites_enabled(
    ids: Vec<String>,
    enabled: bool,
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
) -> AppState {
    {
        let mut s = state.lock().unwrap();
        let mut n = 0usize;
        for site in s.sites.iter_mut() {
            if ids.iter().any(|id| id == &site.id) {
                site.enabled = enabled;
                n += 1;
            }
        }
        s.logs.push(format!(
            "[网站] 批量{} {} 条规则",
            if enabled { "启用" } else { "停用" },
            n
        ));
    }
    commit(&app, &state)
}

#[tauri::command]
fn toggle_feature(
    site: String,
    feature: String,
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
) -> AppState {
    {
        let mut s = state.lock().unwrap();
        let changed = s
            .sites
            .iter_mut()
            .find(|x| x.id == site)
            .and_then(|x| x.features.iter_mut().find(|f| f.id == feature))
            .map(|f| {
                f.enabled = !f.enabled;
                (f.name.clone(), f.enabled)
            });
        if let Some((name, on)) = changed {
            s.logs.push(format!(
                "[修复] {} 已{}",
                name,
                if on { "开启" } else { "关闭" }
            ));
        }
    }
    commit(&app, &state)
}

#[tauri::command]
fn add_custom_site(
    name: String,
    domains: String,
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
) -> AppState {
    {
        let mut s = state.lock().unwrap();
        let id = format!("custom-{}", s.sites.len() + 1);
        s.sites.push(Site {
            id,
            name,
            group: "custom".into(),
            domains,
            enabled: true,
            provider: "auto".into(),
            note: "用户自定义规则".into(),
            custom: true,
            features: vec![],
        });
        s.logs.push("[网站] 已添加自定义加速规则".into());
    }
    commit(&app, &state)
}

#[tauri::command]
fn remove_site(id: String, app: tauri::AppHandle, state: State<'_, SharedState>) -> AppState {
    {
        let mut s = state.lock().unwrap();
        if let Some(pos) = s.sites.iter().position(|x| x.id == id && x.custom) {
            let site = s.sites.remove(pos);
            s.logs
                .push(format!("[网站] 已删除自定义规则：{}", site.name));
        }
    }
    commit(&app, &state)
}

#[tauri::command]
fn set_theme(theme: String, app: tauri::AppHandle, state: State<'_, SharedState>) -> AppState {
    {
        let mut s = state.lock().unwrap();
        s.theme = theme;
    }
    commit(&app, &state)
}

#[tauri::command]
fn set_flags(
    startup: Option<bool>,
    silent_start: Option<bool>,
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
) -> AppState {
    {
        let mut s = state.lock().unwrap();
        if let Some(v) = startup {
            s.startup = v;
            tray::set_autostart(v);
        }
        if let Some(v) = silent_start {
            s.silent_start = v;
        }
    }
    commit(&app, &state)
}

#[tauri::command]
fn update_policy(patch: Policy, app: tauri::AppHandle, state: State<'_, SharedState>) -> AppState {
    {
        let mut s = state.lock().unwrap();
        // 保留数必须逐段递减，否则漏斗逻辑不成立；这里兜底纠正一次。
        // stage1_keep=0 表示全部 IP 参与后续检测（s302 的同名语义）。
        let mut p = patch;
        p.stage1_keep = p.stage1_keep.clamp(0, 256);
        p.stage2_keep = p.stage2_keep.clamp(1, p.stage1_keep.max(1));
        p.stage3_keep = p.stage3_keep.clamp(1, p.stage2_keep);
        p.concurrency = p.concurrency.clamp(1, 128);
        p.handshake_timeout_ms = p.handshake_timeout_ms.clamp(500, 15000);
        p.sample_bytes = p.sample_bytes.clamp(1, 64);
        p.batch_size = p.batch_size.clamp(1, 64);
        p.min_speed_mbps = p.min_speed_mbps.clamp(1, 10000);
        p.total_limit_mbps = p.total_limit_mbps.clamp(10, 10000);
        // 候选选择固定为延迟优先；每条线路缺测速标的时用内置默认补齐，
        // 不允许空 URL 把某条线路的测速变成"用 Akamai 链接测 Cloudflare"。
        let defaults = default_speed_urls();
        for (id, url) in defaults {
            p.speed_test_urls.entry(id).or_insert(url);
        }
        for url in p.speed_test_urls.values_mut() {
            *url = url.trim().to_string();
        }
        p.speed_test_urls.retain(|_, url| !url.is_empty());
        s.policy = p;
        let (a, b, c) = (
            s.policy.stage1_keep,
            s.policy.stage2_keep,
            s.policy.stage3_keep,
        );
        s.logs
            .push(format!("[优选] 漏斗更新：{} → {} → {} 个", a, b, c));
    }
    commit(&app, &state)
}

#[tauri::command]
fn update_capture(
    patch: Capture,
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
) -> AppState {
    {
        let mut s = state.lock().unwrap();
        s.capture = patch;
        s.logs.push("[接管] 代理接管设置已更新".into());
    }
    commit(&app, &state)
}

#[tauri::command]
fn install_ca(
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
    backend: State<'_, Backend>,
) -> AppState {
    let result = backend.ca.install();
    {
        let mut s = state.lock().unwrap();
        match result {
            Ok(msg) => {
                s.ca_installed = true;
                s.logs.push(format!("[证书] {}", msg));
            }
            Err(e) => s.logs.push(format!("[证书] 安装失败：{}", e)),
        }
    }
    commit(&app, &state)
}

#[tauri::command]
fn uninstall_ca(
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
    backend: State<'_, Backend>,
) -> AppState {
    let result = backend.ca.uninstall();
    {
        let mut s = state.lock().unwrap();
        match result {
            Ok(msg) => {
                s.ca_installed = false;
                s.running = false;
                s.logs.push(format!("[证书] {}", msg));
            }
            Err(e) => s.logs.push(format!("[证书] 卸载失败：{}", e)),
        }
    }
    commit(&app, &state)
}

#[tauri::command]
async fn refresh_nodes(
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
) -> Result<AppState, String> {
    // 抓取快照，避免长时间持锁
    let (nodes, providers, policy) = {
        let s = state.lock().unwrap();
        (
            s.nodes.clone(),
            s.providers
                .iter()
                .filter(|p| p.enabled)
                .map(|p| p.id.clone())
                .collect::<Vec<_>>(),
            s.policy.clone(),
        )
    };
    state.lock().unwrap().logs.push(format!(
        "[优选] 开始探测 {} 个节点（并发 {}）…",
        nodes.len(),
        policy.concurrency
    ));

    let (updated, stages) = selector::refresh(nodes, &providers, &policy).await;
    {
        let mut s = state.lock().unwrap();
        let online = s.nodes.len();
        s.nodes = updated;
        let active = s
            .nodes
            .iter()
            .filter(|n| {
                n.status == "在线" || n.status == "稳定" || n.status == "可用" || n.status == "候选"
            })
            .count();
        if active > 0 {
            s.nodes_optimized = true;
        }
        s.logs.push(format!(
            "[优选] 探测完成：漏斗 {} → {} → {} 个，在线 {}/{}",
            stages[0], stages[1], stages[2], active, online
        ));
    }
    Ok(commit(&app, &state))
}

#[tauri::command]
fn get_logs(state: State<'_, SharedState>) -> Vec<String> {
    state.lock().unwrap().logs.clone()
}

#[tauri::command]
fn clear_logs(app: tauri::AppHandle, state: State<'_, SharedState>) -> AppState {
    {
        let mut s = state.lock().unwrap();
        s.logs.clear();
        s.logs.push("[系统] 日志已清空".into());
    }
    commit(&app, &state)
}

pub fn run() {
    // 初始化 rustls 加密提供者（ring）
    let _ = rustls::crypto::ring::default_provider().install_default();

    let ca = Arc::new(CaStore::load_or_create().expect("init local CA"));
    let backend = Backend {
        ca,
        proxy: Mutex::new(None),
    };
    let silent = tray::silent_launch();

    let app = tauri::Builder::default()
        .manage(Arc::new(Mutex::new(AppState::default())) as SharedState)
        .manage(backend)
        // 第二实例把参数交给首个实例后自行退出；回调里唤起已有窗口。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .setup(move |app| {
            tray::setup(app)?;
            if silent {
                if let Some(w) = app.get_webview_window("main") {
                    w.hide()?;
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_running,
            toggle_provider,
            toggle_site,
            set_sites_enabled,
            toggle_feature,
            add_custom_site,
            remove_site,
            set_theme,
            set_flags,
            update_policy,
            update_capture,
            install_ca,
            uninstall_ca,
            refresh_nodes,
            get_logs,
            clear_logs
        ])
        .build(tauri::generate_context!())
        .expect("failed to build SteamPilot");

    // 进程退出（含托盘「退出」直接 app.exit）时兜底恢复系统代理，避免用户断网。
    app.run(|_app, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            let _ = capture::restore();
        }
    });
}

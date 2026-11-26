//! 优选器：对节点池做并发 TCP+TLS 握手探测，按漏斗保留 Top-K。
//!
//! 握手严格按节点自带 SNI 验签（不跳过验证），失败的节点标记离线。
//! 漏斗：延迟初筛 stage1 → 合格留 stage2 → 下载测速留 stage3。
//!
//! 两类连接的 SNI 策略不同，勿混淆：
//!   - 网页请求：别名 SNI + 真实 Host（绕过 SNI 阻断），见 core/proxy。
//!   - 下载测速：直接用测速域名自身 SNI（证书天然匹配，无需伪装），见本文件 speed_probe。

use crate::{Node, Policy};
use rustls::pki_types::ServerName;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

fn tls_connector() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// 从测速 URL 拆出 (host, path)。只接受 https，够用且不用引额外依赖。
fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() {
        return None;
    }
    Some((authority, path))
}

/// 下载测速：连节点 IP，用测速域名自身的 SNI（标准验签），发 Range GET 取样，
/// 返回吞吐 Mbps。测速域名是该 CDN 的合法域名、证书天然匹配，所以不伪装 SNI。
async fn speed_probe(
    node: &Node,
    url: &str,
    sample_bytes: usize,
    timeout_ms: u64,
    connector: &TlsConnector,
) -> Option<f64> {
    let (host, path) = split_url(url)?;
    let addr = format!("{}:443", node.ip);
    let tcp = timeout(Duration::from_millis(timeout_ms), TcpStream::connect(&addr))
        .await
        .ok()?
        .ok()?;
    let name = ServerName::try_from(host.to_string()).ok()?;
    let mut tls = timeout(
        Duration::from_millis(timeout_ms),
        connector.connect(name, tcp),
    )
    .await
    .ok()?
    .ok()?;
    let req = format!(
        "GET /{} HTTP/1.1\r\nHost: {}\r\nRange: bytes=0-{}\r\nUser-Agent: SteamPilot/0.1\r\nConnection: close\r\n\r\n",
        path, host, sample_bytes
    );
    let t0 = Instant::now();
    let got = timeout(Duration::from_millis(timeout_ms), async {
        tls.write_all(req.as_bytes()).await.ok()?;
        let mut buf = [0u8; 16384];
        let mut total = 0usize;
        while total < sample_bytes {
            let n = tls.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            total += n;
        }
        Some(total)
    })
    .await
    .ok()??;
    let secs = t0.elapsed().as_secs_f64();
    if secs <= 0.0 || got == 0 {
        return None;
    }
    // got 含少量响应头，取样 8MB 时误差可忽略。
    Some(got as f64 * 8.0 / secs / 1_000_000.0)
}

/// 探测单节点：TCP 连接 + 按 SNI 严格验签的 TLS 握手，返回耗时 ms。
pub async fn probe_node(node: &Node, timeout_ms: u64, connector: &TlsConnector) -> Option<u32> {
    let t0 = Instant::now();
    let addr = format!("{}:443", node.ip);
    let stream = timeout(Duration::from_millis(timeout_ms), TcpStream::connect(&addr))
        .await
        .ok()?
        .ok()?;
    let name = ServerName::try_from(node.sni.clone()).ok()?;
    let mut tls = timeout(
        Duration::from_millis(timeout_ms),
        connector.connect(name, stream),
    )
    .await
    .ok()?
    .ok()?;
    // 发一个最小的 GET，确认 HTTP 层可用，避免只有握手成功但 Host 路由失败的节点
    let req = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nUser-Agent: SteamPilot/0.1\r\nConnection: close\r\n\r\n",
        node.host
    );
    timeout(Duration::from_millis(timeout_ms), async {
        tls.write_all(req.as_bytes()).await.ok()?;
        let mut buf = [0u8; 16];
        let n = tls.read(&mut buf).await.ok()?;
        Some(n)
    })
    .await
    .ok()??;
    Some(t0.elapsed().as_millis() as u32)
}

/// 对节点池按 provider 并发探测，返回 (更新后的节点列表, 各阶段存活数)。
/// 只探测 enabled 的 provider 对应的节点。
pub async fn refresh(
    nodes: Vec<Node>,
    enabled_providers: &[String],
    policy: &Policy,
) -> (Vec<Node>, [usize; 3]) {
    let connector = TlsConnector::from(tls_connector());
    let sem = Arc::new(tokio::sync::Semaphore::new(policy.concurrency.max(1)));

    let mut handles = Vec::new();
    for node in nodes.into_iter() {
        if !enabled_providers.iter().any(|p| p == &node.provider) {
            handles.push(tokio::spawn(async move { (node, None) }));
            continue;
        }
        let connector = connector.clone();
        let sem = sem.clone();
        let tmo = policy.handshake_timeout_ms as u64;
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok();
            let r = probe_node(&node, tmo, &connector).await;
            (node, r)
        }));
    }

    let mut measured: Vec<(Node, Option<u32>)> = Vec::new();
    for h in handles {
        if let Ok((mut node, r)) = h.await {
            match r {
                Some(ms) => {
                    node.latency_ms = ms;
                    node.status = "在线".into();
                }
                None => node.status = "离线".into(),
            }
            measured.push((node, r));
        }
    }

    // 阶段一：延迟初筛——在线节点按延迟排序，留 stage1_keep。
    let mut online: Vec<Node> = measured
        .iter()
        .filter_map(|(n, r)| r.map(|_| n.clone()))
        .collect();
    online.sort_by_key(|n| n.latency_ms);
    // 0 表示区间内全部 IP，不做截断
    let s1 = if policy.stage1_keep == 0 {
        online.len()
    } else {
        online.len().min(policy.stage1_keep)
    };
    online.truncate(s1);

    // 阶段二：证书校验——能通过严格验签即合格（probe 已含验签），留 stage2_keep。
    let s2 = online.len().min(policy.stage2_keep);
    online.truncate(s2);

    // 阶段三：下载测速——用每条线路自己的测速标的做 Range GET 取样。
    // 低于 min_speed_mbps 的淘汰；无测速标的的线路（如 github-ms 微软 IP）保留、按延迟排。
    let sample = policy.sample_bytes.saturating_mul(1024) as usize * 1024;
    // 下载取样比握手慢，给 3 倍超时；只跑 stage2 存活的少数节点，不影响总体耗时。
    let tmo = (policy.handshake_timeout_ms as u64 * 3).max(6000);
    let min_mbps = policy.min_speed_mbps as f64;
    let mut jobs = Vec::new();
    for n in online {
        let url = policy.speed_test_urls.get(&n.provider).cloned();
        let connector = connector.clone();
        jobs.push(tokio::spawn(async move {
            let mbps = match url {
                Some(u) => speed_probe(&n, &u, sample, tmo, &connector).await,
                None => None, // 该线路没配测速标的，回退用延迟
            };
            (n, mbps)
        }));
    }

    let mut scored: Vec<(Node, Option<f64>)> = Vec::new();
    for h in jobs {
        if let Ok(v) = h.await {
            scored.push(v);
        }
    }
    // 有测速值且达标的排前（按吞吐降序）；达不到的淘汰；无测速值的按延迟排最后保留。
    scored.sort_by(|a, b| match (a.1, b.1) {
        (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.0.latency_ms.cmp(&b.0.latency_ms),
    });
    let mut active = Vec::new();
    let mut reserve = Vec::new();
    for (n, mbps) in scored {
        let fast_enough = mbps.map(|v| v >= min_mbps).unwrap_or(true);
        if fast_enough && active.len() < policy.stage3_keep {
            active.push(n);
        } else {
            reserve.push(n);
        }
    }
    for n in reserve.iter_mut() {
        n.status = "候选".into();
    }
    let s3 = active.len();
    active.extend(reserve);
    let online = active;

    // 合并离线节点，保持界面能看到全量池。
    let mut result: Vec<Node> = measured
        .into_iter()
        .filter_map(|(n, r)| if r.is_none() { Some(n) } else { None })
        .collect();
    result.extend(online);
    result.sort_by_key(|n| (n.status == "离线", n.latency_ms));
    (result, [s1, s2, s3])
}

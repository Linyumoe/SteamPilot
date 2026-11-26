//! 本地 HTTPS 代理：CONNECT 隧道 + 本地 CA MITM + 上游优选节点转发。
//!
//! 数据流：
//!   浏览器 --(CONNECT + TLS, SNI=真域名)--> 本地代理
//!   本地代理 --(本地CA签发叶子证书)--> 浏览器完成 TLS
//!   本地代理 --(解析HTTP, 读Host)--> 分流决策
//!     命中: --(TLS, SNI=别名, 严格验签, HTTP Host=真域名)--> 优选节点
//!     未命中: --(TLS, SNI=真域名, 标准验签)--> 真实上游
//!
//! v1 只处理 HTTP/1.1；每连接一个请求后关闭，正确性优先。

use crate::ca::CaStore;
use crate::core::{decide, Decision};
use crate::{AppState, Node};
use anyhow::{anyhow, Context, Result};
use rustls::pki_types::ServerName;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

pub struct ProxyHandle {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl ProxyHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub type SharedState = Arc<Mutex<AppState>>;

fn upstream_connector() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// 单个节点的在线表现：真实请求 RTT 的 EWMA + 连续失败次数。
#[derive(Clone, Copy)]
struct Metric {
    rtt_ewma: f64,
    fails: u32,
}

/// 在线反馈表（进程内）：清单要求「真实 RTT/失败回写评分，权重高于主动探测」。
/// 实测 RTT 一旦拿到就覆盖探测延迟；失败只降权、成功即清零，不永久剔除节点。
#[derive(Default)]
struct Feedback(Mutex<HashMap<String, Metric>>);

impl Feedback {
    fn ok(&self, key: &str, rtt_ms: u64) {
        let mut m = self.0.lock().unwrap();
        let e = m.entry(key.to_string()).or_insert(Metric {
            rtt_ewma: 0.0,
            fails: 0,
        });
        e.rtt_ewma = if e.rtt_ewma == 0.0 {
            rtt_ms as f64
        } else {
            e.rtt_ewma * 0.7 + rtt_ms as f64 * 0.3
        };
        e.fails = 0;
    }

    fn fail(&self, key: &str) {
        let mut m = self.0.lock().unwrap();
        let e = m.entry(key.to_string()).or_insert(Metric {
            rtt_ewma: 0.0,
            fails: 0,
        });
        e.fails = e.fails.saturating_add(1);
    }

    /// 排序键：(active 档位, 有效耗时)。实测 RTT 优先于探测延迟，失败加惩罚。
    fn score(&self, node: &Node) -> (u8, f64) {
        let m = self.0.lock().unwrap();
        let metric = m.get(&node.ip);
        let base = metric
            .filter(|x| x.rtt_ewma > 0.0)
            .map(|x| x.rtt_ewma)
            .unwrap_or(node.latency_ms as f64);
        let penalty = metric.map(|x| x.fails as f64 * 3000.0).unwrap_or(0.0);
        let active = matches!(node.status.as_str(), "在线" | "稳定" | "可用");
        (!active as u8, base + penalty)
    }
}

/// 最多尝试的候选节点数（首个失败时依次切换）。
const MAX_CANDIDATES: usize = 3;

/// 按 provider 取候选：active 优先，实测评分排序，供失败时依次切换。
fn pick_nodes<'a>(nodes: &'a [Node], provider: &str, fb: &Feedback) -> Vec<&'a Node> {
    let mut usable: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.provider == provider && n.status != "离线")
        .collect();
    usable.sort_by(|a, b| {
        fb.score(a)
            .partial_cmp(&fb.score(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    usable.truncate(MAX_CANDIDATES);
    usable
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 读取 HTTP 头（到 \r\n\r\n）。
async fn read_head<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<String> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 4096];
    loop {
        if buf.len() > 1024 * 1024 {
            return Err(anyhow!("header too large"));
        }
        let n = r.read(&mut tmp).await.context("read head")?;
        if n == 0 {
            return Err(anyhow!("eof reading head"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if find_sub(&buf, b"\r\n\r\n").is_some() {
            return Ok(String::from_utf8_lossy(&buf).to_string());
        }
    }
}

fn parse_request(head: &str) -> Option<(String, String)> {
    let first = head.lines().next()?.trim();
    let mut parts = first.split_whitespace();
    let _method = parts.next()?.to_string();
    let _path = parts.next()?.to_string();
    let mut host = String::new();
    for line in head.lines().skip(1) {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("host:") {
            host = v.trim().to_string();
        }
    }
    Some((_method, host))
}

/// 读一个完整请求（头 + Content-Length body）。
async fn read_request<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<String> {
    let head = read_head(r).await?;
    let mut content_len = 0usize;
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }
    if content_len > 0 && content_len < 32 * 1024 * 1024 {
        let mut body = vec![0u8; content_len];
        r.read_exact(&mut body).await.context("read body")?;
        let mut raw = head.into_bytes();
        raw.extend_from_slice(&body);
        return Ok(String::from_utf8_lossy(&raw).to_string());
    }
    Ok(head)
}

/// 重写请求头：去代理头、强制 close，Host 保持真域名。
fn rewrite_request(raw: &str, force_close: bool) -> Vec<u8> {
    let split = find_sub(raw.as_bytes(), b"\r\n\r\n").unwrap_or(raw.len());
    let head = &raw[..split];
    let body = if split < raw.len() {
        &raw[split + 4..]
    } else {
        ""
    };
    let mut out = String::new();
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("proxy-connection:")
            || lower.starts_with("proxy-authorization:")
            || (force_close && lower.starts_with("connection:"))
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if force_close {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    out.push_str(body);
    out.into_bytes()
}

const PAGE_FIX_SCRIPT: &str = "\n<script>if(typeof onYouTubeIframeAPIReady=='function'){try{onYouTubeIframeAPIReady()}catch(e){}}</script>\n";

/// 透传响应体（响应头已在 try_upstream 阶段读到并传入）；HTML 且开启修复时注入脚本。
async fn relay_response<W: AsyncWriteExt + Unpin>(
    upstream: &mut tokio_rustls::client::TlsStream<TcpStream>,
    downstream: &mut W,
    head: String,
    page_fix: bool,
) -> Result<usize> {
    let status_ok = head
        .lines()
        .next()
        .map(|l| l.contains(" 200"))
        .unwrap_or(false);
    let is_html = status_ok
        && head
            .lines()
            .any(|l| l.to_ascii_lowercase().contains("content-type: text/html"));

    let mut cleaned = String::new();
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:")
            || lower.starts_with("connection:")
            || lower.starts_with("keep-alive:")
            || lower.starts_with("content-length:")
        {
            continue;
        }
        cleaned.push_str(line);
        cleaned.push_str("\r\n");
    }
    cleaned.push_str("Connection: close\r\n\r\n");
    let head_bytes = cleaned.into_bytes();
    downstream
        .write_all(&head_bytes)
        .await
        .context("write resp head")?;

    let mut total = head_bytes.len();
    let mut buf = [0u8; 16384];
    loop {
        let n = upstream.read(&mut buf).await.context("read resp body")?;
        if n == 0 {
            break;
        }
        total += n;
        downstream
            .write_all(&buf[..n])
            .await
            .context("write resp body")?;
    }
    if page_fix && is_html {
        downstream.write_all(PAGE_FIX_SCRIPT.as_bytes()).await.ok();
        total += PAGE_FIX_SCRIPT.len();
    }
    Ok(total)
}

/// 处理一条下游连接。
async fn handle_client(
    socket: TcpStream,
    ca: Arc<CaStore>,
    app_log: Arc<dyn Fn(String) + Send + Sync>,
    state: SharedState,
    connector: Arc<rustls::ClientConfig>,
    feedback: Arc<Feedback>,
) -> Result<()> {
    let mut socket = socket;
    let head = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_head(&mut socket),
    )
    .await
    {
        Ok(v) => v?,
        Err(_) => return Err(anyhow!("CONNECT timeout")),
    };
    let first = head.lines().next().unwrap_or("").trim();
    if !first.to_ascii_uppercase().starts_with("CONNECT ") {
        socket
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
            .await
            .ok();
        return Err(anyhow!("not CONNECT: {}", first));
    }
    let target = first.split_whitespace().nth(1).unwrap_or("");
    let connect_host = target.rsplit_once(':').map(|(h, _)| h).unwrap_or(target);
    let _ = connect_host;

    socket
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .context("send CONNECT 200")?;

    let server_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(ca.clone());
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
    let mut tls = acceptor
        .accept(socket)
        .await
        .context("downstream TLS accept")?;

    let raw = match read_request(&mut tls).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let (_, req_host) = parse_request(&raw).unwrap_or_default();
    let host = if req_host.is_empty() {
        target
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_default()
    } else {
        req_host
    };

    // 决策 + 选候选：临界区尽量短。加速命中给最多 MAX_CANDIDATES 个按实测评分排序的节点，
    // 未命中/无节点则回退直连真域名（单候选、SNI=Host、标准验签）。
    let (candidates, page_fix, accelerated) = {
        let s = state.lock().unwrap();
        match decide(&host, &s.sites) {
            Decision::Direct => (vec![(host.clone(), host.clone())], false, false),
            Decision::Accelerate {
                provider,
                upstream_sni,
                page_fix,
            } => {
                let picked = pick_nodes(&s.nodes, &provider, &feedback);
                if picked.is_empty() {
                    (vec![(host.clone(), upstream_sni)], page_fix, false)
                } else {
                    (
                        picked
                            .iter()
                            .map(|n| (n.ip.clone(), n.sni.clone()))
                            .collect(),
                        page_fix,
                        true,
                    )
                }
            }
        }
    };

    let fwd = rewrite_request(&raw, true);
    let mut last_err = "无可用节点";
    for (idx, (ip, sni)) in candidates.iter().enumerate() {
        match try_upstream(ip, sni, &fwd, &connector).await {
            Ok((mut upstream, head, rtt_ms)) => {
                if accelerated {
                    feedback.ok(ip, rtt_ms);
                    if idx > 0 {
                        app_log(format!(
                            "[代理] {} 切换到 {} 成功（{}ms）",
                            host, ip, rtt_ms
                        ));
                    } else {
                        app_log(format!(
                            "[代理] {} → {} (SNI {} / 实测 {}ms)",
                            host, ip, sni, rtt_ms
                        ));
                    }
                }
                let _ = relay_response(&mut upstream, &mut tls, head, page_fix).await;
                let _ = tls.shutdown().await;
                return Ok(());
            }
            Err(e) => {
                last_err = e;
                if accelerated {
                    feedback.fail(ip);
                }
            }
        }
    }

    app_log(format!("[代理] {} 上游全部失败：{}", host, last_err));
    let _ = tls
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
    Ok(())
}

/// 连一个上游节点：TCP → TLS（按 sni 严格验签）→ 发请求 → 读响应头。
/// 返回 (tls, head, 实测 RTT ms)。RTT 为建连到收到响应头的端到端耗时，作为在线反馈评分。
async fn try_upstream(
    ip: &str,
    sni: &str,
    fwd: &[u8],
    connector: &Arc<rustls::ClientConfig>,
) -> std::result::Result<(tokio_rustls::client::TlsStream<TcpStream>, String, u64), &'static str> {
    let t0 = Instant::now();
    let addr = format!("{}:443", ip);
    let tcp = timeout(Duration::from_secs(8), TcpStream::connect(&addr))
        .await
        .map_err(|_| "连接超时")?
        .map_err(|_| "连接失败")?;
    let name = ServerName::try_from(sni.to_string()).map_err(|_| "SNI 非法")?;
    let mut tls = timeout(
        Duration::from_secs(8),
        TlsConnector::from(connector.clone()).connect(name, tcp),
    )
    .await
    .map_err(|_| "握手超时")?
    .map_err(|_| "握手失败")?;
    tls.write_all(fwd).await.map_err(|_| "发送请求失败")?;
    let head = timeout(Duration::from_secs(8), read_head(&mut tls))
        .await
        .map_err(|_| "读响应头超时")?
        .map_err(|_| "读响应头失败")?;
    if !head.lines().next().unwrap_or("").starts_with("HTTP/") {
        return Err("非法响应");
    }
    let rtt = t0.elapsed().as_millis() as u64;
    Ok((tls, head, rtt))
}

/// 启动代理；停止靠 handle.stop()（accept 循环带 500ms 超时）。
pub async fn start(bind: SocketAddr, ca: Arc<CaStore>, state: SharedState) -> Result<ProxyHandle> {
    let listener = TcpListener::bind(bind).await.context("bind proxy port")?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let connector = upstream_connector();
    // 在线反馈表随代理实例存活，重启代理即清空；不落盘、不序列化给界面。
    let feedback = Arc::new(Feedback::default());

    // 日志闭包：直接写共享状态，由前端轮询/事件获取
    let log_state = state.clone();
    let app_log: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |msg: String| {
        if let Ok(mut s) = log_state.lock() {
            s.logs.push(msg);
            if s.logs.len() > 300 {
                let len = s.logs.len();
                s.logs.drain(0..len - 300);
            }
        }
    });

    tokio::spawn(async move {
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }
            let accept =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                    .await;
            let (socket, _) = match accept {
                Ok(Ok(v)) => v,
                _ => continue,
            };
            let ca = ca.clone();
            let log = app_log.clone();
            let state = state.clone();
            let connector = connector.clone();
            let feedback = feedback.clone();
            tokio::spawn(async move {
                let _ = handle_client(socket, ca, log, state, connector, feedback).await;
            });
        }
    });

    Ok(ProxyHandle { addr: bind, stop })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(ip: &str, latency: u32, status: &str) -> Node {
        Node {
            ip: ip.into(),
            provider: "akamai".into(),
            group: "test".into(),
            sni: "steamcommunity-a.akamaihd.net".into(),
            host: "steamcommunity.com".into(),
            latency_ms: latency,
            status: status.into(),
        }
    }

    #[test]
    fn measured_rtt_overrides_probe_latency() {
        let fb = Feedback::default();
        let n = node("1.1.1.1", 500, "在线");
        assert_eq!(fb.score(&n).1, 500.0); // 无实测时用探测延迟
        fb.ok("1.1.1.1", 120);
        assert_eq!(fb.score(&n).1, 120.0); // 实测 RTT 覆盖
    }

    #[test]
    fn failure_demotes_and_success_clears() {
        let fb = Feedback::default();
        let a = node("1.1.1.1", 100, "在线");
        let b = node("2.2.2.2", 200, "在线");
        let nodes = vec![a, b];
        assert_eq!(pick_nodes(&nodes, "akamai", &fb)[0].ip, "1.1.1.1");
        fb.fail("1.1.1.1"); // 最快节点失败一次 → 惩罚 3000ms，退到后面
        assert_eq!(pick_nodes(&nodes, "akamai", &fb)[0].ip, "2.2.2.2");
        fb.ok("1.1.1.1", 80); // 恢复成功 → 清零惩罚、实测更优 → 回到首位
        assert_eq!(pick_nodes(&nodes, "akamai", &fb)[0].ip, "1.1.1.1");
    }

    #[test]
    fn inactive_node_demoted_but_not_dropped() {
        let fb = Feedback::default();
        let active = node("1.1.1.1", 500, "在线");
        let candidate = node("2.2.2.2", 50, "候选");
        let nodes = vec![active, candidate];
        // 候选节点即使延迟更低也排在 active 之后
        assert_eq!(pick_nodes(&nodes, "akamai", &fb)[0].ip, "1.1.1.1");
    }
}

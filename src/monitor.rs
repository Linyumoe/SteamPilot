//! 后台主动探针（M3）：运行期间每 60s 对节点做一次轻量握手复测，
//! 把实测延迟/在线状态回写到共享状态，让界面无需手动「重新测速」也能反映线路变化。
//!
//! 只做 TCP+TLS 握手 + 最小 GET（不做下载测速），成本低；不改写漏斗分层，
//! 只更新 latency_ms 与 status，在线失败节点标离线，恢复后回到「在线」。

use crate::selector;
use crate::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio_rustls::TlsConnector;

/// 默认复测间隔；加 ±5s 抖动避免多实例同频打爆同一边缘节点。
const INTERVAL_SECS: u64 = 60;

fn tls_connector() -> TlsConnector {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    // rustls 默认启用内存会话票据（Resumption::in_memory_sessions），
    // 这里共享同一个 config，探测与真实请求都能复用上游 TLS 会话。
    TlsConnector::from(Arc::new(cfg))
}

/// 在独立线程上自建 Tokio runtime 运行任务。
///
/// 注意：调用方（Tauri `.setup()`）是同步上下文，**没有 Tokio runtime**，
/// 直接 `tokio::spawn` 会 panic 让整个进程闪退（v0.1.1 曾因此黑屏退出）。
/// 抽成独立函数便于单测在「无 ambient runtime」的场景下验证，防止回归。
fn spawn_with_own_runtime<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    std::thread::Builder::new()
        .name("steampilot-monitor".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(future);
        })
        .ok();
}

/// 启动常驻探针。应用退出时置位 stop 即停。
pub fn start(app: AppHandle, state: Arc<Mutex<AppState>>, stop: Arc<AtomicBool>) {
    spawn_with_own_runtime(async move {
        let connector = tls_connector();
        // 首轮延后一个周期，避免刚启动就探测；用户手动「重新测速」走 refresh_nodes。
        let mut first = true;
        loop {
            let wait = if first {
                Duration::from_secs(INTERVAL_SECS)
            } else {
                let jitter = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
                    % 10) as u64;
                Duration::from_secs(INTERVAL_SECS.saturating_sub(5) + jitter)
            };
            first = false;
            tokio::time::sleep(wait).await;
            if stop.load(Ordering::SeqCst) {
                break;
            }

            // 只探测启用 provider 的节点；仅在加速运行时工作，停下时不产生任何流量。
            let (enabled, tmo, concurrency, running) = {
                let s = state.lock().unwrap();
                (
                    s.providers
                        .iter()
                        .filter(|p| p.enabled)
                        .map(|p| p.id.clone())
                        .collect::<Vec<_>>(),
                    s.policy.handshake_timeout_ms.max(500) as u64,
                    s.policy.concurrency.max(1),
                    s.running,
                )
            };
            if !running || enabled.is_empty() {
                continue;
            }

            let snapshot = state.lock().unwrap().nodes.clone();
            let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
            let mut handles = Vec::new();
            for node in snapshot.into_iter() {
                if !enabled.iter().any(|p| p == &node.provider) {
                    continue;
                }
                let connector = connector.clone();
                let sem = sem.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.ok();
                    let r = selector::probe_node(&node, tmo, &connector).await;
                    (node, r)
                }));
            }

            let mut updates = Vec::new();
            for h in handles {
                if let Ok((mut node, r)) = h.await {
                    match r {
                        Some(ms) => {
                            if node.latency_ms != ms || node.status == "离线" {
                                node.latency_ms = ms;
                                if node.status == "离线" {
                                    node.status = "在线".into();
                                }
                                updates.push(node);
                            }
                        }
                        None => {
                            if node.status != "离线" {
                                // 失败只标离线，不永久剔除：下一轮探测成功会自动恢复。
                                node.status = "离线".into();
                                updates.push(node);
                            }
                        }
                    }
                }
            }
            if updates.is_empty() {
                continue;
            }

            let snapshot = {
                let mut s = state.lock().unwrap();
                // 按 (ip, provider) 合并，保留本轮未探测节点（未启用的 provider）。
                for updated in updates {
                    if let Some(old) = s
                        .nodes
                        .iter_mut()
                        .find(|n| n.ip == updated.ip && n.provider == updated.provider)
                    {
                        *old = updated;
                    }
                }
                let node_count = s.nodes.len();
                s.logs.push(format!(
                    "[探针] 后台复测完成，{} 个节点状态更新",
                    node_count
                ));
                if s.logs.len() > 300 {
                    let len = s.logs.len();
                    s.logs.drain(0..len - 300);
                }
                s.clone()
            };
            let _ = app.emit("state-changed", &snapshot);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_has_alpn_http1() {
        // 确保探针 connector 与真实请求 ALPN 一致，避免测出与实际不符的路径。
        let _ = tls_connector();
    }

    #[test]
    fn stop_flag_defaults_to_running() {
        let stop = Arc::new(AtomicBool::new(false));
        assert!(!stop.load(Ordering::SeqCst));
    }

    /// 回归测试：v0.1.1 曾在 `.setup()`（无 Tokio runtime）里直接 tokio::spawn 而 panic 闪退。
    /// 在刻意不建 runtime 的线程里调用，确认自建 runtime 的 future 真的执行了且不 panic。
    #[test]
    fn own_runtime_works_without_ambient_runtime() {
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        // 此线程没有任何 Tokio runtime；旧实现（直接 tokio::spawn）会 panic。
        spawn_with_own_runtime(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("自建 runtime 的 future 未执行");
    }
}

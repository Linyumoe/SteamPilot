//! 长稳测试（M3/测试项）：周期性经本地代理请求多个站点，统计成功率与延迟漂移。
//!
//! 用法：
//!   cargo run --release --example soak                 # 默认 60 秒冒烟
//!   cargo run --release --example soak -- --hours 24    # 24 小时长稳
//!   cargo run --release --example soak -- --seconds 300 # 自定义 5 分钟
//!
//! 本进程自带代理与 CA（测试模式，curl 用 -k 忽略证书），不装系统根证书、
//! 不改系统代理。检查项：请求成功率、错误分布、是否出现卡死/连接泄漏。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use steampilot_lib::{ca::CaStore, proxy, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let duration = if let Some(i) = args.iter().position(|a| a == "--hours") {
        Duration::from_secs(args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(24) * 3600)
    } else if let Some(i) = args.iter().position(|a| a == "--seconds") {
        Duration::from_secs(args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(60))
    } else {
        Duration::from_secs(60)
    };

    // 目标：覆盖加速站点与非白名单直连；只用 HEAD 避免大流量长时间跑。
    let targets = [
        "https://steamcommunity.com/",
        "https://store.steampowered.com/",
        "https://github.com/",
        "https://www.baidu.com/",
    ];

    let ca = Arc::new(CaStore::load_or_create()?);
    let state = Arc::new(Mutex::new(AppState::default()));
    state.lock().unwrap().ca_installed = true;
    let addr: SocketAddr = "127.0.0.1:7890".parse()?;
    let handle = proxy::start(addr, ca, state.clone()).await?;
    println!(
        "[soak] 代理 {} 就绪，运行 {:?}，每轮 {} 个目标，Ctrl+C 提前结束",
        handle.addr,
        duration,
        targets.len()
    );

    let started = Instant::now();
    let mut rounds = 0u64;
    let mut ok = 0u64;
    let mut fail = 0u64;
    let mut max_round = Duration::ZERO;

    while started.elapsed() < duration {
        let t0 = Instant::now();
        for url in targets {
            // curl -k -I 经本地代理；失败不中断，计失败数。
            // Windows 的 curl 不认 /dev/null，用 NUL（Unix 用 /dev/null）。
            #[cfg(windows)]
            let null_dev = "NUL";
            #[cfg(not(windows))]
            let null_dev = "/dev/null";
            let out = tokio::process::Command::new("curl")
                .args([
                    "-k",
                    "-sS",
                    "-m",
                    "20",
                    "-x",
                    "http://127.0.0.1:7890",
                    "-I",
                    "-o",
                    null_dev,
                    "-w",
                    "%{http_code}",
                    url,
                ])
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    let code = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if code.starts_with('2') || code.starts_with('3') {
                        ok += 1;
                    } else {
                        fail += 1;
                        eprintln!("[soak] {} -> {}", url, code);
                    }
                }
                _ => {
                    fail += 1;
                    eprintln!("[soak] {} -> 请求失败", url);
                }
            }
        }
        rounds += 1;
        let elapsed = t0.elapsed();
        if elapsed > max_round {
            max_round = elapsed;
        }
        // 每 10 轮打印一次进度；30s 一轮节奏，避免长期高频触发 CDN 限流。
        if rounds % 10 == 0 {
            println!(
                "[soak] 第 {} 轮 | 成功 {} 失败 {} | 已运行 {:?} | 最慢一轮 {:?}",
                rounds,
                ok,
                fail,
                started.elapsed(),
                max_round
            );
        }
        tokio::time::sleep(Duration::from_secs(30).saturating_sub(elapsed)).await;
    }

    // 结束前读一次状态快照，确认节点/日志规模没有失控（连接泄漏的间接信号）。
    let (nodes, logs) = {
        let s = state.lock().unwrap();
        (s.nodes.len(), s.logs.len())
    };
    let total = ok + fail;
    println!(
        "[soak] 完成 | {} 轮 / {} 请求 | 成功率 {:.1}% | 节点 {} | 日志 {}（上限 300）",
        rounds,
        total,
        if total == 0 {
            0.0
        } else {
            ok as f64 / total as f64 * 100.0
        },
        nodes,
        logs
    );
    if logs > 300 {
        anyhow::bail!("日志超过 300 条上限，存在日志泄漏");
    }
    handle.stop();
    Ok(())
}

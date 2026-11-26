//! 验证漏斗第三阶段的真实下载测速：用已知可用的 Akamai 节点跑一遍 refresh。
//! 运行：cargo run --release --example selector_smoke

use std::collections::HashMap;
use steampilot_lib::{Node, Policy};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let nodes = vec![
        Node {
            ip: "203.80.97.112".into(),
            provider: "akamai".into(),
            group: "test".into(),
            sni: "steamcommunity-a.akamaihd.net".into(),
            host: "steamcommunity.com".into(),
            latency_ms: 0,
            status: "稳定".into(),
        },
        Node {
            ip: "23.220.70.51".into(),
            provider: "akamai".into(),
            group: "test".into(),
            sni: "steamcommunity-a.akamaihd.net".into(),
            host: "steamcommunity.com".into(),
            latency_ms: 0,
            status: "稳定".into(),
        },
    ];

    let policy = Policy {
        stage1_keep: 8,
        stage2_keep: 4,
        stage3_keep: 2,
        concurrency: 8,
        handshake_timeout_ms: 4000,
        sample_bytes: 4, // 取样 4MB，够算吞吐又不至于太久
        alias: "auto".into(),
        reuse_recent: false,
        randomize_tail: false,
        batch_size: 1,
        min_speed_mbps: 10,
        total_limit_mbps: 1000,
        speed_test_urls: {
            let mut m = HashMap::new();
            m.insert(
                "akamai".into(),
                "https://origin-a.akamaihd.net/EA-Desktop-Client-Download/installer-releases/EA%20app.dmg".into(),
            );
            m
        },
    };

    let (updated, stages) = selector_refresh(nodes, &["akamai".into()], &policy).await;
    println!("漏斗阶段存活数: {:?}", stages);
    for n in updated {
        println!("{} {}ms {} {}", n.ip, n.latency_ms, n.status, n.group);
    }
    Ok(())
}

// 直接复用库里的 selector（通过公开重导出的 refresh）。
async fn selector_refresh(
    nodes: Vec<Node>,
    providers: &[String],
    policy: &Policy,
) -> (Vec<Node>, [usize; 3]) {
    steampilot_lib::selector::refresh(nodes, providers, policy).await
}

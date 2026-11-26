//! 失败切换冒烟测试：在 Akamai 池首位插入一个连不通的坏节点（延迟设为最小以保证被选中），
//! 启动代理后用 curl 请求 Steam 社区，验证代理能自动跳过坏节点、用后面的真实节点返回 200。
//! 用法：cargo run --release --example failover_smoke，然后另开终端：
//!   curl -k -x http://127.0.0.1:7890 https://steamcommunity.com/ -o /dev/null -w "%{http_code}"

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use steampilot_lib::ca::CaStore;
use steampilot_lib::proxy;
use steampilot_lib::{AppState, Node};

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = Arc::new(CaStore::load_or_create().expect("init CA"));
    let mut state = AppState::default();

    // 坏节点：公网保留地址，连不通；latency=0 保证评分排在最前，一定先被尝试。
    let bad = Node {
        ip: "198.51.100.1".into(), // TEST-NET-2，不可路由
        provider: "akamai".into(),
        group: "故意放坏的测试节点".into(),
        sni: "steamcommunity-a.akamaihd.net".into(),
        host: "steamcommunity.com".into(),
        latency_ms: 0,
        status: "在线".into(),
    };
    state.nodes.insert(0, bad);
    state.ca_installed = true;
    state
        .logs
        .push("[failover] 坏节点已插入首位，测试自动切换".into());

    let state = Arc::new(Mutex::new(state));
    let addr: SocketAddr = "127.0.0.1:7890".parse().unwrap();
    let handle = proxy::start(addr, ca, state.clone())
        .await
        .expect("start proxy");
    println!(
        "proxy listening on {} — 坏节点在前，curl 应仍返回 200",
        handle.addr
    );

    tokio::signal::ctrl_c().await.ok();
    handle.stop();
}

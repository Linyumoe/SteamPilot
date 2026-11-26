//! 端到端冒烟测试：不装系统根证书，直接启动代理，用 curl -k 验证转发链路。
//! 用法：cargo run --example proxy_smoke，然后另开终端：
//!   curl -k -x http://127.0.0.1:7890 https://steamcommunity.com/ -o /dev/null -w "%{http_code}"

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use steampilot_lib::{ca::CaStore, proxy, AppState};

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = Arc::new(CaStore::load_or_create().expect("init CA"));
    let state = Arc::new(Mutex::new(AppState::default()));
    let addr: SocketAddr = "127.0.0.1:7890".parse().unwrap();

    // 标记 CA 已装（冒烟测试用 -k 忽略证书，不真的装）
    state.lock().unwrap().ca_installed = true;
    state
        .lock()
        .unwrap()
        .logs
        .push("[smoke] 代理启动（测试模式，curl 用 -k）".into());

    let handle = proxy::start(addr, ca, state.clone())
        .await
        .expect("start proxy");
    println!("proxy listening on {} — Ctrl+C to stop", handle.addr);

    tokio::signal::ctrl_c().await.ok();
    handle.stop();
}

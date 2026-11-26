//! 验证自更新检查：直接调用 update::check，打印解析结果。
//! 运行：cargo run --release --example update_smoke

#[tokio::main]
async fn main() {
    let info = steampilot_lib::update::check().await;
    println!("current : {}", info.current);
    println!("latest  : {}", info.latest);
    println!("available: {}", info.available);
    println!("url     : {}", info.url);
    println!("notes   : {}", info.notes);
    println!("error   : {}", info.error);
}

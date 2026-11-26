//! 系统代理 + PAC 接管冒烟测试（只写当前用户设置，退出前一定恢复）。
//!
//! 两种模式：
//!   capture_smoke.exe           正常：apply → restore，验证可恢复基线。
//!   capture_smoke.exe crash     apply 后不 restore 直接退出，模拟程序被强杀后
//!                              注册表残留自身 PAC；下一次正常运行应能擦掉它。
//!
//! 运行：cargo run --release --example capture_smoke [-- crash]
use steampilot_lib::capture::{apply, restore, show_status};
use steampilot_lib::AppState;

fn main() {
    let state = AppState::default();
    let crash = std::env::args().any(|a| a == "crash");

    if crash {
        println!("== [崩溃模拟] apply 后不恢复，直接退出（注册表将残留自身 PAC）==");
        apply(&state).expect("apply failed");
        show_status();
        // 故意不调用 restore()：模拟进程被强杀，内存中的 SAVED 原配置丢失。
        return;
    }

    println!("== [正常] apply → restore ==");
    apply(&state).expect("apply failed");
    show_status();
    restore().expect("restore failed");
    println!("== 已恢复 ==");
    show_status();
}

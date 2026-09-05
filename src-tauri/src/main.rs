// Release 构建按 Windows GUI 程序编译，避免启动时闪现控制台窗口；debug 保留控制台便于查看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    dsh_desktop_lib::run();
    std::process::exit(0);
}

// UDP2CAL Windows 接收端 - 局域网音频串流
// 双状态机架构 + P2P 独占通信系统
#![windows_subsystem = "windows"]

mod app;
mod audio;
mod capture;
mod config;
mod decoder;
mod diag;
mod firewall;
mod hires;
mod i18n;
mod logger;
mod net;
mod power;
mod protocol;
mod session;
mod tray;
mod ui;

fn main() -> Result<(), iced::Error> {
    app::run()
}

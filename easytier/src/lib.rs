#![allow(dead_code)]

// 架构模块
mod arch;
/// 网关模块
mod gateway;
/// 实例模块
mod instance;
/// 对等中心模块
mod peer_center;
/// VPN 门户模块
mod vpn_portal;

// 公共模块
pub mod common;
/// 连接器模块
pub mod connector;
/// 启动器模块
pub mod launcher;
/// 对等模块
pub mod peers;
/// 协议模块
pub mod proto;
/// 隧道模块
pub mod tunnel;
/// 工具模块
pub mod utils;
/// Web 客户端模块
pub mod web_client;

#[cfg(test)]
mod tests;

// EasyTier 版本常量
pub const VERSION: &str = common::constants::EASYTIER_VERSION;
rust_i18n::i18n!("locales", fallback = "zh-CN");

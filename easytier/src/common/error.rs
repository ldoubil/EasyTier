use std::{io, result};
use thiserror::Error;

use easytier_core::tunnel::TunnelError;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io error")]
    IOError(#[from] io::Error),

    #[cfg(feature = "tun")]
    #[error("rust tun error {0}")]
    TunError(#[from] tun::Error),

    #[error("tunnel error {0}")]
    TunnelError(#[from] TunnelError),
    #[error("Not found")]
    NotFound,
    #[error("Invalid Url: {0}")]
    InvalidUrl(String),
    #[error("Shell Command error: {0}")]
    ShellCommandError(String),
    #[error("Timeout error: {0}")]
    Timeout(#[from] tokio::time::error::Elapsed),
    /// Windows 网卡在系统里可见，但 IP 层还没准备好接受地址配置。
    /// 调用方可以据此决定“重建一次 NIC”，而不是把它当成普通的 IO 失败。
    #[error(
        "interface `{ifname}` not ready for {operation}: {attempts} attempt(s) in {elapsed_ms} ms, last error: {last_error}"
    )]
    InterfaceNotReady {
        ifname: String,
        operation: &'static str,
        attempts: u32,
        elapsed_ms: u64,
        last_error: String,
    },
    #[error("anyhow error: {0}")]
    AnyhowError(#[from] anyhow::Error),
}

pub type Result<T> = result::Result<T, Error>;

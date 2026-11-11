use crate::common::ifcfg::win::types::{RouteDataIpv4, RouteDataIpv6};

use super::win::luid::InterfaceLuid;
use async_trait::async_trait;
use cidr::{Ipv4Inet, Ipv6Inet};
use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr},
    time::{Duration, Instant},
};
use windows::Win32::NetworkManagement::IpHelper::INTERNAL_IF_OPER_STATUS;
use windows::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND, NO_ERROR},
    NetworkManagement::IpHelper::{GetIfEntry, MIB_IFROW, SetIfEntry},
    System::Diagnostics::Debug::{
        FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
    },
};
use windows::core::PWSTR;
use winreg::{
    RegKey,
    enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE},
};

use super::{Error, IfConfiguerTrait};
#[derive(Default)]
pub struct WindowsIfConfiger {}

fn ignore_removed_not_found(result: Result<(), u32>) -> Result<(), u32> {
    match result {
        Err(error) if error == ERROR_NOT_FOUND.0 => Ok(()),
        other => other,
    }
}

fn format_win_error(error: u32) -> String {
    // use FormatMessageW to get the error message
    let mut buffer = vec![0; 1024];
    let size = buffer.len() as u32;
    let flags = FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS;

    unsafe {
        FormatMessageW(
            flags,
            None,
            error,
            0,
            PWSTR(buffer.as_mut_ptr()),
            size,
            None,
        );
    }
    let str_end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    format!(
        "{} (code: {})",
        String::from_utf16_lossy(&buffer[..str_end]).trim(),
        error
    )
}

/// Windows 新建 Wintun 适配器后，接口进入 IPv4 协议栈需要一小段时间；
/// 这期间 CreateUnicastIpAddressEntry 会返回 87/1168。150ms 间隔、4s 预算
/// 覆盖现场日志观察到的最长延迟，同时不会让连接流程明显变慢。
const IP_CONFIG_RETRY_INTERVAL: Duration = Duration::from_millis(150);
const IP_CONFIG_RETRY_BUDGET: Duration = Duration::from_millis(4000);

/// 名字出现后，等待 IPv4 IP 层接口就绪的上限。超时只告警不失败，
/// 交给 add_ip_address 的重试兜底，避免 IPv6-only 场景被误判。
const INTERFACE_READY_BUDGET: Duration = Duration::from_millis(5000);

fn is_retryable_ifcfg_error(code: Option<u32>) -> bool {
    match code {
        // name -> index 还没查到，说明适配器刚创建、还没注册完
        None => true,
        // 87  = ERROR_INVALID_PARAMETER：接口存在但还不能接收地址
        // 1168 = ERROR_NOT_FOUND：接口还没进入 IP 层
        Some(code) => code == ERROR_INVALID_PARAMETER.0 || code == ERROR_NOT_FOUND.0,
    }
}

impl WindowsIfConfiger {
    pub fn get_interface_index(name: &str) -> Option<u32> {
        crate::arch::windows::find_interface_index(name).ok()
    }

    #[tracing::instrument(err, ret)]
    async fn add_ip_address(name: &str, addr: Ipv4Inet) -> Result<(), Error> {
        let started = Instant::now();
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            // 每一轮都必须重新做 name -> if_index -> LUID 的解析。
            // 适配器在创建/重建窗口内可能换掉 LUID，缓存第一次的 LUID 再重试
            // 只会一直拿到同一个错误（本机实验：旧 LUID 等 1s 仍是 1168）。
            match Self::try_add_ip_address(name, &addr) {
                Ok(()) => {
                    if attempts > 1 {
                        tracing::info!(
                            ?name,
                            attempts,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "IPv4 address added after retry"
                        );
                    }
                    return Ok(());
                }
                Err((code, last_error)) => {
                    let elapsed = started.elapsed();
                    if !is_retryable_ifcfg_error(code) || elapsed >= IP_CONFIG_RETRY_BUDGET {
                        return Err(Error::InterfaceNotReady {
                            ifname: name.to_string(),
                            operation: "add_ipv4_address",
                            attempts,
                            elapsed_ms: elapsed.as_millis() as u64,
                            last_error,
                        });
                    }
                    tracing::warn!(
                        ?name,
                        attempts,
                        elapsed_ms = elapsed.as_millis() as u64,
                        %last_error,
                        "interface not ready for IPv4 address, retrying"
                    );
                    tokio::time::sleep(IP_CONFIG_RETRY_INTERVAL).await;
                }
            }
        }
    }

    fn try_add_ip_address(name: &str, addr: &Ipv4Inet) -> Result<(), (Option<u32>, String)> {
        let if_index = Self::get_interface_index(name)
            .ok_or_else(|| (None, "interface not found".to_string()))?;
        let luid =
            InterfaceLuid::luid_from_index(if_index).map_err(|e| (Some(e), format_win_error(e)))?;
        luid.add_ipv4_address(addr)
            .map_err(|e| (Some(e), format_win_error(e)))
    }

    #[tracing::instrument(err, ret)]
    async fn remove_ip_address(name: &str, addr: Option<Ipv4Inet>) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };
        let luid = InterfaceLuid::luid_from_index(if_index).map_err(|e| {
            anyhow::anyhow!("Failed to get interface luid: {}", format_win_error(e))
        })?;
        if let Some(addr) = addr {
            ignore_removed_not_found(luid.delete_ipv4_address(&addr)).map_err(|e| {
                anyhow::anyhow!("Failed to delete IPv4 address: {}", format_win_error(e))
            })?;
        } else {
            luid.flush_ipv4_addresses().map_err(|e| {
                anyhow::anyhow!("Failed to flush IPv4 addresses: {}", format_win_error(e))
            })?;
        }
        Ok(())
    }

    #[tracing::instrument(err, ret)]
    async fn set_interface_status(name: &str, up: bool) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };

        unsafe {
            let mut if_row = MIB_IFROW {
                wszName: [0; 256],
                dwIndex: if_index,
                dwType: 0,
                dwMtu: 0,
                dwSpeed: 0,
                dwPhysAddrLen: 0,
                bPhysAddr: [0; 8],
                dwAdminStatus: if up { 1 } else { 2 }, // 1 = up, 2 = down
                dwOperStatus: INTERNAL_IF_OPER_STATUS(0),
                dwLastChange: 0,
                dwInOctets: 0,
                dwInUcastPkts: 0,
                dwInNUcastPkts: 0,
                dwInDiscards: 0,
                dwInErrors: 0,
                dwInUnknownProtos: 0,
                dwOutOctets: 0,
                dwOutUcastPkts: 0,
                dwOutNUcastPkts: 0,
                dwOutDiscards: 0,
                dwOutErrors: 0,
                dwOutQLen: 0,
                dwDescrLen: 0,
                bDescr: [0; 256],
            };

            if GetIfEntry(&mut if_row) == NO_ERROR.0 {
                if SetIfEntry(&if_row) == NO_ERROR.0 {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("Failed to set interface status").into())
                }
            } else {
                Err(anyhow::anyhow!("Failed to get interface entry").into())
            }
        }
    }

    #[tracing::instrument(err, ret)]
    async fn set_interface_mtu(name: &str, mtu: u32) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };
        let luid = InterfaceLuid::luid_from_index(if_index).map_err(|e| {
            anyhow::anyhow!("Failed to get interface luid: {}", format_win_error(e))
        })?;
        luid.set_iface_config(mtu).map_err(|e| {
            anyhow::anyhow!("Failed to set interface config: {}", format_win_error(e))
        })?;
        Ok(())
    }

    #[tracing::instrument(err, ret)]
    async fn add_ipv6_address(name: &str, addr: Ipv6Inet) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };
        let luid = InterfaceLuid::luid_from_index(if_index).map_err(|e| {
            anyhow::anyhow!("Failed to get interface luid: {}", format_win_error(e))
        })?;
        luid.add_ipv6_address(&addr)
            .map_err(|e| anyhow::anyhow!("Failed to add IPv6 address: {}", format_win_error(e)))?;
        Ok(())
    }

    #[tracing::instrument(err, ret)]
    async fn remove_ipv6_address(name: &str, addr: Option<Ipv6Inet>) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };
        let luid = InterfaceLuid::luid_from_index(if_index).map_err(|e| {
            anyhow::anyhow!("Failed to get interface luid: {}", format_win_error(e))
        })?;
        if let Some(addr) = addr {
            ignore_removed_not_found(luid.delete_ipv6_address(&addr)).map_err(|e| {
                anyhow::anyhow!("Failed to delete IPv6 address: {}", format_win_error(e))
            })?;
        } else {
            luid.flush_ipv6_addresses().map_err(|e| {
                anyhow::anyhow!("Failed to flush IPv6 addresses: {}", format_win_error(e))
            })?;
        }
        Ok(())
    }
}

#[async_trait]
impl IfConfiguerTrait for WindowsIfConfiger {
    async fn add_ipv4_route(
        &self,
        name: &str,
        address: Ipv4Addr,
        cidr_prefix: u8,
        cost: Option<i32>,
    ) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };
        let luid = InterfaceLuid::luid_from_index(if_index).map_err(|e| {
            anyhow::anyhow!("Failed to get interface luid: {}", format_win_error(e))
        })?;

        luid.add_routes_ipv4([RouteDataIpv4 {
            destination: Ipv4Inet::new(address, cidr_prefix).unwrap(),
            next_hop: Ipv4Addr::UNSPECIFIED,
            metric: cost.unwrap_or(9000) as u32,
        }])
        .map_err(|e| anyhow::anyhow!("Failed to add route: {}", format_win_error(e)))?;
        Ok(())
    }

    async fn remove_ipv4_route(
        &self,
        name: &str,
        address: Ipv4Addr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };
        let luid = InterfaceLuid::luid_from_index(if_index).map_err(|e| {
            anyhow::anyhow!("Failed to get interface luid: {}", format_win_error(e))
        })?;
        ignore_removed_not_found(luid.delete_route_ipv4(
            &Ipv4Inet::new(address, cidr_prefix).unwrap(),
            &Ipv4Addr::UNSPECIFIED,
        ))
        .map_err(|e| anyhow::anyhow!("Failed to delete route: {}", format_win_error(e)))?;
        Ok(())
    }

    async fn add_ipv4_ip(
        &self,
        name: &str,
        address: Ipv4Addr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        Self::add_ip_address(name, Ipv4Inet::new(address, cidr_prefix).unwrap()).await
    }

    async fn set_link_status(&self, name: &str, up: bool) -> Result<(), Error> {
        Self::set_interface_status(name, up).await
    }

    async fn remove_ip(&self, name: &str, ip: Option<Ipv4Inet>) -> Result<(), Error> {
        Self::remove_ip_address(name, ip).await
    }

    async fn wait_interface_show(&self, name: &str) -> Result<(), Error> {
        // 第一步：等名字出现。这里保持无超时，避免开机/TUN 创建慢时误判。
        let if_index = loop {
            if let Some(idx) = Self::get_interface_index(name) {
                tracing::info!(?name, ?idx, "Interface found");
                break idx;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        // 第二步：等 IPv4 IP 层接口就绪，最多 5s。
        // 唯一就绪判据是 GetIpInterfaceEntry(AF_INET)：它和失败的
        // SetIpInterfaceEntry / CreateUnicastIpAddressEntry 查同一张表；
        // 而 GetIfEntry/SetIfEntry（link_up）在故障现场是成功的，不能作为判据。
        let deadline = Instant::now() + INTERFACE_READY_BUDGET;
        loop {
            let ready = Self::get_interface_index(name)
                .and_then(|idx| InterfaceLuid::luid_from_index(idx).ok())
                .map(|luid| luid.is_ipv4_interface_ready())
                .unwrap_or(false);
            if ready {
                tracing::info!(?name, ?if_index, "Interface IPv4 stack ready");
                return Ok(());
            }
            if Instant::now() >= deadline {
                // IPv6-only 或被安全软件拖慢时不在这里失败，
                // 由 add_ip_address 的 4s 重试和外层 NIC 重建兜底。
                tracing::warn!(
                    ?name,
                    ?if_index,
                    elapsed_ms = INTERFACE_READY_BUDGET.as_millis() as u64,
                    "IPv4 stack not ready after wait, continue and let address assignment retry"
                );
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn set_mtu(&self, name: &str, mtu: u32) -> Result<(), Error> {
        Self::set_interface_mtu(name, mtu).await
    }

    async fn add_ipv6_ip(
        &self,
        name: &str,
        address: Ipv6Addr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        Self::add_ipv6_address(name, Ipv6Inet::new(address, cidr_prefix).unwrap()).await
    }

    async fn remove_ipv6(&self, name: &str, ip: Option<Ipv6Inet>) -> Result<(), Error> {
        Self::remove_ipv6_address(name, ip).await
    }

    async fn add_ipv6_route(
        &self,
        name: &str,
        address: Ipv6Addr,
        cidr_prefix: u8,
        cost: Option<i32>,
    ) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };

        let luid = InterfaceLuid::luid_from_index(if_index).map_err(|e| {
            anyhow::anyhow!("Failed to get interface luid: {}", format_win_error(e))
        })?;

        luid.add_routes_ipv6([RouteDataIpv6 {
            destination: Ipv6Inet::new(address, cidr_prefix).unwrap(),
            next_hop: Ipv6Addr::UNSPECIFIED,
            metric: cost.unwrap_or(9000) as u32,
        }])
        .map_err(|e| anyhow::anyhow!("Failed to add route: {}", format_win_error(e)))?;
        Ok(())
    }

    async fn remove_ipv6_route(
        &self,
        name: &str,
        address: Ipv6Addr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        let Some(if_index) = Self::get_interface_index(name) else {
            return Err(Error::NotFound);
        };

        let luid = InterfaceLuid::luid_from_index(if_index).map_err(|e| {
            anyhow::anyhow!("Failed to get interface luid: {}", format_win_error(e))
        })?;

        ignore_removed_not_found(luid.delete_route_ipv6(
            &Ipv6Inet::new(address, cidr_prefix).unwrap(),
            &Ipv6Addr::UNSPECIFIED,
        ))
        .map_err(|e| anyhow::anyhow!("Failed to delete route: {}", format_win_error(e)))?;
        Ok(())
    }
}

pub struct RegistryManager;

impl RegistryManager {
    pub const IPV4_TCPIP_INTERFACE_PREFIX: &str =
        r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\";
    pub const IPV6_TCPIP_INTERFACE_PREFIX: &str =
        r"SYSTEM\CurrentControlSet\Services\Tcpip6\Parameters\Interfaces\";
    pub const NETBT_INTERFACE_PREFIX: &str =
        r"SYSTEM\CurrentControlSet\Services\NetBT\Parameters\Interfaces\Tcpip_";

    pub fn reg_change_catrgory_in_profile(dev_name: &str) -> io::Result<()> {
        use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE, enums::KEY_ALL_ACCESS};
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let profiles_key = hklm.open_subkey_with_flags(
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\NetworkList\\Profiles",
            KEY_ALL_ACCESS,
        )?;

        for subkey_name in profiles_key.enum_keys().filter_map(Result::ok) {
            let subkey = profiles_key.open_subkey_with_flags(&subkey_name, KEY_ALL_ACCESS)?;
            match subkey.get_value::<String, _>("ProfileName") {
                Ok(profile_name) => {
                    if !dev_name.is_empty() && dev_name == profile_name {
                        match subkey.set_value("Category", &1u32) {
                            Ok(_) => tracing::trace!("Successfully set Category in registry"),
                            Err(e) => tracing::error!("Failed to set Category in registry: {}", e),
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to read ProfileName for subkey {}: {}",
                        subkey_name,
                        e
                    );
                }
            }
        }
        Ok(())
    }

    // 根据接口名称查找 GUID
    pub fn find_interface_guid(interface_name: &str) -> io::Result<String> {
        // 注册表路径：所有网络接口的根目录
        let network_key_path =
            r"SYSTEM\CurrentControlSet\Control\Network\{4D36E972-E325-11CE-BFC1-08002BE10318}";

        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let network_key = hklm.open_subkey_with_flags(network_key_path, KEY_READ)?;

        // 遍历该路径下的所有 GUID 子键
        for guid in network_key.enum_keys().map_while(Result::ok) {
            if let Ok(guid_key) = network_key.open_subkey_with_flags(&guid, KEY_READ) {
                // 检查 Connection/Name 是否匹配目标接口名
                if let Ok(conn_key) = guid_key.open_subkey_with_flags("Connection", KEY_READ)
                    && let Ok(name) = conn_key.get_value::<String, _>("Name")
                    && name == interface_name
                {
                    return Ok(guid);
                }
            }
        }

        // 如果没有找到对应的接口
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Interface not found",
        ))
    }

    // 打开注册表键
    pub fn open_interface_key(interface_guid: &str, prefix: &str) -> io::Result<RegKey> {
        let path = format!(r"{}{}", prefix, interface_guid);
        let hkey_local_machine = RegKey::predef(HKEY_LOCAL_MACHINE);
        hkey_local_machine.open_subkey_with_flags(&path, KEY_WRITE)
    }

    // 禁用动态 DNS 更新
    // disableDynamicUpdates sets the appropriate registry values to prevent the
    // Windows DHCP client from sending dynamic DNS updates for our interface to
    // AD domain controllers.
    pub fn disable_dynamic_updates(interface_guid: &str) -> io::Result<()> {
        let prefixes = [
            Self::IPV4_TCPIP_INTERFACE_PREFIX,
            Self::IPV6_TCPIP_INTERFACE_PREFIX,
        ];

        for prefix in &prefixes {
            let key = match Self::open_interface_key(interface_guid, prefix) {
                Ok(k) => k,
                Err(e) => {
                    // 模拟 mute-key-not-found-if-closing 行为
                    if matches!(e.kind(), io::ErrorKind::NotFound) {
                        continue;
                    } else {
                        return Err(e);
                    }
                }
            };

            key.set_value("RegistrationEnabled", &0u32)?;
            key.set_value("DisableDynamicUpdate", &1u32)?;
            key.set_value("MaxNumberOfAddressesToRegister", &0u32)?;
        }

        Ok(())
    }

    // 设置单个 DWORD 值到指定的注册表路径下
    fn set_single_dword(
        interface_guid: &str,
        prefix: &str,
        value_name: &str,
        data: u32,
    ) -> io::Result<()> {
        let key = match Self::open_interface_key(interface_guid, prefix) {
            Ok(k) => k,
            Err(e) => {
                // 模拟 muteKeyNotFoundIfClosing 行为：忽略 Key Not Found 错误
                return if matches!(e.kind(), io::ErrorKind::NotFound) {
                    Ok(())
                } else {
                    Err(e)
                };
            }
        };

        key.set_value(value_name, &data)?;
        Ok(())
    }

    // 禁用 NetBIOS 名称解析请求
    pub fn disable_netbios(interface_guid: &str) -> io::Result<()> {
        Self::set_single_dword(
            interface_guid,
            Self::NETBT_INTERFACE_PREFIX,
            "NetbiosOptions",
            2,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ERROR_NOT_FOUND, ignore_removed_not_found};
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
    };

    #[test]
    fn removed_windows_ifcfg_ignores_only_not_found() {
        assert_eq!(ignore_removed_not_found(Ok(())), Ok(()));
        assert_eq!(ignore_removed_not_found(Err(ERROR_NOT_FOUND.0)), Ok(()));
        for error in [
            ERROR_ACCESS_DENIED.0,
            ERROR_INVALID_PARAMETER.0,
            ERROR_FILE_NOT_FOUND.0,
        ] {
            assert_eq!(ignore_removed_not_found(Err(error)), Err(error));
        }
    }
}

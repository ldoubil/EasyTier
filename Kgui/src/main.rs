use std::collections::BTreeMap;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use easytier::{common::config::{ConfigLoader, gen_default_flags, FileLoggerConfig, TomlConfigLoader}, launcher::{NetworkConfig, NetworkInstance, NetworkInstanceRunningInfo}, proto, utils::{self, NewFilterSender}, VERSION};
use easytier::common::config::{NetworkIdentity, PeerConfig};
use easytier::common::global_ctx::{EventBusSubscriber, GlobalCtxEvent};
use easytier::common::scoped_task::ScopedTask;

static INSTANCE_MAP: Lazy<DashMap<String, NetworkInstance>> =
    Lazy::new(DashMap::new);





async fn create_and_store_network_instance(cfg: TomlConfigLoader, name:String) -> Result<(), String> {
    println!("Starting easytier with config:");
    println!("############### TOML ###############\n");
    println!("{}", cfg.dump());
    println!("-----------------------------------");
    // 创建网络实例
    let mut instance = NetworkInstance::new(cfg).set_fetch_node_info(false);
    // 启动网络实例，并处理可能的错误
    let _t = ScopedTask::from(handle_event(instance.start().unwrap()));
    // 打印实例启动信息
    if let Some(e) = instance.wait().await {
        println!("launcher error: {}", e);
        return Err(e.to_string());
    }

    // 将实例存储到 INSTANCE_MAP 中
    INSTANCE_MAP.insert(name,instance);

    Ok(())
}


fn create_config() -> TomlConfigLoader {
    let mut cfg = TomlConfigLoader::default();
    // 构造 PeerConfig 实例并设置 peers
    let peer_config = PeerConfig {
        uri: "tcp://public.easytier.net:11010".to_string().parse().unwrap()
    };
    let peer_config2 = PeerConfig {
        uri: "udp://public.easytier.net:11010".to_string().parse().unwrap()
    };
    cfg.set_hostname(Option::from("开开开问问".to_string()));
    cfg.set_peers(vec![peer_config, peer_config2]);
    cfg.set_listeners(vec![
        "tcp://0.0.0.0:11010".to_string().parse().unwrap(),
        "udp://0.0.0.0:11010".to_string().parse().unwrap(),
    ]);
    cfg.set_inst_name("default".to_string());
    cfg.set_dhcp(true);
    cfg.set_network_identity(NetworkIdentity::new(
        "test".to_string(),
        "test".to_string(),
    ));
    cfg
}

#[tokio::main]
async fn main() {
    // 创建一个示例配置
    let cfg = create_config();
    // 并行启动网络实例
    let handle1 = tokio::spawn(async move {
        create_and_store_network_instance(cfg, "aaa".to_string()).await.expect("TODO: panic message");
    });

    // 等待所有任务完成
    tokio::join!(handle1);
}



fn easytier_version() -> Result<String, String> {
    Ok(VERSION.to_string())
}

/// 函数名: collect_network_infos
/// # 描述: 收集所有网络实例的运行信息，并返回一个按实例名称排序的有序映射
/// # 参数: 无
/// # 返回值:
///   Ok(BTreeMap<String, NetworkInstanceRunningInfo>): 成功时返回包含实例名称和对应运行信息的有序映射
///   Err(String): 发生错误时返回错误描述字符串
fn collect_network_infos() -> Result<BTreeMap<String, NetworkInstanceRunningInfo>, String> {
    let mut ret = BTreeMap::new();
    // 遍历实例映射并收集运行中的实例信息
    for instance in INSTANCE_MAP.iter() {
        if let Some(info) = instance.get_running_info() {
            ret.insert(instance.key().clone(), info);
        }
    }
    Ok(ret)
}
fn print_event(msg: String) {
    println!(
        "{}: {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        msg
    );
}

fn peer_conn_info_to_string(p: proto::cli::PeerConnInfo) -> String {
    format!(
        "my_peer_id: {}, dst_peer_id: {}, tunnel_info: {:?}",
        p.my_peer_id, p.peer_id, p.tunnel
    )
}
pub fn handle_event(mut events: EventBusSubscriber) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            match e {
                GlobalCtxEvent::PeerAdded(p) => {
                    print_event(format!("新对等节点已添加. 对等节点ID: {}", p));
                }

                GlobalCtxEvent::PeerRemoved(p) => {
                    print_event(format!("对等节点已移除. 对等节点ID: {}", p));
                }

                GlobalCtxEvent::PeerConnAdded(p) => {
                    print_event(format!(
                        "新对等节点连接已添加. 连接信息: {}",
                        peer_conn_info_to_string(p)
                    ));
                }

                GlobalCtxEvent::PeerConnRemoved(p) => {
                    print_event(format!(
                        "对等节点连接已移除. 连接信息: {}",
                        peer_conn_info_to_string(p)
                    ));
                }

                GlobalCtxEvent::ListenerAddFailed(p, msg) => {
                    print_event(format!(
                        "监听器添加失败. 监听器: {}, 消息: {}",
                        p, msg
                    ));
                }

                GlobalCtxEvent::ListenerAcceptFailed(p, msg) => {
                    print_event(format!(
                        "监听器接受连接失败. 监听器: {}, 消息: {}",
                        p, msg
                    ));
                }

                GlobalCtxEvent::ListenerAdded(p) => {
                    if p.scheme() == "ring" {
                        continue;
                    }
                    print_event(format!("新监听器已添加. 监听器: {}", p));
                }

                GlobalCtxEvent::ConnectionAccepted(local, remote) => {
                    print_event(format!(
                        "新连接已接受. 本地: {}, 远程: {}",
                        local, remote
                    ));
                }

                GlobalCtxEvent::ConnectionError(local, remote, err) => {
                    print_event(format!(
                        "连接错误. 本地: {}, 远程: {}, 错误: {}",
                        local, remote, err
                    ));
                }

                GlobalCtxEvent::TunDeviceReady(dev) => {
                    print_event(format!("TUN设备已就绪. 设备: {}", dev));
                }

                GlobalCtxEvent::TunDeviceError(err) => {
                    print_event(format!("TUN设备错误. 错误: {}", err));
                }

                GlobalCtxEvent::Connecting(dst) => {
                    print_event(format!("正在连接到对等节点. 目标: {}", dst));
                }

                GlobalCtxEvent::ConnectError(dst, ip_version, err) => {
                    print_event(format!(
                        "连接到对等节点错误. 目标: {}, IP版本: {}, 错误: {}",
                        dst, ip_version, err
                    ));
                }

                GlobalCtxEvent::VpnPortalClientConnected(portal, client_addr) => {
                    print_event(format!(
                        "VPN门户客户端已连接. 门户: {}, 客户端地址: {}",
                        portal, client_addr
                    ));
                }

                GlobalCtxEvent::VpnPortalClientDisconnected(portal, client_addr) => {
                    print_event(format!(
                        "VPN门户客户端已断开连接. 门户: {}, 客户端地址: {}",
                        portal, client_addr
                    ));
                }

                GlobalCtxEvent::DhcpIpv4Changed(old, new) => {
                    print_event(format!("DHCP IPv4地址已更改. 旧地址: {:?}, 新地址: {:?}", old, new));
                }

                GlobalCtxEvent::DhcpIpv4Conflicted(ip) => {
                    print_event(format!("DHCP IPv4地址冲突. 地址: {:?}", ip));
                }
            }
        }
    })
}




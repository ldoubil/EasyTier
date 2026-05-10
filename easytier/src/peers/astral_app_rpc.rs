//! Application-level peer RPC façade ("envelope" pattern).
//!
//! See `easytier/src/proto/astral_rpc.proto` for the wire-level protocol. The
//! service surface is intentionally tiny and stable — applications layer
//! their own routing on top of the `channel` field plus the `payload` bytes.
//!
//! ## Lifecycle
//!
//! For every running [`crate::instance::instance::Instance`] we create one
//! [`AstralAppRpcService`] and:
//!
//! 1. register a server impl on the instance's [`PeerRpcManager`], so peers in
//!    the same network can call us;
//! 2. publish the service via the global [`REGISTRY`], keyed by the
//!    `instance_id` (uuid). External crates (e.g. `astral_rust_core`) look the
//!    service up there to send / subscribe / reply.
//!
//! ## Inbound dispatch
//!
//! Incoming `Call` / `Notify` / `Ping` requests run on the peer-rpc server.
//!
//! - `Ping` is handled inline (no application involvement).
//! - `Notify` is broadcast on the inbound channel and immediately ack'd.
//! - `Call` is broadcast with a fresh `token`; the server task then awaits a
//!   matching reply via [`AstralAppRpcService::reply_call`] (or times out).
//!
//! Multiple subscribers can listen at the same time (e.g. CLI debug + Dart UI).
//! The first subscriber to call `reply_call(token, …)` wins; later replies for
//! the same token are silently dropped.

use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use tokio::sync::{broadcast, oneshot};

use crate::common::PeerId;
use crate::peers::peer_manager::PeerManager;
use crate::peers::peer_rpc::PeerRpcManager;
use crate::proto::astral_rpc::{
    AppCallRequest, AppCallResponse, AppNotifyRequest, AppNotifyResponse, AppPingRequest,
    AppPingResponse, AstralAppRpc, AstralAppRpcClientFactory, AstralAppRpcServer,
};
use crate::proto::rpc_impl::RpcController;
use crate::proto::rpc_types::controller::Controller as _;
use crate::proto::rpc_types::error::{Error as RpcError, Result as RpcResult};

/// Default channel capacity for the inbound broadcast.
///
/// Must be a power-of-two-ish; large enough to absorb burst traffic for slow
/// subscribers but small enough to bound memory if no subscriber drains.
const INBOUND_CHANNEL_CAPACITY: usize = 1024;

/// Default upper bound on how long a service-side `Call` will wait for the
/// application to produce a reply. The peer-rpc client's own timeout
/// (`RpcController::timeout_ms`) is independent and applies on the caller side.
const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Status codes returned by the service when it cannot dispatch to the
/// application. Application-defined statuses should be `>= 0` (with `0`
/// meaning success); transport-level failures use the negative range below.
pub mod status {
    /// Successful application reply.
    pub const OK: i32 = 0;
    /// No active subscriber on the receiving side.
    pub const NO_SUBSCRIBER: i32 = -1;
    /// Application took too long to call `reply_call`.
    pub const REPLY_TIMEOUT: i32 = -2;
    /// Service has been dropped (instance shut down between dispatch and reply).
    pub const SERVICE_DROPPED: i32 = -3;
}

/// Inbound event delivered to subscribers. Each running instance has its own
/// broadcast channel; subscribe via [`AstralAppRpcService::subscribe_inbound`].
#[derive(Debug, Clone)]
pub enum AppInboundEvent {
    /// Request that expects a reply. The receiver MUST eventually call
    /// [`AstralAppRpcService::reply_call`] with the carried `token`, or the
    /// caller will see `status::REPLY_TIMEOUT`.
    Call {
        from_peer_id: PeerId,
        channel: String,
        request_id: u64,
        /// Opaque token used to route the reply back to the awaiting RPC task.
        token: u64,
        payload: Vec<u8>,
    },
    /// Fire-and-forget notification (the underlying RPC is still ack'd).
    Notify {
        from_peer_id: PeerId,
        channel: String,
        payload: Vec<u8>,
    },
}

/// Per-instance handle for the application RPC service. Cheap to clone (`Arc`).
pub struct AstralAppRpcService {
    instance_id: uuid::Uuid,
    network_name: String,
    my_peer_id: PeerId,
    peer_rpc_mgr: Weak<PeerRpcManager>,

    inbound_tx: broadcast::Sender<AppInboundEvent>,
    pending_replies: DashMap<u64, oneshot::Sender<AppCallResponse>>,
    next_token: AtomicU64,
    reply_timeout: Duration,
}

impl std::fmt::Debug for AstralAppRpcService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AstralAppRpcService")
            .field("instance_id", &self.instance_id)
            .field("network_name", &self.network_name)
            .field("my_peer_id", &self.my_peer_id)
            .field("pending_replies", &self.pending_replies.len())
            .finish()
    }
}

impl AstralAppRpcService {
    fn new(
        instance_id: uuid::Uuid,
        network_name: String,
        my_peer_id: PeerId,
        peer_rpc_mgr: Weak<PeerRpcManager>,
        reply_timeout: Duration,
    ) -> Arc<Self> {
        let (inbound_tx, _) = broadcast::channel(INBOUND_CHANNEL_CAPACITY);
        Arc::new(Self {
            instance_id,
            network_name,
            my_peer_id,
            peer_rpc_mgr,
            inbound_tx,
            pending_replies: DashMap::new(),
            next_token: AtomicU64::new(1),
            reply_timeout,
        })
    }

    /// Network instance UUID this service is attached to.
    pub fn instance_id(&self) -> uuid::Uuid {
        self.instance_id
    }

    /// Network name (used as the peer-rpc `domain`).
    pub fn network_name(&self) -> &str {
        &self.network_name
    }

    /// Local peer id within the EasyTier network.
    pub fn my_peer_id(&self) -> PeerId {
        self.my_peer_id
    }

    /// Subscribe to inbound `Call` and `Notify` events. Multiple subscribers
    /// are supported; each receives an independent copy.
    pub fn subscribe_inbound(&self) -> broadcast::Receiver<AppInboundEvent> {
        self.inbound_tx.subscribe()
    }

    /// Number of currently in-flight inbound calls awaiting an application reply.
    pub fn pending_call_count(&self) -> usize {
        self.pending_replies.len()
    }

    /// Send a `Call` to a specific peer and await the typed response.
    ///
    /// `timeout_ms` controls the *peer-rpc transport* timeout (set on
    /// `RpcController`); the receiver-side application reply timeout is set at
    /// service creation time.
    pub async fn call(
        &self,
        dst_peer_id: PeerId,
        channel: impl Into<String>,
        request_id: u64,
        payload: Vec<u8>,
        flags: u32,
        timeout_ms: i32,
    ) -> Result<AppCallResponse, anyhow::Error> {
        let mgr = self
            .peer_rpc_mgr
            .upgrade()
            .ok_or_else(|| anyhow!("peer rpc manager has been dropped"))?;

        let stub = mgr
            .rpc_client()
            .scoped_client::<AstralAppRpcClientFactory<RpcController>>(
                self.my_peer_id,
                dst_peer_id,
                self.network_name.clone(),
            );

        let mut ctrl = RpcController::default();
        ctrl.set_timeout_ms(timeout_ms);

        stub.call(
            ctrl,
            AppCallRequest {
                from_peer_id: self.my_peer_id,
                channel: channel.into(),
                request_id,
                payload,
                flags,
            },
        )
        .await
        .map_err(|e| anyhow!("astral_app_rpc.call failed: {}", e))
    }

    /// Send a fire-and-forget `Notify` to a specific peer.
    pub async fn notify(
        &self,
        dst_peer_id: PeerId,
        channel: impl Into<String>,
        payload: Vec<u8>,
        timeout_ms: i32,
    ) -> Result<(), anyhow::Error> {
        let mgr = self
            .peer_rpc_mgr
            .upgrade()
            .ok_or_else(|| anyhow!("peer rpc manager has been dropped"))?;

        let stub = mgr
            .rpc_client()
            .scoped_client::<AstralAppRpcClientFactory<RpcController>>(
                self.my_peer_id,
                dst_peer_id,
                self.network_name.clone(),
            );

        let mut ctrl = RpcController::default();
        ctrl.set_timeout_ms(timeout_ms);

        stub.notify(
            ctrl,
            AppNotifyRequest {
                from_peer_id: self.my_peer_id,
                channel: channel.into(),
                payload,
            },
        )
        .await
        .map(|_| ())
        .map_err(|e| anyhow!("astral_app_rpc.notify failed: {}", e))
    }

    /// Round-trip ping. Returns the measured RTT in milliseconds (clock-skew
    /// independent — uses sender-side monotonic-ish wall-clock).
    pub async fn ping(
        &self,
        dst_peer_id: PeerId,
        timeout_ms: i32,
    ) -> Result<i64, anyhow::Error> {
        let mgr = self
            .peer_rpc_mgr
            .upgrade()
            .ok_or_else(|| anyhow!("peer rpc manager has been dropped"))?;

        let stub = mgr
            .rpc_client()
            .scoped_client::<AstralAppRpcClientFactory<RpcController>>(
                self.my_peer_id,
                dst_peer_id,
                self.network_name.clone(),
            );

        let mut ctrl = RpcController::default();
        ctrl.set_timeout_ms(timeout_ms);

        let nonce: u64 = rand::random();
        let send_ms = unix_millis();
        stub.ping(
            ctrl,
            AppPingRequest {
                from_peer_id: self.my_peer_id,
                nonce,
                sender_unix_ms: send_ms,
            },
        )
        .await
        .map_err(|e| anyhow!("astral_app_rpc.ping failed: {}", e))?;

        let rtt = unix_millis().saturating_sub(send_ms) as i64;
        Ok(rtt)
    }

    /// Provide the application reply for an inbound `Call` event identified by
    /// `token`. Returns `true` if the reply was delivered, `false` if the
    /// token was unknown (already replied / timed out / never existed).
    ///
    /// Application-level non-success outcomes should use a positive `status`
    /// (with `0` meaning success); negative codes are reserved for transport.
    pub fn reply_call(
        &self,
        token: u64,
        status: i32,
        error_msg: String,
        payload: Vec<u8>,
    ) -> bool {
        match self.pending_replies.remove(&token) {
            Some((_, tx)) => tx
                .send(AppCallResponse {
                    status,
                    error_msg,
                    payload,
                })
                .is_ok(),
            None => false,
        }
    }
}

// ============================================================================
// Server-side RPC handler.
// ============================================================================

#[derive(Clone)]
struct ServiceImpl {
    parent: Weak<AstralAppRpcService>,
}

impl ServiceImpl {
    fn parent(&self) -> RpcResult<Arc<AstralAppRpcService>> {
        self.parent.upgrade().ok_or(RpcError::Shutdown)
    }
}

#[async_trait::async_trait]
impl AstralAppRpc for ServiceImpl {
    type Controller = RpcController;

    async fn call(
        &self,
        _ctrl: RpcController,
        req: AppCallRequest,
    ) -> RpcResult<AppCallResponse> {
        let parent = self.parent()?;

        // No application is listening: short-circuit so callers don't sit
        // until their RPC timeout expires.
        if parent.inbound_tx.receiver_count() == 0 {
            return Ok(AppCallResponse {
                status: status::NO_SUBSCRIBER,
                error_msg: "no inbound subscriber on receiver".to_string(),
                payload: Vec::new(),
            });
        }

        let token = parent.next_token.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        parent.pending_replies.insert(token, tx);

        let event = AppInboundEvent::Call {
            from_peer_id: req.from_peer_id,
            channel: req.channel,
            request_id: req.request_id,
            token,
            payload: req.payload,
        };

        // SendError only happens if all receivers got dropped between the
        // count check and here; recover the token slot then.
        if parent.inbound_tx.send(event).is_err() {
            parent.pending_replies.remove(&token);
            return Ok(AppCallResponse {
                status: status::NO_SUBSCRIBER,
                error_msg: "inbound subscribers vanished mid-dispatch".to_string(),
                payload: Vec::new(),
            });
        }

        match tokio::time::timeout(parent.reply_timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                // Sender dropped without sending → service was torn down.
                parent.pending_replies.remove(&token);
                Ok(AppCallResponse {
                    status: status::SERVICE_DROPPED,
                    error_msg: "service dropped before reply".to_string(),
                    payload: Vec::new(),
                })
            }
            Err(_) => {
                parent.pending_replies.remove(&token);
                Ok(AppCallResponse {
                    status: status::REPLY_TIMEOUT,
                    error_msg: format!(
                        "application reply timed out after {} ms",
                        parent.reply_timeout.as_millis()
                    ),
                    payload: Vec::new(),
                })
            }
        }
    }

    async fn notify(
        &self,
        _ctrl: RpcController,
        req: AppNotifyRequest,
    ) -> RpcResult<AppNotifyResponse> {
        let parent = self.parent()?;
        // Drop silently if nobody is listening — semantics of fire-and-forget.
        let _ = parent.inbound_tx.send(AppInboundEvent::Notify {
            from_peer_id: req.from_peer_id,
            channel: req.channel,
            payload: req.payload,
        });
        Ok(AppNotifyResponse::default())
    }

    async fn ping(
        &self,
        _ctrl: RpcController,
        req: AppPingRequest,
    ) -> RpcResult<AppPingResponse> {
        Ok(AppPingResponse {
            nonce: req.nonce,
            sender_unix_ms: req.sender_unix_ms,
            receiver_unix_ms: unix_millis(),
        })
    }
}

// ============================================================================
// Multi-instance global registry.
// ============================================================================

static REGISTRY: Lazy<DashMap<uuid::Uuid, Arc<AstralAppRpcService>>> = Lazy::new(DashMap::new);

/// Look up a running instance's RPC service by UUID. Returns `None` if no
/// service has been installed for that instance (or it has been removed).
pub fn get_service(instance_id: &uuid::Uuid) -> Option<Arc<AstralAppRpcService>> {
    REGISTRY.get(instance_id).map(|x| x.value().clone())
}

/// Snapshot of all currently registered instance ids. Useful for diagnostics.
pub fn list_instance_ids() -> Vec<uuid::Uuid> {
    REGISTRY.iter().map(|kv| *kv.key()).collect()
}

/// Convenience: count of registered services.
pub fn instance_count() -> usize {
    REGISTRY.len()
}

// ============================================================================
// Wiring.
// ============================================================================

/// Install the Astral application RPC service on `peer_mgr`'s peer-rpc
/// manager and publish it in the global registry under `instance_id`.
///
/// This is normally called from `Instance::run` once per running instance and
/// must NOT be called twice for the same `instance_id` (the second call
/// overrides the first).
pub fn install(
    instance_id: uuid::Uuid,
    peer_mgr: &Arc<PeerManager>,
) -> Arc<AstralAppRpcService> {
    install_with_reply_timeout(instance_id, peer_mgr, DEFAULT_REPLY_TIMEOUT)
}

/// Variant of [`install`] allowing the caller to override the application
/// reply timeout (useful for tests / specialized integrations).
pub fn install_with_reply_timeout(
    instance_id: uuid::Uuid,
    peer_mgr: &Arc<PeerManager>,
    reply_timeout: Duration,
) -> Arc<AstralAppRpcService> {
    let network_name = peer_mgr.get_global_ctx().get_network_name();
    let my_peer_id = peer_mgr.my_peer_id();
    let rpc_mgr = peer_mgr.get_peer_rpc_mgr();

    let svc = AstralAppRpcService::new(
        instance_id,
        network_name.clone(),
        my_peer_id,
        Arc::downgrade(&rpc_mgr),
        reply_timeout,
    );

    rpc_mgr.rpc_server().registry().register(
        AstralAppRpcServer::new(ServiceImpl {
            parent: Arc::downgrade(&svc),
        }),
        &network_name,
    );

    REGISTRY.insert(instance_id, svc.clone());
    tracing::info!(
        ?instance_id,
        ?my_peer_id,
        network = %network_name,
        "astral app rpc service installed"
    );
    svc
}

/// Remove the service for `instance_id` from the global registry. The peer-rpc
/// server registration is automatically cleared by
/// `PeerManager::clear_resources` (which calls `unregister_all`).
pub fn uninstall(instance_id: &uuid::Uuid) -> Option<Arc<AstralAppRpcService>> {
    let removed = REGISTRY.remove(instance_id).map(|(_, v)| v);
    if removed.is_some() {
        tracing::info!(?instance_id, "astral app rpc service uninstalled");
    }
    removed
}

// ============================================================================
// Helpers.
// ============================================================================

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_insert_remove_roundtrip() {
        let id = uuid::Uuid::new_v4();
        assert!(get_service(&id).is_none());

        // Build a synthetic service without going through `install` (no real
        // peer manager needed for registry mechanics).
        let (tx, _rx) = broadcast::channel(8);
        let svc = Arc::new(AstralAppRpcService {
            instance_id: id,
            network_name: "test".into(),
            my_peer_id: 1,
            peer_rpc_mgr: Weak::new(),
            inbound_tx: tx,
            pending_replies: DashMap::new(),
            next_token: AtomicU64::new(1),
            reply_timeout: Duration::from_secs(1),
        });
        REGISTRY.insert(id, svc.clone());
        assert_eq!(get_service(&id).map(|s| s.my_peer_id()), Some(1));
        assert!(list_instance_ids().contains(&id));
        assert!(uninstall(&id).is_some());
        assert!(get_service(&id).is_none());
    }

    #[test]
    fn reply_call_returns_false_for_unknown_token() {
        let (tx, _rx) = broadcast::channel(8);
        let svc = AstralAppRpcService {
            instance_id: uuid::Uuid::new_v4(),
            network_name: "test".into(),
            my_peer_id: 1,
            peer_rpc_mgr: Weak::new(),
            inbound_tx: tx,
            pending_replies: DashMap::new(),
            next_token: AtomicU64::new(1),
            reply_timeout: Duration::from_secs(1),
        };
        assert!(!svc.reply_call(42, 0, "".into(), vec![]));
    }
}

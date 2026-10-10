// Generated bindings for `astral_rpc.proto`. The raw types/services are
// produced by `prost` + the RPC service generator at build time.
// Application-level glue (service impl, multi-instance registry, send
// helpers) lives in `easytier_core::peers::astral_app_rpc`.
include!(concat!(env!("OUT_DIR"), "/astral_rpc.rs"));
#[cfg(feature = "json-rpc")]
include!(concat!(env!("OUT_DIR"), "/astral_rpc.serde.rs"));

use crate::network::config::NetworkConfig;
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ProtocolConfig {
    pub network: NetworkConfig,
    pub message_timeout_seconds: f32,
    pub ask_peer_list_interval_seconds: f32,
}

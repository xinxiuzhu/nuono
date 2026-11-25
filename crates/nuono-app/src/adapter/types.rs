use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterConfig {
    pub id: String,
    pub adapter_type: String,
    pub enabled: bool,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterInfo {
    pub id: String,
    pub adapter_type: String,
    pub name: String,
    pub status: AdapterStatus,
    pub enabled: bool,
    pub platform_name: String,
    pub capabilities: AdapterCapabilitiesInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterCapabilitiesInfo {
    pub send_message: bool,
    pub delete_message: bool,
    pub send_image: bool,
    pub send_voice: bool,
    pub send_video: bool,
    pub group_ban: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AdapterStatus {
    Stopped,
    Running,
    Error,
}

impl std::fmt::Display for AdapterStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterStatus::Stopped => write!(f, "stopped"),
            AdapterStatus::Running => write!(f, "running"),
            AdapterStatus::Error => write!(f, "error"),
        }
    }
}

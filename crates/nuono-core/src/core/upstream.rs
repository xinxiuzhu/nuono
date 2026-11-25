use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum UpstreamType {
    #[serde(rename = "astrbot")]
    #[default]
    AstrBot,
    #[serde(rename = "nonebot")]
    NoneBot,
    #[serde(rename = "go-cqhttp")]
    GoCQHTTP,
    #[serde(rename = "custom")]
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpstreamConfig {
    pub url: String,
    pub upstream_type: UpstreamType,
    pub headers: HashMap<String, String>,
}

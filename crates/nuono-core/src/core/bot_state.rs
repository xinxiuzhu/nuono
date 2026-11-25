//! 进程内的 OneBot 实时态。数据库连接行记录历史，不代表在线。
use crate::service::BotStatus;
use chrono::{DateTime, Utc};
use nuono_app::transport::TransportFrame;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
struct Entry {
    generation: u64,
    sender: mpsc::Sender<TransportFrame>,
    ip: String,
    platform: String,
    nickname: Option<String>,
    last_seen: DateTime<Utc>,
    heartbeat: Option<DateTime<Utc>>,
    evidence: Option<(Instant, bool, bool)>,
    timeout: Duration,
    failure: Option<String>,
    evidence_updated: Instant,
}
#[derive(Default)]
pub struct BotStateRegistry {
    entries: Mutex<HashMap<i64, Entry>>,
}
impl BotStateRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(
        &self,
        id: i64,
        generation: u64,
        sender: mpsc::Sender<TransportFrame>,
        ip: String,
        platform: String,
        timeout: Duration,
    ) {
        let mut entries = self.entries.lock().expect("bot state mutex poisoned");
        if entries.get(&id).is_some_and(|e| e.generation > generation) {
            return;
        }
        entries.insert(
            id,
            Entry {
                generation,
                sender,
                ip,
                platform,
                nickname: None,
                last_seen: Utc::now(),
                heartbeat: None,
                evidence: None,
                timeout,
                failure: None,
                evidence_updated: Instant::now(),
            },
        );
    }
    pub fn remove(&self, id: i64, generation: u64) -> bool {
        let mut entries = self.entries.lock().expect("bot state mutex poisoned");
        if entries.get(&id).is_some_and(|e| e.generation == generation) {
            entries.remove(&id);
            true
        } else {
            false
        }
    }
    pub fn connection(&self, id: i64) -> Option<(u64, mpsc::Sender<TransportFrame>)> {
        self.entries
            .lock()
            .expect("bot state mutex poisoned")
            .get(&id)
            .filter(|e| !e.sender.is_closed())
            .map(|e| (e.generation, e.sender.clone()))
    }
    pub fn observe(&self, id: i64, generation: u64, value: &Value) {
        let mut entries = self.entries.lock().expect("bot state mutex poisoned");
        let Some(e) = entries.get_mut(&id).filter(|e| e.generation == generation) else {
            return;
        };
        e.last_seen = Utc::now();
        if value.get("post_type").and_then(Value::as_str) == Some("meta_event")
            && value.get("meta_event_type").and_then(Value::as_str) == Some("heartbeat")
        {
            e.heartbeat = Some(e.last_seen);
            e.evidence_updated = Instant::now();
            if let Some(interval) = value
                .get("interval")
                .and_then(Value::as_u64)
                .filter(|v| *v > 0 && *v <= 3_600_000)
            {
                e.timeout = e
                    .timeout
                    .max(Duration::from_millis(interval.saturating_mul(2)));
            }
            match parse_status(&value["status"]) {
                Ok((online, good)) => {
                    e.evidence = Some((Instant::now(), online, good));
                    e.failure = None;
                }
                Err(_) => {
                    e.evidence = None;
                    e.failure = Some("status_unconfirmed".into());
                }
            }
        }
    }
    pub fn probe_result(
        &self,
        id: i64,
        generation: u64,
        result: std::result::Result<&Value, String>,
    ) -> bool {
        self.probe_result_since(id, generation, Instant::now(), result)
    }
    pub fn probe_result_since(
        &self,
        id: i64,
        generation: u64,
        started: Instant,
        result: std::result::Result<&Value, String>,
    ) -> bool {
        let mut entries = self.entries.lock().expect("bot state mutex poisoned");
        let Some(e) = entries
            .get_mut(&id)
            .filter(|e| e.generation == generation && e.evidence_updated <= started)
        else {
            return false;
        };
        e.evidence_updated = Instant::now();
        match result.and_then(parse_status) {
            Ok((online, good)) => {
                e.evidence = Some((Instant::now(), online, good));
                e.last_seen = Utc::now();
                e.failure = None;
            }
            Err(reason) => {
                e.failure = Some(reason);
            }
        }
        true
    }
    /// 仅当连接批次号匹配当前活跃连接时才缓存账号身份信息；
    /// 能查到登录身份并不代表账号整体处于健康可用状态。
    pub fn login_info_result(&self, id: i64, generation: u64, value: &Value) -> Option<String> {
        let user_id = value
            .get("user_id")
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))?;
        if user_id != id {
            return None;
        }
        let nickname = value.get("nickname")?.as_str()?.trim();
        if nickname.is_empty() {
            return None;
        }
        let mut entries = self.entries.lock().expect("bot state mutex poisoned");
        let entry = entries
            .get_mut(&id)
            .filter(|entry| entry.generation == generation)?;
        entry.nickname = Some(nickname.to_owned());
        Some(nickname.to_owned())
    }

    pub fn snapshot(&self, id: i64) -> Option<BotStatus> {
        self.entries
            .lock()
            .expect("bot state mutex poisoned")
            .get(&id)
            .map(|e| project(id, e, Instant::now()))
    }
    pub fn snapshots(&self) -> Vec<BotStatus> {
        self.entries
            .lock()
            .expect("bot state mutex poisoned")
            .iter()
            .map(|(id, e)| project(*id, e, Instant::now()))
            .collect()
    }
}
fn parse_status(value: &Value) -> std::result::Result<(bool, bool), String> {
    if value.get("data").is_some() || value.get("retcode").is_some() || value.get("echo").is_some()
    {
        return Err("invalid_status_response".into());
    }
    let online = value
        .get("online")
        .and_then(Value::as_bool)
        .ok_or_else(|| "status_unconfirmed".to_string())?;
    let good = match value.get("good") {
        None => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| "invalid_status_response".to_string())?,
    };
    Ok((online, good))
}
fn project(id: i64, e: &Entry, now: Instant) -> BotStatus {
    let connected = !e.sender.is_closed();
    let (status, reason) = if !connected {
        ("offline", "transport_disconnected".to_string())
    } else if let Some(failure) = &e.failure {
        ("degraded", failure.clone())
    } else {
        match e.evidence {
            None => ("unknown", "status_unconfirmed".into()),
            Some((at, _, _)) if now.saturating_duration_since(at) > e.timeout => {
                ("degraded", "heartbeat_expired".into())
            }
            Some((_, false, _)) => ("offline", "platform_offline".into()),
            // 客户端上报的聚合健康度可能包含与消息收发无关的内部模块状态。
            Some((_, true, false)) => ("online", "platform_unhealthy".into()),
            Some((_, true, true)) => ("online", "status_confirmed".into()),
        }
    };
    BotStatus {
        bot_id: id.to_string(),
        nickname: e.nickname.clone(),
        status: status.into(),
        ip_address: e.ip.clone(),
        last_heartbeat: e
            .heartbeat
            .map(crate::clock::format_db_ts)
            .unwrap_or_default(),
        platform: e.platform.clone(),
        status_reason: Some(reason),
        transport_connected: Some(connected),
        last_seen: Some(e.last_seen.to_rfc3339()),
    }
}
pub fn disconnected(id: impl Into<String>) -> BotStatus {
    BotStatus {
        bot_id: id.into(),
        nickname: None,
        status: "offline".into(),
        ip_address: String::new(),
        last_heartbeat: String::new(),
        platform: "onebot".into(),
        status_reason: Some("transport_disconnected".into()),
        transport_connected: Some(false),
        last_seen: None,
    }
}
#[cfg(test)]
#[path = "../../tests/unit/core/bot_state.rs"]
mod tests;

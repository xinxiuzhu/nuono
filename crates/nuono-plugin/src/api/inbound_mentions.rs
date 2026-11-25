//! 原生入站提及（`@`）解析，保留平台身份作用域与精确的文本字节区间。
//!
//! 单纯的显示名称、裸 `@username` 文本或带应用作用域平台上的无类型 ID，都不能作为可靠的用户身份依据。
//! 这些辅助方法仅用于解析入站提及，不会改变指令匹配逻辑。
use std::{borrow::Cow, ops::Range};

use anyhow::{Result, anyhow, bail, ensure};
use nuono_domain::{
    DomainEvent, IdentityKind, IdentityRef, IdentityScheme, MessageSegment, Platform,
};
use serde_json::Value;

use crate::{Event, RuntimeBotRoute};

const MAX_MENTIONS: usize = 64;
const MAX_SEGMENTS: usize = 4096;
const MAX_TEXT_BYTES: usize = 256 * 1024;

impl Event<'_> {
    /// 从强类型事件包装器或其经校验的运行时 JSON 投影中读取原生领域事件（[`DomainEvent`]）。
    /// 若事件未携带原生领域元数据则返回 `Ok(None)`；若元数据存在但格式损坏则返回错误。
    /// 现有的借用型 `domain_event()` 访问器保持不变。
    pub fn native_domain_event(&self) -> Result<Option<Cow<'_, DomainEvent>>> {
        domain_event(self)
    }

    /// 按原消息中的出现顺序返回去重后且带完整作用域的原生用户提及列表（已自动排除对 Bot 自身的提及）。
    /// 若遇到身份作用域不明或格式损坏的提及元数据则返回错误；纯文本显示名形式的 `@` 不会在此解析。
    pub fn native_user_mentions(&self) -> Result<Vec<IdentityRef>> {
        Ok(collect(self)?.users)
    }

    /// 仅剔除原消息元数据能够明确证明字符区间位置的原生 `@` 提及。
    /// 其余文本（包括空白字符和未经平台身份证实的裸 `@username`）均原样保留。
    /// 对 Bot 自身的原生提及虽然不会出现在用户提及列表中，但其对应的 `@` 文本仍会被一并移除。
    pub fn text_without_native_mentions(&self) -> Result<String> {
        let mentions = collect(self)?;
        strip_ranges(self.text(), &mentions.ranges)
    }

    /// 在不丢失 Telegram UTF-16 实体偏移量的前提下，剔除指定文本片段中的原生 `@` 提及。
    /// 传入的 `fragment` 必须在 `Event::text()` 中恰好唯一出现一次（空字符串直接返回空）；
    /// 若片段边界恰好把某个原生 `@` 截断成两半，则返回错误。
    pub fn text_without_native_mentions_fragment(&self, fragment: &str) -> Result<String> {
        let mentions = collect(self)?;
        if fragment.is_empty() {
            return Ok(String::new());
        }
        let start = unique_substring(self.text(), fragment)?;
        let end = start + fragment.len();
        let mut ranges = Vec::new();
        for range in mentions.ranges {
            if range.end <= start || range.start >= end {
                continue;
            }
            ensure!(
                range.start >= start && range.end <= end,
                "消息片段截断了原生提及"
            );
            ranges.push(range.start - start..range.end - start);
        }
        strip_ranges(fragment, &ranges)
    }
}

#[derive(Default)]
struct Mentions {
    users: Vec<IdentityRef>,
    ranges: Vec<Range<usize>>,
    bots: Vec<IdentityRef>,
}

impl Mentions {
    fn record(&mut self, identity: IdentityRef, bot: bool) -> Result<()> {
        if bot {
            self.users.retain(|user| user != &identity);
            if !self.bots.contains(&identity) {
                self.bots.push(identity);
            }
        } else if !self.users.contains(&identity) && !self.bots.contains(&identity) {
            self.users.push(identity);
        }
        ensure!(
            self.users.len() + self.bots.len() <= MAX_MENTIONS,
            "原生提及数量超过上限"
        );
        Ok(())
    }

    fn finish(mut self, text: &str) -> Result<Self> {
        self.ranges.sort_by_key(|range| (range.start, range.end));
        self.ranges.dedup();
        ensure!(self.ranges.len() <= MAX_MENTIONS, "原生提及数量超过上限");
        let mut end = 0;
        for range in &self.ranges {
            ensure!(
                range.start >= end
                    && range.start < range.end
                    && range.end <= text.len()
                    && text.is_char_boundary(range.start)
                    && text.is_char_boundary(range.end),
                "原生提及文本范围无效或重叠"
            );
            end = range.end;
        }
        Ok(self)
    }
}

fn collect(event: &Event<'_>) -> Result<Mentions> {
    ensure!(
        event.text().len() <= MAX_TEXT_BYTES,
        "消息文本超过提及解析上限"
    );
    let domain = domain_event(event)?;
    let route = event
        .route()
        .ok_or_else(|| anyhow!("原生提及缺少机器人路由"))?;
    ensure!(
        !route.instance_id.is_empty() && !route.bot_self_id.is_empty(),
        "原生提及机器人路由无效"
    );
    let platform = platform(&route.platform);
    ensure!(
        event.platform().is_empty() || self::platform(event.platform()) == platform,
        "原生提及事件平台与机器人路由不一致"
    );
    if let Some(domain) = domain.as_deref() {
        ensure!(
            domain.bot.platform() == &platform
                && domain.bot.instance_id.as_str() == route.instance_id
                && domain.bot.account_id.value().as_str() == route.bot_self_id,
            "原生提及事件与机器人路由不一致"
        );
    }
    let message = domain.as_deref().and_then(DomainEvent::message);
    let raw = message
        .map(|message| &message.raw)
        .filter(|raw| !raw.is_null())
        .or_else(|| event.raw_event());
    let mut mentions = Mentions::default();
    let at_ids = if let Some(message) = message {
        ensure!(message.segments.len() <= MAX_SEGMENTS, "消息段数量超过上限");
        message
            .segments
            .iter()
            .filter_map(|segment| match segment {
                MessageSegment::At { user_id } => Some(user_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    } else if platform == Platform::OneBot {
        raw_at_ids(
            raw.and_then(|raw| raw.get("message"))
                .filter(|value| !value.is_null())
                .or_else(|| event.message()),
        )?
    } else {
        // 仅凭运行时投影消息段无法证明飞书或 QQ 的身份体系（scheme）与应用命名空间。
        raw_at_ids(event.message())?
    };
    ensure!(at_ids.len() <= MAX_MENTIONS, "原生提及数量超过上限");
    match platform {
        Platform::OneBot => {
            for id in &at_ids {
                if id == "all" {
                    continue;
                }
                mentions.record(
                    identity(Platform::OneBot, IdentityScheme::Native, id, None)?,
                    id == &route.bot_self_id,
                )?;
            }
            onebot_text_ranges(
                event.text(),
                raw.or_else(|| Some(event.raw())),
                &at_ids,
                &mut mentions,
            )?;
            // 部分 `Event::new` 调用方传入了已解析的 OneBot 消息段，但未附带 `DomainEvent` 快照。
            // 若整条消息段数组能与文本完全对齐，仍可据此定位 `@ID` 的字符区间；但局部子串或裸文本 `@ID` 不予采信。
            if domain.is_none()
                && let Some(segments) = event.message().and_then(Value::as_array)
            {
                let projected_ids = raw_at_ids(event.message())?;
                ensure!(
                    projected_ids == at_ids,
                    "OneBot 原消息与 runtime 提及消息段不一致"
                );
                let _ = projected_at_ranges(event.text(), segments, &at_ids, &mut mentions)?;
            }
        }
        Platform::Telegram => telegram_mentions(event.text(), raw, &route, &at_ids, &mut mentions)?,
        Platform::Discord => discord_mentions(event.text(), raw, &route, &at_ids, &mut mentions)?,
        Platform::Feishu => feishu_mentions(
            event.text(),
            raw,
            &route,
            &at_ids,
            domain
                .as_deref()
                .is_some_and(|domain| *domain.bot.account_id.scheme() == IdentityScheme::AppId),
            &mut mentions,
        )?,
        Platform::QqOfficial | Platform::Other(_) | _ => {
            ensure!(
                at_ids.is_empty(),
                "此平台的原生提及缺少明确身份规则及作用域"
            );
            if let Some(raw) = raw {
                ensure!(
                    raw.get("mentions").is_none_or(
                        |value| value.is_null() || value.as_array().is_some_and(Vec::is_empty)
                    ),
                    "此平台的原生提及身份规则尚不受支持"
                );
            }
        }
    }
    // 领域事件投影会把每个 At 段格式化为 `@native_id`；
    // 仅当整条投影消息拼出的文本与 `event.text()` 完全一致时，才按该投影移除 `@native_id` 区间。
    if let Some(domain) = domain.as_deref() {
        let projected =
            crate::domain::projection::project_legacy_runtime_event(domain, "mention_ranges");
        if projected.get("raw_message").and_then(Value::as_str) == Some(event.text())
            && let Some(segments) = projected.get("message").and_then(Value::as_array)
        {
            ensure!(
                projected_at_ranges(event.text(), segments, &at_ids, &mut mentions)?,
                "原生消息提及投影长度或范围不一致"
            );
        }
    }
    mentions.finish(event.text())
}

fn domain_event<'a>(event: &'a Event<'_>) -> Result<Option<Cow<'a, DomainEvent>>> {
    if let Some(domain) = event.domain_event() {
        return Ok(Some(Cow::Borrowed(domain)));
    }
    match event
        .raw()
        .get("domain_event")
        .filter(|value| !value.is_null())
    {
        Some(value) => {
            bound_domain_metadata(value)?;
            Ok(Some(Cow::Owned(
                serde_json::from_value(value.clone())
                    .map_err(|_| anyhow!("事件中的原生领域身份无效"))?,
            )))
        }
        None => Ok(None),
    }
}

fn bound_domain_metadata(value: &Value) -> Result<()> {
    let mut pending = vec![(value, 0usize)];
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        ensure!(
            nodes <= 100_000 && depth <= 32,
            "领域事件提及元数据超过解析上限"
        );
        match value {
            Value::String(value) => bytes = bytes.saturating_add(value.len()),
            Value::Array(values) => {
                ensure!(
                    values.len() <= 100_000usize.saturating_sub(nodes),
                    "领域事件提及元数据超过解析上限"
                );
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                ensure!(
                    values.len() <= 100_000usize.saturating_sub(nodes),
                    "领域事件提及元数据超过解析上限"
                );
                for (key, value) in values {
                    bytes = bytes.saturating_add(key.len());
                    pending.push((value, depth + 1));
                }
            }
            _ => {}
        }
        ensure!(
            bytes <= 4 * 1024 * 1024 && pending.len() <= 100_000,
            "领域事件提及元数据超过解析上限"
        );
    }
    Ok(())
}

fn platform(value: &str) -> Platform {
    match value.to_ascii_lowercase().as_str() {
        "onebot" | "onebot v11" => Platform::OneBot,
        "telegram" => Platform::Telegram,
        "discord" => Platform::Discord,
        "feishu" => Platform::Feishu,
        "qq" | "qq_official" | "qq official" => Platform::QqOfficial,
        _ => Platform::Other(value.to_owned()),
    }
}

fn exact_id(value: &Value) -> Result<String> {
    let id = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) if value.as_i64().is_some() || value.as_u64().is_some() => {
            value.to_string()
        }
        _ => bail!("原生提及用户 ID 无效"),
    };
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control) && id.trim() == id,
        "原生提及用户 ID 无效"
    );
    Ok(id)
}

fn identity(
    platform: Platform,
    scheme: IdentityScheme,
    id: &str,
    scope: Option<&str>,
) -> Result<IdentityRef> {
    exact_id(&Value::String(id.to_owned()))?;
    if scheme == IdentityScheme::Numeric {
        ensure!(
            id.bytes().all(|byte| byte.is_ascii_digit())
                && id.parse::<i64>().ok().is_some_and(|id| id > 0),
            "Telegram 提及用户 ID 必须为正整数"
        );
    }
    if scheme == IdentityScheme::Snowflake {
        ensure!(
            id.bytes().all(|byte| byte.is_ascii_digit())
                && id.parse::<u64>().ok().is_some_and(|id| id > 0),
            "Discord 提及用户 ID 必须为正整数"
        );
    }
    Ok(IdentityRef::new(
        platform,
        IdentityKind::User,
        scheme,
        id,
        scope,
    )?)
}

fn raw_at_ids(value: Option<&Value>) -> Result<Vec<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let Some(segments) = value.as_array() else {
        // 原始 OneBot CQ 码字符串本身不包含经过验证的结构化身份信息。
        ensure!(value.is_string(), "入站消息段格式无效");
        return Ok(Vec::new());
    };
    ensure!(segments.len() <= MAX_SEGMENTS, "消息段数量超过上限");
    segments
        .iter()
        .filter(|segment| segment.get("type").and_then(Value::as_str) == Some("at"))
        .map(|segment| {
            exact_id(
                segment
                    .get("data")
                    .and_then(|data| data.get("qq"))
                    .ok_or_else(|| anyhow!("原生提及缺少用户 ID"))?,
            )
        })
        .collect()
}

fn array_field<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a [Value]> {
    match value
        .and_then(|value| value.get(name))
        .filter(|value| !value.is_null())
    {
        Some(value) => {
            let values = value
                .as_array()
                .ok_or_else(|| anyhow!("原生提及 {name} 字段无效"))?;
            let limit = if matches!(name, "entities" | "caption_entities") {
                MAX_SEGMENTS
            } else {
                MAX_MENTIONS
            };
            ensure!(values.len() <= limit, "原生提及元数据数量超过上限");
            Ok(values)
        }
        None => Ok(&[]),
    }
}

fn optional_bot(value: &Value, field: &str) -> Result<bool> {
    match value.get(field).filter(|value| !value.is_null()) {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| anyhow!("原生提及机器人标记无效")),
        None => Ok(false),
    }
}

fn discord_mentions(
    text: &str,
    raw: Option<&Value>,
    route: &RuntimeBotRoute,
    ats: &[String],
    mentions: &mut Mentions,
) -> Result<()> {
    let mut proven = Vec::new();
    for user in array_field(raw, "mentions")? {
        let id = exact_id(
            user.get("id")
                .ok_or_else(|| anyhow!("Discord 提及缺少用户 ID"))?,
        )?;
        let bot = optional_bot(user, "bot")? || id == route.bot_self_id;
        mentions.record(
            identity(Platform::Discord, IdentityScheme::Snowflake, &id, None)?,
            bot,
        )?;
        proven.push(id);
    }
    for id in ats {
        if id == "all" {
            continue;
        }
        if !proven.contains(id) {
            mentions.record(
                identity(Platform::Discord, IdentityScheme::Snowflake, id, None)?,
                id == &route.bot_self_id,
            )?;
            proven.push(id.clone());
        }
    }
    // Discord 的 mentions 元数据只提供用户 ID 而没有字符偏移量。
    // 因此仅在非转义文本且处于 Markdown 代码块之外的位置匹配原生 `<@id>` / `<@!id>` 标记；普通显示名保持不动。
    let bytes = text.as_bytes();
    let mut cursor = 0;
    let mut code_ticks = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor += 1;
            if let Some(ch) = text[cursor..].chars().next() {
                cursor += ch.len_utf8();
            }
            continue;
        }
        if bytes[cursor] == b'`' {
            let run = bytes[cursor..]
                .iter()
                .take_while(|byte| **byte == b'`')
                .count();
            if code_ticks == 0 {
                code_ticks = run;
            } else if code_ticks == run {
                code_ticks = 0;
            }
            cursor += run;
            continue;
        }
        if code_ticks == 0
            && text[cursor..].starts_with("<@")
            && let Some(end) = text[cursor..].find('>').map(|end| cursor + end + 1)
        {
            let id = text[cursor + 2..end - 1]
                .strip_prefix('!')
                .unwrap_or(&text[cursor + 2..end - 1]);
            if proven.iter().any(|proven| proven == id) {
                mentions.ranges.push(cursor..end);
            }
            cursor = end;
            continue;
        }
        cursor += text[cursor..]
            .chars()
            .next()
            .expect("nonempty remainder")
            .len_utf8();
    }
    Ok(())
}

fn telegram_mentions(
    text: &str,
    raw: Option<&Value>,
    route: &RuntimeBotRoute,
    ats: &[String],
    mentions: &mut Mentions,
) -> Result<()> {
    for id in ats {
        if id == "all" {
            continue;
        }
        mentions.record(
            identity(Platform::Telegram, IdentityScheme::Numeric, id, None)?,
            id == &route.bot_self_id,
        )?;
    }
    let Some(raw) = raw else {
        return Ok(());
    };
    let (source, entities) = if let Some(value) = raw.get("text").filter(|value| !value.is_null()) {
        (
            value
                .as_str()
                .ok_or_else(|| anyhow!("Telegram 原消息文本无效"))?,
            array_field(Some(raw), "entities")?,
        )
    } else if let Some(value) = raw.get("caption").filter(|value| !value.is_null()) {
        (
            value
                .as_str()
                .ok_or_else(|| anyhow!("Telegram 原消息说明无效"))?,
            array_field(Some(raw), "caption_entities")?,
        )
    } else {
        ensure!(
            array_field(Some(raw), "entities")?.is_empty()
                && array_field(Some(raw), "caption_entities")?.is_empty(),
            "Telegram 提及实体缺少原文本"
        );
        return Ok(());
    };
    let mut offset = None;
    for entity in entities {
        let kind = entity
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Telegram 实体类型无效"))?;
        let (id, bot) = if kind == "text_mention" {
            let user = entity
                .get("user")
                .filter(|user| user.is_object())
                .ok_or_else(|| anyhow!("Telegram 文本提及缺少原生 User"))?;
            (
                exact_id(
                    user.get("id")
                        .ok_or_else(|| anyhow!("Telegram 提及缺少用户 ID"))?,
                )?,
                optional_bot(user, "is_bot")?,
            )
        } else if kind == "text_link" {
            let url = entity
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Telegram 链接实体地址无效"))?;
            if !url.starts_with("tg://user") {
                continue;
            }
            let id = url
                .strip_prefix("tg://user?id=")
                .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
                .ok_or_else(|| anyhow!("Telegram 原生用户链接无效"))?;
            (id.to_owned(), false)
        } else {
            ensure!(
                entity.get("user").is_none_or(Value::is_null),
                "Telegram 实体携带了未支持的原生用户身份规则"
            );
            // 纯 `@username` 形式的 mention 实体不包含不可变的 Telegram 原生用户数字 ID，因此跳过。
            continue;
        };
        let start = entity
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("Telegram 提及位置无效"))?;
        let length = entity
            .get("length")
            .and_then(Value::as_u64)
            .filter(|length| *length > 0)
            .ok_or_else(|| anyhow!("Telegram 提及长度无效"))?;
        let range = utf16_range(source, start, length)?;
        let base = match offset {
            Some(offset) => offset,
            None => {
                let base = unique_substring(text, source)?;
                offset = Some(base);
                base
            }
        };
        mentions.record(
            identity(Platform::Telegram, IdentityScheme::Numeric, &id, None)?,
            bot || id == route.bot_self_id,
        )?;
        mentions.ranges.push(base + range.start..base + range.end);
    }
    Ok(())
}

fn feishu_mentions(
    text: &str,
    raw: Option<&Value>,
    route: &RuntimeBotRoute,
    ats: &[String],
    app_id_proven: bool,
    mentions: &mut Mentions,
) -> Result<()> {
    let raw = raw.ok_or_else(|| anyhow!("飞书原生提及缺少原消息"))?;
    ensure!(
        app_id_proven
            || raw.get("app_id").is_some()
            || raw
                .get("header")
                .and_then(|header| header.get("app_id"))
                .is_some(),
        "飞书原生提及缺少已证明的应用 ID"
    );
    for value in [
        raw.get("app_id"),
        raw.get("header").and_then(|header| header.get("app_id")),
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            value.as_str() == Some(route.bot_self_id.as_str()),
            "飞书原生提及应用作用域与机器人不一致"
        );
    }
    let message = raw
        .get("message")
        .filter(|message| message.is_object())
        .unwrap_or(raw);
    let mut proven = Vec::new();
    let mut keys = Vec::<(String, IdentityRef)>::new();
    for mention in array_field(Some(message), "mentions")? {
        let fields = mention
            .get("id")
            .filter(|id| id.is_object())
            .ok_or_else(|| anyhow!("飞书提及缺少原生身份字段"))?;
        let mut selected = None;
        for (field, scheme) in [
            ("open_id", IdentityScheme::OpenId),
            ("user_id", IdentityScheme::UserId),
            ("union_id", IdentityScheme::UnionId),
        ] {
            if let Some(value) = fields.get(field).filter(|value| !value.is_null()) {
                ensure!(value.is_string(), "飞书原生提及身份字段必须为字符串");
                let id = exact_id(value)?;
                proven.push(id.clone());
                if selected.is_none() {
                    selected = Some((id, scheme));
                }
            }
        }
        let (id, scheme) = selected.ok_or_else(|| anyhow!("飞书提及没有明确身份规则"))?;
        let key = mention
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("飞书提及缺少原消息 key"))?;
        ensure!(
            key.len() <= 128
                && key
                    .strip_prefix("@_user_")
                    .is_some_and(|tail| !tail.is_empty()
                        && tail
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')),
            "飞书提及 key 无效"
        );
        let user = identity(Platform::Feishu, scheme, &id, Some(&route.bot_self_id))?;
        if let Some((_, previous)) = keys.iter().find(|(previous_key, _)| previous_key == key) {
            ensure!(previous == &user, "飞书提及 key 对应了不同原生身份");
        } else {
            keys.push((key.to_owned(), user.clone()));
        }
        mentions.record(user, optional_bot(mention, "is_bot")?)?;
        for (start, _) in text.match_indices(key) {
            // 检查后继字符边界，避免在匹配 `@_user_1` 时误截断 `@_user_12` 的前半截。
            if text[start + key.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
            {
                mentions.ranges.push(start..start + key.len());
            }
        }
        proven.push(key.to_owned());
    }
    for id in ats {
        ensure!(
            id == "all" || proven.contains(id),
            "飞书 At 段缺少可证明的身份规则及应用作用域"
        );
    }
    Ok(())
}

fn onebot_text_ranges(
    text: &str,
    raw: Option<&Value>,
    ids: &[String],
    mentions: &mut Mentions,
) -> Result<()> {
    let Some(raw) = raw else {
        return Ok(());
    };
    if raw.get("raw_message").and_then(Value::as_str) != Some(text) {
        return Ok(());
    }
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(start) = text[cursor..].find("[CQ:at,").map(|start| cursor + start) {
        let end = text[start..]
            .find(']')
            .map(|end| start + end + 1)
            .ok_or_else(|| anyhow!("OneBot 原生提及编码不完整"))?;
        let mut id = None;
        for field in text[start + 7..end - 1].split(',') {
            if let Some(value) = field.strip_prefix("qq=") {
                ensure!(id.is_none(), "OneBot 原生提及身份字段重复");
                id = Some(
                    value
                        .replace("&#44;", ",")
                        .replace("&#91;", "[")
                        .replace("&#93;", "]")
                        .replace("&amp;", "&"),
                );
            }
        }
        let id = id.ok_or_else(|| anyhow!("OneBot 原生提及缺少用户 ID"))?;
        found.push(id.clone());
        if id != "all" && ids.contains(&id) {
            mentions.ranges.push(start..end);
        }
        cursor = end;
    }
    if !found.is_empty() {
        let expected = ids
            .iter()
            .filter(|id| id.as_str() != "all")
            .collect::<Vec<_>>();
        let actual = found
            .iter()
            .filter(|id| id.as_str() != "all")
            .collect::<Vec<_>>();
        ensure!(actual == expected, "OneBot 原生提及文本与消息段不一致");
    }
    Ok(())
}

fn projected_at_ranges(
    text: &str,
    segments: &[Value],
    ids: &[String],
    mentions: &mut Mentions,
) -> Result<bool> {
    ensure!(segments.len() <= MAX_SEGMENTS, "消息段数量超过上限");
    let mut ranges = Vec::new();
    let mut cursor = 0;
    for segment in segments {
        let kind = segment
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .ok_or_else(|| anyhow!("原生消息段类型无效"))?;
        let data = segment.get("data").unwrap_or(&Value::Null);
        let part = match kind {
            "text" => data
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("原生消息段文本无效"))?
                .to_owned(),
            "at" => format!(
                "@{}",
                exact_id(data.get("qq").ok_or_else(|| anyhow!("投影提及缺少 ID"))?)?
            ),
            "image" => "[图片]".into(),
            "reply" => "[回复]".into(),
            "file" => format!(
                "[文件:{}]",
                data.get("name").and_then(Value::as_str).unwrap_or("")
            ),
            "record" => "[语音]".into(),
            "video" => "[视频]".into(),
            "markdown" => data
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            "template_markdown" => "[模板 Markdown]".into(),
            "button" => data
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            "node" => "[转发消息]".into(),
            _ => format!("[{kind}]"),
        };
        if !text[cursor..].starts_with(&part) {
            return Ok(false);
        }
        if kind == "at" {
            let id = exact_id(data.get("qq").ok_or_else(|| anyhow!("投影提及缺少 ID"))?)?;
            if id != "all" && ids.contains(&id) {
                ranges.push(cursor..cursor + part.len());
            }
        }
        cursor += part.len();
    }
    if cursor != text.len() {
        return Ok(false);
    }
    mentions.ranges.extend(ranges);
    Ok(true)
}

fn utf16_range(text: &str, offset: u64, length: u64) -> Result<Range<usize>> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| anyhow!("Telegram 提及范围溢出"))?;
    let mut units = 0;
    let mut start_byte = None;
    let mut end_byte = None;
    for (byte, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), '\0')))
    {
        if units == offset {
            start_byte = Some(byte);
        }
        if units == end {
            end_byte = Some(byte);
            break;
        }
        units += u64::from(ch.len_utf16() as u32);
    }
    match (start_byte, end_byte) {
        (Some(start), Some(end)) if start < end => Ok(start..end),
        _ => bail!("Telegram 提及范围越界或截断 UTF-16 字符"),
    }
}

fn unique_substring(text: &str, part: &str) -> Result<usize> {
    ensure!(!part.is_empty(), "消息片段为空，无法确认提及位置");
    let start = text
        .find(part)
        .ok_or_else(|| anyhow!("消息片段不属于原消息文本"))?;
    let next = start + part.chars().next().expect("nonempty fragment").len_utf8();
    ensure!(
        !text[next..].contains(part),
        "消息片段在原消息中不唯一，无法确认提及位置"
    );
    Ok(start)
}

fn strip_ranges(text: &str, ranges: &[Range<usize>]) -> Result<String> {
    let mut result = String::with_capacity(text.len());
    let mut end = 0;
    for range in ranges {
        ensure!(
            range.start >= end
                && range.start < range.end
                && range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end),
            "原生提及文本范围无效"
        );
        result.push_str(&text[end..range.start]);
        end = range.end;
    }
    result.push_str(&text[end..]);
    Ok(result)
}

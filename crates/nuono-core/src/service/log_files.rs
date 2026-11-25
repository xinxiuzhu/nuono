//! 固定日志目录的只读、脱敏文件分页；不接受用户提供的目录。
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{NuonoError, Result as NuonoResult};
use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::telemetry::{self, FileLogSettings, is_log_file_name};

const MAX_SCAN_BYTES: u64 = 1024 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_QUERY_BYTES: usize = 256;
const SKIPPED_LINE: &str = "[该日志行过长或仅剩部分内容，已跳过；可继续分段查看其他日志]";

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LogFileInfo {
    pub name: String,
    pub size_bytes: u64,
    pub modified_at: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LogFileList {
    pub enabled: bool,
    pub retention_days: u64,
    pub files: Vec<LogFileInfo>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LogFileLine {
    /// 行起点的字节偏移；超长行跳过提示使用本段起点。
    pub offset: u64,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LogFilePage {
    pub name: String,
    pub size_bytes: u64,
    /// 本段按文件顺序由旧到新排列。
    pub lines: Vec<LogFileLine>,
    /// 向前继续扫描的 exclusive 字节位置。空命中页仍可能有下一段。
    pub next_before: Option<u64>,
    /// 上下文模式下向后继续读取的行起点。
    pub next_after: Option<u64>,
    pub scanned_bytes: u64,
}

pub async fn list_files() -> NuonoResult<LogFileList> {
    let settings = telemetry::file_log_settings()
        .ok_or_else(|| NuonoError::Internal("日志系统尚未初始化".into()))?;
    tokio::task::spawn_blocking(move || list_in(&settings))
        .await
        .map_err(|error| NuonoError::Internal(error.to_string()))?
        .map_err(service_error)
}

pub async fn read_content(
    name: String,
    before: Option<u64>,
    limit: usize,
    query: Option<String>,
    around: Option<u64>,
    plugin: Option<String>,
) -> NuonoResult<LogFilePage> {
    let settings = telemetry::file_log_settings()
        .ok_or_else(|| NuonoError::Internal("日志系统尚未初始化".into()))?;
    tokio::task::spawn_blocking(move || {
        if let Some(offset) = around {
            if before.is_some()
                || query.as_deref().is_some_and(|q| !q.trim().is_empty())
                || plugin.as_deref().is_some_and(|p| !p.trim().is_empty())
            {
                bail!(NuonoError::Validation(
                    "上下文定位不能同时指定分页游标或筛选条件".into()
                ));
            }
            read_context_in(&settings.directory, &name, offset, limit)
        } else {
            read_in(
                &settings.directory,
                &name,
                before,
                limit,
                query.as_deref(),
                plugin.as_deref(),
            )
        }
    })
    .await
    .map_err(|error| NuonoError::Internal(error.to_string()))?
    .map_err(service_error)
}

fn service_error(error: anyhow::Error) -> NuonoError {
    error
        .downcast::<NuonoError>()
        .unwrap_or_else(|error| NuonoError::Internal(error.to_string()))
}
fn requested_file_error(error: std::io::Error) -> NuonoError {
    if error.kind() == std::io::ErrorKind::NotFound {
        NuonoError::NotFound("日志文件不存在".into())
    } else {
        NuonoError::Internal(error.to_string())
    }
}

fn open_directory(root: &Path) -> Result<File> {
    // 目录在 init 时已解析；若之后被换成软链接，则停止读取而非跟随新目标。
    if root.canonicalize().context("日志目录不可用")? != root {
        bail!("日志目录已变化，请重启后重试");
    }
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, open};
        let directory = open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("无法打开日志目录")?;
        Ok(File::from(directory))
    }
    #[cfg(not(unix))]
    {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // 打开目录并禁止跟随符号链接或重解析点（FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT）。
            options.custom_flags(0x02000000 | 0x00200000);
        }
        let directory = options.open(root).context("无法打开日志目录")?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            bail!("日志目录无效");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                bail!("日志目录已变化，请重启后重试");
            }
        }
        Ok(directory)
    }
}

fn open_regular(root: &Path, directory: &File, name: &str) -> Result<File> {
    if !is_log_file_name(name) {
        bail!(NuonoError::Validation("日志文件名无效".into()));
    }
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, openat};
        let _ = root;
        File::from(
            openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| requested_file_error(std::io::Error::from(error)))?,
        )
    };
    #[cfg(not(unix))]
    let file = {
        let _ = directory;
        let path = root.join(name);
        let metadata = std::fs::symlink_metadata(&path).map_err(requested_file_error)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!(NuonoError::Validation("只允许读取常规日志文件".into()));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        options.open(path).map_err(requested_file_error)?
    };
    let metadata = file.metadata().context("无法读取日志文件信息")?;
    if !metadata.is_file() {
        bail!(NuonoError::Validation("只允许读取常规日志文件".into()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            // Windows 重解析点属性（FILE_ATTRIBUTE_REPARSE_POINT，即符号链接或目录联接）
            bail!(NuonoError::Validation("不允许读取日志软链接".into()));
        }
    }
    Ok(file)
}

fn list_in(settings: &FileLogSettings) -> Result<LogFileList> {
    let mut output = LogFileList {
        enabled: settings.enabled,
        retention_days: settings.retention_days,
        files: vec![],
    };
    // 尚未创建目录（包括关闭文件输出时）属于正常空状态。
    if !settings
        .directory
        .try_exists()
        .context("无法检查日志目录")?
    {
        return Ok(output);
    }
    let directory = open_directory(&settings.directory)?;
    for entry in std::fs::read_dir(&settings.directory).context("无法列出日志文件")? {
        let entry = entry.context("无法读取日志目录条目")?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_log_file_name(&name) {
            continue;
        }
        // 清理/滚动与列表可同时发生；消失的文件以及软链接、特殊文件均不展示。
        let Ok(file) = open_regular(&settings.directory, &directory, &name) else {
            continue;
        };
        let metadata = file.metadata().context("无法读取日志文件信息")?;
        output.files.push(LogFileInfo {
            name,
            size_bytes: metadata.len(),
            modified_at: metadata
                .modified()
                .ok()
                .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()),
        });
    }
    output.files.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| b.name.cmp(&a.name))
    });
    Ok(output)
}

fn read_in(
    root: &Path,
    name: &str,
    before: Option<u64>,
    limit: usize,
    query: Option<&str>,
    plugin: Option<&str>,
) -> Result<LogFilePage> {
    if !is_log_file_name(name) {
        bail!(NuonoError::Validation("日志文件名无效".into()));
    }
    if !(1..=500).contains(&limit) {
        bail!(NuonoError::Validation(
            "每页行数必须在 1 到 500 之间".into()
        ));
    }
    let query = query.unwrap_or("").trim();
    if query.len() > MAX_QUERY_BYTES {
        bail!(NuonoError::Validation("搜索关键字不能超过 256 字节".into()));
    }
    let plugin = plugin.unwrap_or("").trim();
    if plugin.len() > MAX_QUERY_BYTES {
        bail!(NuonoError::Validation("插件名称不能超过 256 字节".into()));
    }
    let directory = open_directory(root)?;
    let mut file = open_regular(root, &directory, name)?;
    let size_bytes = file.metadata()?.len();
    read_before(&mut file, name, size_bytes, before, limit, query, plugin)
}

/// 只使用显式归属；消息或模块名里提到插件不代表该插件产生了日志。
fn attributed_plugin(value: &serde_json::Value) -> Option<&str> {
    fn field(value: &serde_json::Value) -> Option<&str> {
        [
            "/fields/plugin",
            "/fields/plugin_name",
            "/plugin",
            "/plugin_name",
            "/fields/fields_json/plugin",
            "/fields/fields_json/plugin_name",
        ]
        .into_iter()
        .find_map(|path| {
            value
                .pointer(path)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
        })
    }
    field(value)
        .or_else(|| value.get("span").and_then(field))
        .or_else(|| {
            value
                .get("spans")
                .and_then(serde_json::Value::as_array)
                .and_then(|spans| spans.iter().rev().find_map(field))
        })
}

fn matches_plugin(text: &str, plugin: &str) -> bool {
    plugin.is_empty()
        || serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .is_some_and(|value| attributed_plugin(&value) == Some(plugin))
}

fn read_before(
    file: &mut File,
    name: &str,
    size_bytes: u64,
    before: Option<u64>,
    limit: usize,
    query: &str,
    plugin: &str,
) -> Result<LogFilePage> {
    let end = before.unwrap_or(size_bytes);
    if end > size_bytes {
        bail!(NuonoError::Conflict(
            "日志文件已缩短或游标无效，请刷新文件后重试".into()
        ));
    }
    let mut page = LogFilePage {
        name: name.into(),
        size_bytes,
        lines: vec![],
        next_before: None,
        next_after: None,
        scanned_bytes: 0,
    };
    if end == 0 || limit == 0 {
        page.next_before = (end > 0).then_some(end);
        return Ok(page);
    }
    let start = end.saturating_sub(MAX_SCAN_BYTES);
    // 多读一个字节，用于判定扫描窗口是否从完整行开始。
    let read_start = start.saturating_sub(1);
    file.seek(SeekFrom::Start(read_start))?;
    let mut bytes = vec![0; (end - read_start) as usize];
    file.read_exact(&mut bytes)
        .context("日志文件读取期间已变化，请刷新后重试")?;
    if file.metadata()?.len() < end {
        bail!(NuonoError::Conflict(
            "日志文件读取期间已缩短，请刷新后重试".into()
        ));
    }
    page.scanned_bytes = bytes.len() as u64;
    let partial_start = start > 0 && bytes[0] != b'\n';
    let bytes = if start > 0 { &bytes[1..] } else { &bytes[..] };
    // 从后向前查找换行，只保留当前行；空行很多时也无需建立整段行索引。
    let mut cursor = bytes.len();
    let mut next = end;
    while cursor > 0 {
        let terminated = bytes[cursor - 1] == b'\n';
        let to = cursor - usize::from(terminated);
        let from = bytes[..to]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        cursor = from;
        if from == 0 && partial_start && to <= MAX_LINE_BYTES && terminated {
            // 窗口左侧普通行的残片留到下一页完整读取，不能截短或重复展示。
            next = start + to as u64 + 1;
            break;
        }
        next = start + from as u64;
        let partial_end = !terminated && end < size_bytes;
        let oversized = to - from > MAX_LINE_BYTES || (from == 0 && partial_start) || partial_end;
        let (text, truncated) = if oversized {
            (SKIPPED_LINE.to_string(), true)
        } else {
            let raw = std::str::from_utf8(&bytes[from..to]);
            match raw {
                Ok(raw) => telemetry::sanitize_file_line(raw.trim_suffix('\r')),
                Err(_) => ("[该日志行编码不完整或无效，内容已隐藏]".to_string(), true),
            }
        };
        if (query.is_empty() || text.contains(query)) && matches_plugin(&text, plugin) {
            page.lines.push(LogFileLine {
                offset: start + from as u64,
                text,
                truncated,
            });
            if page.lines.len() == limit {
                break;
            }
        }
    }
    page.lines.reverse();
    page.next_before = (next > 0).then_some(next);
    Ok(page)
}

fn read_context_in(root: &Path, name: &str, offset: u64, limit: usize) -> Result<LogFilePage> {
    if !(1..=500).contains(&limit) {
        bail!(NuonoError::Validation(
            "每页行数必须在 1 到 500 之间".into()
        ));
    }
    let directory = open_directory(root)?;
    let mut file = open_regular(root, &directory, name)?;
    let size_bytes = file.metadata()?.len();
    if offset >= size_bytes {
        bail!(NuonoError::Conflict(
            "目标日志已不存在或文件已缩短，请重新搜索".into()
        ));
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut preceding = [0];
        file.read_exact(&mut preceding)?;
        if preceding[0] != b'\n' {
            bail!(NuonoError::Validation(
                "目标位置不是完整日志的起点，请重新搜索".into()
            ));
        }
    }
    // 同一文件句柄读取两侧，每侧最多扫描约 1 MiB；目标包含在后半段。
    let mut page = read_before(&mut file, name, size_bytes, Some(offset), limit / 2, "", "")?;
    file.seek(SeekFrom::Start(offset))?;
    let length = (size_bytes - offset).min(MAX_SCAN_BYTES) as usize;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)
        .context("日志文件读取期间已变化，请重新搜索")?;
    page.scanned_bytes += length as u64;
    let mut cursor = 0;
    while cursor < length && page.lines.len() < limit {
        let newline = bytes[cursor..].iter().position(|byte| *byte == b'\n');
        let end = newline.map_or(length, |index| cursor + index);
        let partial = newline.is_none() && offset + (length as u64) < size_bytes;
        // 普通残片留给下一段；目标行过长时只展示占位，不回显残片。
        if partial && cursor > 0 {
            break;
        }
        let (text, truncated) = if partial || end - cursor > MAX_LINE_BYTES {
            (SKIPPED_LINE.to_string(), true)
        } else {
            match std::str::from_utf8(&bytes[cursor..end]) {
                Ok(raw) => telemetry::sanitize_file_line(raw.trim_suffix('\r')),
                Err(_) => ("[该日志行编码不完整或无效，内容已隐藏]".to_string(), true),
            }
        };
        page.lines.push(LogFileLine {
            offset: offset + cursor as u64,
            text,
            truncated,
        });
        if partial {
            cursor = length;
            break;
        }
        cursor = end + usize::from(newline.is_some());
    }
    if offset + (cursor as u64) < size_bytes && cursor > 0 && bytes[cursor - 1] == b'\n' {
        page.next_after = Some(offset + cursor as u64);
    }
    if file.metadata()?.len() < size_bytes {
        bail!(NuonoError::Conflict(
            "日志文件读取期间已缩短，请重新搜索".into()
        ));
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    struct TempLogs(PathBuf);
    impl TempLogs {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("nuono-log-files-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn write(&self, content: &str) {
            std::fs::write(self.0.join("2026-10-03.log"), content).unwrap();
        }
        fn read(&self, before: Option<u64>, limit: usize, query: Option<&str>) -> LogFilePage {
            read_in(&self.0, "2026-10-03.log", before, limit, query, None).unwrap()
        }
    }
    impl Drop for TempLogs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn log_file_errors_distinguish_input_missing_cursor_conflict_and_io() {
        let logs = TempLogs::new();
        logs.write("x");
        assert!(matches!(
            service_error(read_in(&logs.0, "../secret", None, 10, None, None).unwrap_err()),
            NuonoError::Validation(_)
        ));
        assert!(matches!(
            service_error(read_in(&logs.0, "2026-10-02.log", None, 10, None, None).unwrap_err()),
            NuonoError::NotFound(_)
        ));
        assert!(matches!(
            service_error(
                read_in(&logs.0, "2026-10-03.log", Some(100), 10, None, None).unwrap_err()
            ),
            NuonoError::Conflict(_)
        ));
        assert!(matches!(
            requested_file_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            NuonoError::Internal(_)
        ));
    }

    #[test]
    fn plugin_filter_uses_explicit_fields_and_spans_with_exact_names() {
        let logs = TempLogs::new();
        let values = [
            serde_json::json!({"fields":{"plugin":"Demo","message":"retry","token":"hidden"}}),
            serde_json::json!({"fields":{"plugin_name":"Demo","message":"ready"}}),
            serde_json::json!({"plugin":"Demo","message":"retry"}),
            serde_json::json!({"fields":{"fields_json":r#"{"plugin_name":"Demo"}"#,"message":"retry"}}),
            serde_json::json!({"span":{"plugin":"Demo"},"fields":{"message":"retry"}}),
            serde_json::json!({"spans":[{"plugin":"Other"},{"plugin_name":"Demo"}],"fields":{"message":"retry"}}),
            serde_json::json!({"fields":{"plugin":"Other","message":"Demo retry"},"span":{"plugin":"Demo"}}),
            serde_json::json!({"fields":{"plugin":"DemoExtra","message":"retry"}}),
            serde_json::json!({"target":"Demo::handler","fields":{"message":"Demo retry"}}),
            serde_json::json!({"fields":{"plugin":"demo","message":"retry"}}),
        ];
        logs.write(
            &values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let page = read_in(&logs.0, "2026-10-03.log", None, 200, None, Some(" Demo ")).unwrap();
        assert_eq!(page.lines.len(), 6);
        assert!(!page.lines[0].text.contains("hidden"));
        let combined = read_in(
            &logs.0,
            "2026-10-03.log",
            None,
            200,
            Some("retry"),
            Some("Demo"),
        )
        .unwrap();
        assert_eq!(combined.lines.len(), 5);
        assert!(
            read_in(&logs.0, "2026-10-03.log", None, 200, None, Some("Missing"))
                .unwrap()
                .lines
                .is_empty()
        );
        assert!(
            read_in(
                &logs.0,
                "2026-10-03.log",
                None,
                200,
                None,
                Some(&"x".repeat(257))
            )
            .is_err()
        );
        let hit = &combined.lines[0];
        let context = read_context_in(&logs.0, "2026-10-03.log", hit.offset, 3).unwrap();
        assert!(context.lines.iter().any(|line| line.text.contains("ready")));
    }

    #[test]
    fn plugin_search_advances_bounded_cursors_and_preserves_filtered_pagination() {
        let logs = TempLogs::new();
        let hit = r#"{"fields":{"plugin":"Demo","message":"old hit"}}"#;
        logs.write(&format!(
            "{hit}\n{}\nplain mentions Demo\n",
            "x".repeat(MAX_SCAN_BYTES as usize + 100)
        ));
        let first = read_in(&logs.0, "2026-10-03.log", None, 200, None, Some("Demo")).unwrap();
        assert!(first.lines.is_empty());
        let mut before = first.next_before;
        let mut found = vec![];
        while let Some(cursor) = before {
            let page = read_in(&logs.0, "2026-10-03.log", before, 1, None, Some("Demo")).unwrap();
            assert!(page.scanned_bytes <= MAX_SCAN_BYTES + 1);
            assert!(page.next_before.is_none_or(|next| next < cursor));
            found.extend(page.lines.into_iter().map(|line| line.text));
            before = page.next_before;
        }
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("old hit"));
        logs.write(&format!("{hit}\nplain\n{hit}\nplain\n{hit}\n"));
        let mut offsets = vec![];
        let mut before = None;
        loop {
            let page = read_in(&logs.0, "2026-10-03.log", before, 1, None, Some("Demo")).unwrap();
            offsets.extend(page.lines.into_iter().map(|line| line.offset));
            before = page.next_before;
            if before.is_none() {
                break;
            }
        }
        assert_eq!(offsets.len(), 3);
        assert!(offsets.windows(2).all(|pair| pair[0] > pair[1]));
    }

    #[test]
    fn context_contains_the_exact_search_hit_and_unfiltered_neighbors() {
        let logs = TempLogs::new();
        let content = "第一条\r\n\ncontext before\nunique hit\ncontext after\nlast";
        logs.write(content);
        let hit = logs.read(None, 200, Some("unique hit")).lines.remove(0);
        let page = read_context_in(&logs.0, "2026-10-03.log", hit.offset, 3).unwrap();
        assert_eq!(
            page.lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["context before", "unique hit", "context after"]
        );
        assert_eq!(page.lines[1].offset, hit.offset);
        let older = logs.read(page.next_before, 10, None);
        assert_eq!(
            older
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["第一条", ""]
        );
        let newer =
            read_context_in(&logs.0, "2026-10-03.log", page.next_after.unwrap(), 1).unwrap();
        assert_eq!(newer.lines[0].text, "last");
        assert!(newer.next_after.is_none());
        let first = read_context_in(&logs.0, "2026-10-03.log", 0, 3).unwrap();
        assert_eq!(first.lines[0].text, "第一条");
        assert!(first.next_before.is_none());
    }

    #[test]
    fn context_preserves_redaction_and_rejects_changed_or_invalid_positions() {
        let logs = TempLogs::new();
        logs.write("start\npassword=hidden credential\nend");
        let page = read_context_in(&logs.0, "2026-10-03.log", 6, 3).unwrap();
        assert_eq!(page.lines[1].offset, 6);
        assert!(page.lines[1].text.contains("[REDACTED]"));
        assert!(!page.lines[1].text.contains("hidden"));
        for offset in [1, 5, 100, u64::MAX] {
            assert!(read_context_in(&logs.0, "2026-10-03.log", offset, 3).is_err());
        }
        for limit in [0, 501, usize::MAX] {
            assert!(read_context_in(&logs.0, "2026-10-03.log", 6, limit).is_err());
        }
        logs.write("short");
        assert!(read_context_in(&logs.0, "2026-10-03.log", 6, 3).is_err());
    }

    #[test]
    fn context_scan_is_bounded_for_large_records_and_preserves_complete_lines() {
        let logs = TempLogs::new();
        logs.write(&format!(
            "{}\ntarget\n{}\nend",
            "a".repeat(MAX_SCAN_BYTES as usize + 20),
            "b".repeat(MAX_SCAN_BYTES as usize + 20)
        ));
        let offset = MAX_SCAN_BYTES + 21;
        let page = read_context_in(&logs.0, "2026-10-03.log", offset, 200).unwrap();
        assert!(page.scanned_bytes <= 2 * MAX_SCAN_BYTES + 1);
        assert!(
            page.lines
                .iter()
                .any(|line| line.offset == offset && line.text == "target")
        );
        assert!(
            page.lines
                .iter()
                .all(|line| line.truncated || line.text == "target")
        );
        let next =
            read_context_in(&logs.0, "2026-10-03.log", page.next_after.unwrap(), 200).unwrap();
        assert!(next.lines.last().unwrap().truncated);
    }

    #[test]
    fn backwards_pages_preserve_lines_offsets_and_unterminated_unicode_tail() {
        let logs = TempLogs::new();
        for content in ["甲\n乙\n丙", "甲\n乙\n丙\n", "甲\r\n\r\n丙\r\n", "\n", ""] {
            logs.write(content);
            let mut before = None;
            let mut actual = Vec::new();
            loop {
                let page = logs.read(before, 1, None);
                assert!(page.scanned_bytes <= MAX_SCAN_BYTES + 1);
                let mut older: Vec<_> = page
                    .lines
                    .into_iter()
                    .map(|line| (line.offset, line.text))
                    .collect();
                older.extend(actual);
                actual = older;
                match page.next_before {
                    Some(next) => {
                        assert!(next < before.unwrap_or(content.len() as u64));
                        before = Some(next);
                    }
                    None => break,
                }
            }
            let mut offset = 0;
            let expected: Vec<_> = content
                .split_inclusive('\n')
                .map(|line| {
                    let item = (
                        offset,
                        line.trim_end_matches('\n')
                            .trim_end_matches('\r')
                            .to_string(),
                    );
                    offset += line.len() as u64;
                    item
                })
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn scan_window_defers_partial_normal_lines_without_loss_or_duplicates() {
        let logs = TempLogs::new();
        let expected: Vec<_> = (0..650)
            .map(|n| format!("第{n}行{}", "中".repeat(1300)))
            .collect();
        logs.write(&expected.join("\n"));
        let mut actual = vec![];
        let mut before = None;
        loop {
            let page = logs.read(before, 500, None);
            assert!(page.scanned_bytes <= MAX_SCAN_BYTES + 1);
            assert!(page.lines.iter().all(|line| !line.truncated));
            let mut older: Vec<_> = page.lines.into_iter().map(|line| line.text).collect();
            older.extend(actual);
            actual = older;
            before = page.next_before;
            if before.is_none() {
                break;
            }
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn search_can_return_empty_page_and_always_advances_over_huge_lines() {
        let logs = TempLogs::new();
        logs.write(&format!("较早匹配\n{}\n末尾\n", "密".repeat(800_000)));
        let first = logs.read(None, 200, Some("较早匹配"));
        assert!(first.lines.is_empty());
        assert!(first.next_before.is_some());
        let mut before = first.next_before;
        let mut matched = vec![];
        for _ in 0..10 {
            let page = logs.read(before, 200, Some("较早匹配"));
            assert!(page.scanned_bytes <= MAX_SCAN_BYTES + 1);
            matched.extend(page.lines.into_iter().map(|line| line.text));
            if let Some(next) = page.next_before {
                assert!(next < before.unwrap());
            }
            before = page.next_before;
            if before.is_none() {
                break;
            }
        }
        assert!(before.is_none());
        assert_eq!(matched, ["较早匹配"]);
        let initial = logs.read(None, 200, None);
        assert!(
            initial
                .lines
                .iter()
                .any(|line| line.truncated && line.text == SKIPPED_LINE)
        );
        assert!(initial.lines.iter().all(|line| !line.text.contains('密')));
    }

    #[test]
    fn append_keeps_older_cursor_stable_and_truncation_rejects_out_of_bounds() {
        let logs = TempLogs::new();
        logs.write("first\nsecond\nthird");
        let first = logs.read(None, 1, None);
        let before = first.next_before;
        std::fs::OpenOptions::new()
            .append(true)
            .open(logs.0.join("2026-10-03.log"))
            .unwrap()
            .write_all(b" appended\nfourth\n")
            .unwrap();
        let older = logs.read(before, 10, None);
        assert_eq!(
            older
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        logs.write("x");
        assert!(
            read_in(&logs.0, "2026-10-03.log", before, 10, None, None)
                .unwrap_err()
                .to_string()
                .contains("缩短")
        );
    }

    #[test]
    fn directory_listing_supports_disabled_output_and_missing_directory() {
        let logs = TempLogs::new();
        logs.write("history\n");
        for name in [
            "other.log",
            "nuono.log",
            "nuono.log.2026-10-03",
            "nuono.log.old",
            "2026-02-30.log",
            "2026-10-02.log",
        ] {
            std::fs::write(logs.0.join(name), "test").unwrap();
        }
        std::fs::create_dir(logs.0.join("2026-10-01.log")).unwrap();
        let mut settings = FileLogSettings {
            enabled: false,
            retention_days: 7,
            directory: logs.0.clone(),
        };
        let list = list_in(&settings).unwrap();
        assert!(!list.enabled);
        assert_eq!(list.retention_days, 7);
        assert_eq!(list.files.len(), 2);
        assert!(list.files.iter().all(|file| is_log_file_name(&file.name)));
        settings.directory = logs.0.join("missing");
        assert!(list_in(&settings).unwrap().files.is_empty());
    }

    #[test]
    fn rejects_paths_invalid_limits_and_oversized_queries() {
        let logs = TempLogs::new();
        logs.write("test");
        for name in [
            "nuono.log",
            "nuono.log.2026-10-03",
            "2026-02-30.log",
            "2026-10-03.other.log",
            "../2026-10-03.log",
            "/2026-10-03.log",
            "sub/../2026-10-03.log",
            "sub\\2026-10-03.log",
            "2026-1-2.log",
            "2026-10-03.log\0",
        ] {
            assert!(
                read_in(&logs.0, name, None, 200, None, None).is_err(),
                "{name}"
            );
        }
        for limit in [0, 501, usize::MAX] {
            assert!(read_in(&logs.0, "2026-10-03.log", None, limit, None, None).is_err());
        }
        assert!(read_in(&logs.0, "2026-10-03.log", Some(5), 200, None, None).is_err());
        assert!(
            read_in(
                &logs.0,
                "2026-10-03.log",
                None,
                200,
                Some(&"x".repeat(257)),
                None
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_special_files_without_blocking() {
        let logs = TempLogs::new();
        let target = logs.0.join("outside.txt");
        std::fs::write(&target, "hidden").unwrap();
        std::os::unix::fs::symlink(&target, logs.0.join("2026-10-03.log")).unwrap();
        assert!(read_in(&logs.0, "2026-10-03.log", None, 200, None, None).is_err());
        std::fs::remove_file(logs.0.join("2026-10-03.log")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(logs.0.join("2026-10-03.log"))
                .status()
                .unwrap()
                .success()
        );
        assert!(read_in(&logs.0, "2026-10-03.log", None, 200, None, None).is_err());
        let list = list_in(&FileLogSettings {
            enabled: true,
            retention_days: 7,
            directory: logs.0.clone(),
        })
        .unwrap();
        assert!(list.files.is_empty());
    }

    #[test]
    fn file_content_redacts_recursive_json_and_filters_only_sanitized_text() {
        let logs = TempLogs::new();
        let value = serde_json::json!({"fields":{"api_key":"hidden-key", "nested":[{"password":"hidden-password"}], "fields_json": r#"{"token":"hidden-token","okay":true}"#},"message":"请求 Bearer hidden-bearer"});
        logs.write(&format!("{value}\n"));
        let page = logs.read(None, 200, None);
        let text = &page.lines[0].text;
        assert!(text.contains("REDACTED"));
        for value in [
            "hidden-key",
            "hidden-password",
            "hidden-token",
            "hidden-bearer",
        ] {
            assert!(!text.contains(value));
        }
        assert!(logs.read(None, 200, Some("hidden-key")).lines.is_empty());
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["fields"]["fields_json"]["okay"], true);
    }

    #[test]
    fn file_content_redacts_quoted_text_fields_ansi_and_broken_json() {
        let logs = TempLogs::new();
        logs.write(concat!(
            "message password=\"hello world\" api_key='secret spaces'\n",
            "Authorization: Basic dXNlcjpwYXNz\n",
            "api_token=hidden-api refresh_token=hidden-refresh user_password=hidden-user\n",
            "body=\"{\\\"api_key\\\":\\\"debug-body-key\\\"}\"\n",
            "Cookie: first=first-cookie; second=second-cookie\n",
            "token=Some(\"some secret\") later=okay\n",
            "password={\"value\":\"debug secret\"}\n",
            "message fields_json=\"{\\\"api_key\\\":\\\"hidden-key\\\"}\"\n",
            "message \u{1b}[3mpassword\u{1b}[0m=\"ansi-secret\"\n",
            "{\"password\":\"broken-secret\n"
        ));
        let page = logs.read(None, 200, None);
        let combined = page
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for hidden in [
            "hello",
            "world",
            "secret spaces",
            "hidden-key",
            "ansi-secret",
            "broken-secret",
            "dXNlcjpwYXNz",
            "hidden-api",
            "hidden-refresh",
            "hidden-user",
            "debug-body-key",
            "first-cookie",
            "second-cookie",
            "some secret",
            "debug secret",
            "\u{1b}",
        ] {
            assert!(!combined.contains(hidden), "{hidden}");
        }
        assert!(page.lines.last().unwrap().truncated);
    }
}

//! 受大小限制的增量 SSE 流解码器，供 Chat Completions 与 Responses 两种协议共用。
use super::{AiError, ChatStreamChunk, request_error};
use futures::StreamExt;
use nuono_ai::usage::UsageOutcome;
use tokio::sync::mpsc;

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

struct Decoder {
    line: Vec<u8>,
    data: String,
    skip_lf: bool,
    first_line: bool,
    event_bytes: usize,
    limit: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data: String::new(),
            skip_lf: false,
            first_line: true,
            event_bytes: 0,
            limit: MAX_EVENT_BYTES,
        }
    }
}

impl Decoder {
    // 仅在凑齐整行后再做 UTF-8 解码，防止多字节字符被网络分包截断。
    // 遇到 `\r` 时立即触发换行，并在后续字节中跳过紧随其后的 `\n`（兼容跨分包的 CRLF）。
    fn push(&mut self, byte: u8) -> Result<Option<String>, AiError> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        if self.event_bytes == self.limit {
            return Err(AiError::ResponseParseFailed(
                "AI 流事件超过 8 MiB 上限".into(),
            ));
        }
        self.event_bytes += 1;
        if !matches!(byte, b'\r' | b'\n') {
            self.line.push(byte);
            return Ok(None);
        }
        self.skip_lf = byte == b'\r';
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| AiError::ResponseParseFailed("AI 流包含无效 UTF-8".into()))?;
        let line = if self.first_line {
            self.first_line = false;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        let event = if line.is_empty() {
            self.event_bytes = 0;
            if self.data.is_empty() {
                None
            } else {
                self.data.pop(); // 每条 data 行末尾都会追加一个换行符，派发事件前去掉最后一个多余换行。
                Some(std::mem::take(&mut self.data))
            }
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
                self.data.push('\n');
            }
            None
        };
        self.line.clear();
        Ok(event)
    }
}

pub(super) enum Event {
    Chunk(ChatStreamChunk),
    Done,
    Ignore,
}

pub(super) async fn forward(
    response: reqwest::Response,
    tx: mpsc::Sender<Result<ChatStreamChunk, AiError>>,
    mut parse: impl FnMut(&str) -> Result<Event, AiError>,
    premature_end: &'static str,
    drain_usage_footer: bool,
) -> UsageOutcome {
    let mut stream = response.bytes_stream();
    let mut tx = Some(tx);
    let mut decoder = Decoder::default();
    let mut finished = false;
    let mut footer_deadline = None;
    loop {
        let part = tokio::select! {
            biased;
            _ = async {
                if let Some(tx) = &tx { tx.closed().await; }
                else { futures::future::pending::<()>().await; }
            }, if !finished => return UsageOutcome::Cancelled,
            _ = async { if let Some(deadline) = footer_deadline {tokio::time::sleep_until(deadline).await;} else {futures::future::pending::<()>().await;} } => return UsageOutcome::Succeeded,
            part = stream.next() => part,
        };
        let bytes = match part {
            Some(Ok(bytes)) => bytes,
            Some(Err(error)) => {
                if let Some(tx) = &tx {
                    let _ = tx.send(Err(request_error(error))).await;
                }
                return if finished {
                    UsageOutcome::Succeeded
                } else {
                    UsageOutcome::Failed
                };
            }
            None => {
                if finished {
                    return UsageOutcome::Succeeded;
                }
                if let Some(tx) = &tx {
                    let _ = tx
                        .send(Err(AiError::RequestFailed(premature_end.into())))
                        .await;
                }
                return UsageOutcome::Failed;
            }
        };
        for byte in bytes {
            let event = match decoder
                .push(byte)
                .and_then(|data| data.map(|data| parse(&data)).unwrap_or(Ok(Event::Ignore)))
            {
                Ok(event) => event,
                Err(error) => {
                    if let Some(tx) = &tx {
                        let _ = tx.send(Err(error)).await;
                    }
                    return if finished {
                        UsageOutcome::Succeeded
                    } else {
                        UsageOutcome::Failed
                    };
                }
            };
            match event {
                Event::Chunk(chunk) => {
                    let terminal = chunk.finish_reason.is_some();
                    if let Some(tx) = &tx
                        && tx.send(Ok(chunk)).await.is_err()
                    {
                        return UsageOutcome::Cancelled;
                    }
                    if terminal {
                        if !drain_usage_footer {
                            return UsageOutcome::Succeeded;
                        }
                        finished = true;
                        // 立即关闭面向调用方的输出通道，后台任务仅继续读取流尾部的用量信息。
                        // 接收端拿到回复内容后即可结束读取，无需等待流尾部的用量统计事件。
                        tx.take();
                        // 最终的 token 用量帧通常跟在 finish_reason 之后下发；
                        // 即使上层接收端已关闭，也在后台保留短暂的宽限期将其读完。
                        footer_deadline =
                            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(2));
                    }
                }
                Event::Done => return UsageOutcome::Succeeded,
                Event::Ignore => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(decoder: &mut Decoder, bytes: &[u8]) -> Result<Vec<String>, AiError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if let Some(event) = decoder.push(byte)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    #[test]
    fn multiline_data_and_unicode_survive_every_network_split() {
        let wire = "\u{feff}: comment\r\nevent: message\r\nid: 1\r\nretry: 5\r\ndata: {\r\ndata: \"content\":\"你好🙂\"}\r\n\r\n";
        for split in 0..=wire.len() {
            let mut decoder = Decoder::default();
            let mut events = decode(&mut decoder, &wire.as_bytes()[..split]).unwrap();
            events.extend(decode(&mut decoder, &wire.as_bytes()[split..]).unwrap());
            assert_eq!(events, ["{\n\"content\":\"你好🙂\"}"], "split={split}");
        }
    }

    #[test]
    fn supports_all_line_endings_including_split_crlf() {
        let mut decoder = Decoder::default();
        assert_eq!(decode(&mut decoder, b"data: a\r\r").unwrap(), ["a"]);
        assert!(decode(&mut decoder, b"\ndata: b\r").unwrap().is_empty());
        assert_eq!(decode(&mut decoder, b"\n\r").unwrap(), ["b"]);
        assert_eq!(decode(&mut decoder, b"\ndata: c\n\n").unwrap(), ["c"]);
    }

    #[test]
    fn dispatches_only_blank_line_terminated_events() {
        let mut decoder = Decoder::default();
        assert!(decode(&mut decoder, b"data: partial\n").unwrap().is_empty());
        assert_eq!(decode(&mut decoder, b"\n").unwrap(), ["partial"]);
        assert!(
            decode(&mut decoder, b"data: missing newline")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn strips_only_one_space_and_preserves_empty_data_fields() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decode(&mut decoder, b"data:  x\ndata\n\n").unwrap(),
            [" x\n"]
        );
        assert_eq!(decode(&mut decoder, b"data\n\n").unwrap(), [""]);
        assert!(
            decode(&mut decoder, b": heartbeat\nunknown: x\n\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_utf8_is_an_explicit_parse_error() {
        assert!(matches!(
            decode(&mut Decoder::default(), b"data: \xff\n\n"),
            Err(AiError::ResponseParseFailed(_))
        ));
    }

    #[test]
    fn event_limit_applies_to_unterminated_lines_and_multiline_data() {
        for wire in [
            b"data: 12345678901234567890".as_slice(),
            b"data: 123\ndata: 456\ndata: 789\n\n".as_slice(),
        ] {
            let mut decoder = Decoder {
                limit: 16,
                ..Decoder::default()
            };
            assert!(matches!(
                decode(&mut decoder, wire),
                Err(AiError::ResponseParseFailed(_))
            ));
        }
    }

    #[test]
    fn limit_resets_between_events_in_one_network_chunk() {
        let mut decoder = Decoder {
            limit: 9,
            ..Decoder::default()
        };
        assert_eq!(
            decode(&mut decoder, b"data: a\n\ndata: b\n\n").unwrap(),
            ["a", "b"]
        );
    }
}

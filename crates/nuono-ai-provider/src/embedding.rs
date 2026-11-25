//! 兼容 OpenAI 协议的向量化（Embedding）HTTP 传输层。
//! 由调用方负责选择模型、校验输入并管理 HTTP 客户端策略。
use nuono_ai::AiError;
use serde_json::{Value, json};
use std::time::Duration;

const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

fn invalid() -> AiError {
    // 接口地址、响应体和底层请求错误中可能包含敏感凭据，因此统一返回脱敏错误。
    AiError::RequestFailed("Embedding model configuration or response is invalid".into())
}

pub fn endpoint(base: &str) -> Result<reqwest::Url, AiError> {
    let base = base.trim().trim_end_matches('/');
    let base = base
        .strip_suffix("/chat/completions")
        .or_else(|| base.strip_suffix("/responses"))
        .unwrap_or(base);
    let path = if base.ends_with("/embeddings") {
        base.to_owned()
    } else {
        format!("{base}/embeddings")
    };
    let url = reqwest::Url::parse(&path).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(url)
}

/// 直接复用传入的 HTTP 客户端，不重新构建客户端或修改重定向/重试策略。
/// 调用方在发起请求前需先完成输入参数校验。
pub async fn embed(
    client: &reqwest::Client,
    base: &str,
    model: &str,
    api_key: &str,
    input: &[String],
) -> Result<Vec<Vec<f32>>, AiError> {
    embed_with_usage(client, base, model, api_key, input, None).await
}

/// 与 `embed` 相同，但支持传入回调来记录 token 用量。
pub async fn embed_with_usage(
    client: &reqwest::Client,
    base: &str,
    model: &str,
    api_key: &str,
    input: &[String],
    tracking: Option<crate::usage::UsageTracking>,
) -> Result<Vec<Vec<f32>>, AiError> {
    let url = endpoint(base)?;
    let mut attempt =
        crate::usage::Attempt::new(tracking, model, nuono_ai::usage::UsageOperation::Embedding);
    let result = async {
        let mut response = client
            .post(url)
            .bearer_auth(api_key)
            .json(&json!({"model":model,"input":input,"encoding_format":"float"}))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| invalid())?;
        if response.status() != reqwest::StatusCode::OK
            || response
                .content_length()
                .is_some_and(|bytes| bytes > RESPONSE_LIMIT as u64)
        {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
            if chunk.len() > RESPONSE_LIMIT.saturating_sub(bytes.len()) {
                return Err(invalid());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        attempt.report(crate::usage::embedding_usage(&value));
        response_vectors(value, model, input.len())
    }
    .await;
    attempt.finish(if result.is_ok() {
        nuono_ai::usage::UsageOutcome::Succeeded
    } else {
        nuono_ai::usage::UsageOutcome::Failed
    });
    result
}

fn response_vectors(value: Value, model: &str, rows: usize) -> Result<Vec<Vec<f32>>, AiError> {
    if value.get("model").and_then(Value::as_str) != Some(model) {
        return Err(invalid());
    }
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .filter(|data| data.len() == rows)
        .ok_or_else(invalid)?;
    let mut vectors = vec![None; rows];
    for item in data {
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < rows)
            .ok_or_else(invalid)?;
        if vectors[index].is_some() {
            return Err(invalid());
        }
        let values = item
            .get("embedding")
            .and_then(Value::as_array)
            .filter(|values| values.len() <= 4096)
            .ok_or_else(invalid)?;
        let vector = values
            .iter()
            .map(|value| value.as_f64().map(|value| value as f32).ok_or_else(invalid))
            .collect::<Result<Vec<_>, _>>()?;
        vectors[index] = Some(vector);
    }
    let vectors = vectors
        .into_iter()
        .map(|vector| vector.ok_or_else(invalid))
        .collect::<Result<Vec<_>, _>>()?;
    nuono_ai::embedding::validate_vectors(&vectors, rows).map_err(|_| invalid())?;
    Ok(vectors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn embedding_endpoints_normalize_existing_api_paths_and_reject_credentials() {
        for base in [
            "https://example.com/v1",
            " https://example.com/v1/ ",
            "https://example.com/v1/chat/completions",
            "https://example.com/v1/responses",
            "https://example.com/v1/embeddings",
        ] {
            assert_eq!(
                endpoint(base).unwrap().as_str(),
                "https://example.com/v1/embeddings"
            );
        }
        for base in [
            "not-a-url",
            "file:///tmp/models",
            "https://user:secret@example.com/v1",
            "https://example.com/v1?api_key=secret",
            "https://example.com/v1#secret",
        ] {
            let error = endpoint(base).unwrap_err().to_string();
            assert!(!error.contains("secret"));
            assert!(!error.contains(base));
        }
    }

    #[test]
    fn embedding_rows_use_explicit_indexes_and_reject_corrupt_batches() {
        let rows = response_vectors(json!({"model":"test","data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}), "test", 2).unwrap();
        assert_eq!(rows, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        for value in [
            json!({"model":"other","data":[{"index":0,"embedding":[1.0]}]}),
            json!({"model":"test","data":[{"index":0,"embedding":[0.0]}]}),
            json!({"model":"test","data":[{"index":1,"embedding":[1.0]}]}),
            json!({"model":"test","data":[{"index":0,"embedding":[1e100]}]}),
        ] {
            assert!(response_vectors(value, "test", 1).is_err());
        }
        assert!(response_vectors(json!({"model":"test","data":[{"index":0,"embedding":[1.0]},{"index":0,"embedding":[1.0]}]}), "test", 2).is_err());
    }

    async fn mock_response(response: String) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            request
        });
        (base, server)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn embedding_http_transport_preserves_input_order_and_float_request() {
        let body = json!({"model":"test","data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}).to_string();
        let (base, server) = mock_response(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )).await;
        let rows = embed(
            &client(),
            &base,
            "test",
            "private-key",
            &["one".into(), "two".into()],
        )
        .await
        .unwrap();
        assert_eq!(rows, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        let request = server.await.unwrap();
        let end = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        let headers = std::str::from_utf8(&request[..end])
            .unwrap()
            .to_ascii_lowercase();
        assert!(headers.starts_with("post /v1/embeddings http/1.1\r\n"));
        assert!(
            headers
                .lines()
                .any(|line| line == "authorization: bearer private-key")
        );
        let body: Value = serde_json::from_slice(&request[end + 4..]).unwrap();
        assert_eq!(
            body,
            json!({"model":"test","input":["one","two"],"encoding_format":"float"})
        );
    }

    #[tokio::test]
    async fn embedding_http_transport_rejects_redirects_and_oversized_responses() {
        for response in [
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/private-key\r\nContent-Length: 0\r\n\r\n".to_owned(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", RESPONSE_LIMIT + 1),
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 11\r\n\r\nprivate-key".to_owned(),
        ] {
            let (base, server) = mock_response(response).await;
            let error = embed(&client(), &base, "test", "private-key", &["one".into()])
                .await.unwrap_err().to_string();
            assert!(!error.contains("private-key"));
            assert!(!error.contains(&base));
            server.await.unwrap();
        }
    }
}

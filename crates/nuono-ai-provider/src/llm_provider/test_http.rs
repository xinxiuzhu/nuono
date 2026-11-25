//! 供各传输层测试共用的本地回环 HTTP 测试桩，绝不加载真实凭据。
use std::{collections::HashMap, future::Future, time::Duration};

use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub(super) struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

pub(super) struct HttpFixture {
    base_url: String,
    // 当测试断言触发 panic 或测试桩被丢弃时，JoinSet 会自动中止后台服务端任务。
    tasks: JoinSet<RecordedRequest>,
}

impl HttpFixture {
    pub async fn spawn<F, Fut>(handler: F) -> Self
    where
        F: FnOnce(TcpStream, RecordedRequest) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            tokio::time::timeout(FIXTURE_TIMEOUT, async move {
                let (mut socket, peer) = listener.accept().await.unwrap();
                assert!(peer.ip().is_loopback());
                let request = read_request(&mut socket).await;
                handler(socket, request.clone()).await;
                request
            })
            .await
            .expect("local HTTP fixture exceeded its deadline")
        });
        Self { base_url, tasks }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn finish(mut self) -> RecordedRequest {
        self.tasks
            .join_next()
            .await
            .expect("fixture task missing")
            .expect("fixture task failed")
    }
}

pub(super) async fn read_request(socket: &mut TcpStream) -> RecordedRequest {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break offset + 4;
        }
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= MAX_REQUEST_BYTES, "request too large");
    };
    let mut lines = std::str::from_utf8(&bytes[..header_end]).unwrap().lines();
    let mut request_line = lines.next().unwrap().split_ascii_whitespace();
    let method = request_line.next().unwrap().to_owned();
    let path = request_line.next().unwrap().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<HashMap<_, _>>();
    let content_length = headers["content-length"].parse::<usize>().unwrap();
    assert!(content_length <= MAX_REQUEST_BYTES - header_end);
    while bytes.len() < header_end + content_length {
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= MAX_REQUEST_BYTES, "request too large");
    }
    RecordedRequest {
        method,
        path,
        headers,
        body: serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap(),
    }
}

pub(super) async fn write_response(
    socket: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) {
    let header = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
    socket.flush().await.unwrap();
}

pub(super) async fn write_sse_headers(socket: &mut TcpStream) {
    socket
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    socket.flush().await.unwrap();
}

pub(super) async fn write_chunk(socket: &mut TcpStream, bytes: &[u8]) {
    assert!(
        !bytes.is_empty(),
        "use finish_chunks for the terminal chunk"
    );
    let mut frame = format!("{:x}\r\n", bytes.len()).into_bytes();
    frame.extend_from_slice(bytes);
    frame.extend_from_slice(b"\r\n");
    socket.write_all(&frame).await.unwrap();
    socket.flush().await.unwrap();
}

pub(super) async fn finish_chunks(socket: &mut TcpStream) {
    socket.write_all(b"0\r\n\r\n").await.unwrap();
    socket.flush().await.unwrap();
}

pub(super) async fn wait_for_client_close(socket: &mut TcpStream) {
    let mut byte = [0];
    loop {
        match socket.read(&mut byte).await {
            Ok(0) => return,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return;
            }
            Err(error) => panic!("reading client close failed: {error}"),
        }
    }
}

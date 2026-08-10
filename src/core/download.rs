//! 断点续传下载器：Range 续传、SHA-256 校验、进度回调、重试、原子落盘。

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::core::repo::Checksum;

const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
/// 读流空闲超时（大文件长时间无数据视为连接挂死）。
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 文本抓取上限（repo XML 等，防恶意超大响应）。
const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(thiserror::Error, Debug)]
pub enum DownloadError {
    #[error("SHA-256 校验失败：预期 {expected}，实际 {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("HTTP {status}")]
    Http { status: u16 },
    #[error("尺寸不符：预期 {expected} 字节，实际 {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("{source}")]
    Io { source: std::io::Error },
    #[error("{source}")]
    Network { source: reqwest::Error },
}

pub struct Downloader {
    client: reqwest::Client,
}

impl Downloader {
    pub fn new() -> anyhow::Result<Downloader> {
        let client = reqwest::Client::builder()
            .user_agent(format!("liteavd/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(READ_IDLE_TIMEOUT)
            .build()
            .context("创建 HTTP 客户端失败")?;
        Ok(Downloader { client })
    }

    /// 抓取小文本（repo XML 等）。上限 16MB，超出报错。
    pub async fn fetch_text(&self, url: &str) -> anyhow::Result<String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url} 失败"))?
            .error_for_status()
            .with_context(|| format!("GET {url} HTTP 错误"))?;
        if resp.content_length().unwrap_or(0) > MAX_TEXT_BYTES {
            anyhow::bail!("GET {url} 响应过大（>{MAX_TEXT_BYTES}B）");
        }
        let mut bytes =
            Vec::with_capacity(resp.content_length().unwrap_or(0).min(MAX_TEXT_BYTES) as usize);
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("GET {url} 读取响应失败"))?;
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .context("响应长度溢出")?;
            if next_len as u64 > MAX_TEXT_BYTES {
                anyhow::bail!("GET {url} 响应过大（>{MAX_TEXT_BYTES}B）");
            }
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes).with_context(|| format!("GET {url} 响应不是有效 UTF-8"))
    }

    /// 下载到 `dest`（`.part` 临时文件 + 完成后原子 rename）。
    /// `progress(done, total)` 同步回调，UI 需自行桥接到主线程。
    /// 网络错误重试（指数退避）；校验失败与 HTTP 非 2xx 直接失败。
    pub async fn download(
        &self,
        url: &str,
        dest: &Path,
        expected: Option<&Checksum>,
        progress: impl Fn(u64, u64),
    ) -> Result<(), DownloadError> {
        if let Some(parent) = dest.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| DownloadError::Io { source })?;
        }
        let part = part_path(dest);
        let mut last_err = None;
        for attempt in 1..=RETRY_ATTEMPTS {
            match self
                .try_download(url, &part, dest, expected, &progress)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e @ (DownloadError::ChecksumMismatch { .. } | DownloadError::Http { .. })) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(e);
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt < RETRY_ATTEMPTS {
                        tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt - 1)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap())
    }

    async fn try_download(
        &self,
        url: &str,
        part: &Path,
        dest: &Path,
        expected: Option<&Checksum>,
        progress: &impl Fn(u64, u64),
    ) -> Result<(), DownloadError> {
        // 416 / Content-Range 不一致时删除 .part 从头重试，最多一次
        for _restart in 0..=1 {
            let existing = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);

            let mut request = self.client.get(url);
            if existing > 0 {
                request = request.header(RANGE, format!("bytes={existing}-"));
            }
            let resp = request
                .send()
                .await
                .map_err(|e| DownloadError::Network { source: e })?;

            let status = resp.status();
            // 416：Range 起点超出资源末尾（.part 已完整或服务器数据变小）→ 丢弃重下。
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                let _ = std::fs::remove_file(part);
                continue;
            }

            let resume = status == StatusCode::PARTIAL_CONTENT;
            // 206 时验证 Content-Range 起点与 .part 一致，防止服务器内容变化导致拼接损坏。
            if resume && existing > 0 {
                let cr = resp
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| {
                        v.strip_prefix("bytes ")?
                            .split('/')
                            .next()?
                            .split('-')
                            .next()?
                            .parse::<u64>()
                            .ok()
                    });
                if cr != Some(existing) {
                    // 服务器内容与 .part 不一致 → 从头重下
                    let _ = std::fs::remove_file(part);
                    continue;
                }
            }

            let total = if resume {
                existing + resp.content_length().unwrap_or(0)
            } else if status.is_success() {
                resp.content_length().unwrap_or(0)
            } else {
                return Err(DownloadError::Http {
                    status: status.as_u16(),
                });
            };

            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(part)
                .map_err(|e| DownloadError::Io { source: e })?;
            if !resume {
                // 服务器不支持 Range（200 全量）→ 从头覆盖，丢弃旧 .part 内容
                out.set_len(0)
                    .map_err(|e| DownloadError::Io { source: e })?;
            }

            let mut sha1 = Sha1::new();
            let mut sha256 = Sha256::new();
            // 流式回读已有 .part 喂 hasher（审计 #8：不得整文件读入内存，1.7GB 镜像会 OOM）
            if resume && existing > 0 {
                let f = std::fs::File::open(part).map_err(|e| DownloadError::Io { source: e })?;
                let mut reader = BufReader::with_capacity(1 << 16, f);
                let mut buf = [0u8; 1 << 16];
                loop {
                    let n = reader
                        .read(&mut buf)
                        .map_err(|e| DownloadError::Io { source: e })?;
                    if n == 0 {
                        break;
                    }
                    sha1.update(&buf[..n]);
                    sha256.update(&buf[..n]);
                }
            }

            let mut stream = resp.bytes_stream();
            let mut done = if resume { existing } else { 0 };
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| DownloadError::Network { source: e })?;
                out.write_all(&chunk)
                    .map_err(|e| DownloadError::Io { source: e })?;
                sha1.update(&chunk);
                sha256.update(&chunk);
                done += chunk.len() as u64;
                progress(done, total);
            }
            drop(out);

            // 尺寸核对：服务器声明总长时，落盘大小必须一致（截断/超发即失败重试）。
            if total > 0 {
                let actual = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
                if actual != total {
                    return Err(DownloadError::SizeMismatch {
                        expected: total,
                        actual,
                    });
                }
            }

            let actual = match expected {
                Some(Checksum::Sha1(_)) => format!("{:x}", sha1.finalize()),
                _ => format!("{:x}", sha256.finalize()),
            };
            if let Some(checksum) = expected.filter(|c| !c.hex().eq_ignore_ascii_case(&actual)) {
                let _ = std::fs::remove_file(part);
                return Err(DownloadError::ChecksumMismatch {
                    expected: checksum.hex().to_string(),
                    actual,
                });
            }
            std::fs::rename(part, dest).map_err(|e| DownloadError::Io { source: e })?;
            return Ok(());
        }
        // 两次 restart 后仍 416/不一致——数据源异常，报错走重试
        Err(DownloadError::Io {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "下载源 Range 状态反复异常（416/Content-Range 不一致）",
            ),
        })
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::{Duration, sleep};

    use super::*;

    fn sha256_hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    /// 最小 HTTP/1.1 测试服务器：GET + Range（206），可禁用 Range（200 全量）。
    struct TestServer {
        addr: String,
    }

    impl TestServer {
        fn spawn(data: Vec<u8>, support_range: bool) -> TestServer {
            let std_listener = TcpListener::bind("127.0.0.1:0").unwrap();
            std_listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
            let addr = format!("http://{}", listener.local_addr().unwrap());
            let data = Arc::new(data);
            let data2 = data.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    loop {
                        let (socket, _) = match listener.accept().await {
                            Ok(x) => x,
                            Err(_) => break,
                        };
                        let data = data2.clone();
                        tokio::spawn(async move {
                            handle_conn(socket, &data, support_range).await;
                        });
                    }
                });
            });
            TestServer { addr }
        }
    }

    async fn handle_conn(mut socket: TcpStream, data: &[u8], support_range: bool) {
        let mut buf = vec![0u8; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let mut range_start: Option<u64> = None;
        for line in req.lines() {
            if let Some(v) = line.to_lowercase().strip_prefix("range: bytes=") {
                range_start = v.split('-').next().and_then(|s| s.parse().ok());
            }
        }
        let range_start = if support_range { range_start } else { None };
        match range_start {
            Some(start) => {
                if start >= data.len() as u64 {
                    // Range 起点超出资源末尾 → 416
                    let resp = "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\n\r\n";
                    socket.write_all(resp.as_bytes()).await.unwrap();
                    return;
                }
                let body = &data[start as usize..];
                let resp = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                    body.len(),
                    start,
                    data.len() - 1,
                    data.len()
                );
                socket.write_all(resp.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
            }
            None => {
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", data.len());
                socket.write_all(resp.as_bytes()).await.unwrap();
                socket.write_all(data).await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn downloads_file_and_verifies_sha256() {
        let data: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let server = TestServer::spawn(data.clone(), true);
        let dir = temp_dir();
        let dest = dir.join("a.zip");
        let dl = Downloader::new().unwrap();
        let progress_calls = AtomicU32::new(0);
        dl.download(
            &format!("{}/a.zip", server.addr),
            &dest,
            Some(&crate::core::repo::Checksum::Sha256(sha256_hex(&data))),
            |_, _| {
                progress_calls.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!dest.with_extension("zip.part").exists());
        assert!(progress_calls.load(Ordering::Relaxed) > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn creates_missing_destination_parents() {
        let data = vec![42u8; 1024];
        let server = TestServer::spawn(data.clone(), true);
        let dir = temp_dir();
        let dest = dir.join("not-created/yet/archive.zip");

        Downloader::new()
            .unwrap()
            .download(
                &format!("{}/archive.zip", server.addr),
                &dest,
                None,
                |_, _| {},
            )
            .await
            .unwrap();

        assert_eq!(std::fs::read(dest).unwrap(), data);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fetch_text_rejects_chunked_body_over_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/repo.xml", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = std::io::Read::read(&mut socket, &mut request);
            std::io::Write::write_all(
                &mut socket,
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..=(MAX_TEXT_BYTES as usize / chunk.len()) {
                std::io::Write::write_all(&mut socket, format!("{:x}\r\n", chunk.len()).as_bytes())
                    .unwrap();
                std::io::Write::write_all(&mut socket, &chunk).unwrap();
                std::io::Write::write_all(&mut socket, b"\r\n").unwrap();
            }
            std::thread::sleep(Duration::from_secs(5));
            let _ = std::io::Write::write_all(&mut socket, b"0\r\n\r\n");
        });

        let error = tokio::time::timeout(
            Duration::from_secs(2),
            Downloader::new().unwrap().fetch_text(&url),
        )
        .await
        .expect("应在读完响应前立即拒绝超限 chunk")
        .unwrap_err();
        assert!(error.to_string().contains("响应过大"), "{error:#}");
    }

    #[tokio::test]
    async fn resumes_from_existing_part() {
        let data: Vec<u8> = (0..8192).map(|i| (i * 7 % 253) as u8).collect();
        let server = TestServer::spawn(data.clone(), true);
        let dir = temp_dir();
        let dest = dir.join("b.zip");
        let part = dest.with_extension("zip.part");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(&part, &data[..3000]).unwrap();

        let dl = Downloader::new().unwrap();
        dl.download(
            &format!("{}/b.zip", server.addr),
            &dest,
            Some(&crate::core::repo::Checksum::Sha256(sha256_hex(&data))),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn checksum_mismatch_fails_and_cleans_part() {
        let data: Vec<u8> = vec![1u8; 1024];
        let server = TestServer::spawn(data, true);
        let dir = temp_dir();
        let dest = dir.join("c.zip");
        let dl = Downloader::new().unwrap();
        let err = dl
            .download(
                &format!("{}/c.zip", server.addr),
                &dest,
                Some(&crate::core::repo::Checksum::Sha256("deadbeef".into())),
                |_, _| {},
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ChecksumMismatch { .. }));
        assert!(!dest.exists());
        assert!(!dest.with_extension("zip.part").exists());
    }

    #[tokio::test]
    async fn works_without_range_support() {
        let data: Vec<u8> = vec![3u8; 2048];
        let server = TestServer::spawn(data.clone(), false);
        let dir = temp_dir();
        let dest = dir.join("d.zip");
        let part = dest.with_extension("zip.part");
        std::fs::write(&part, &data[..1000]).unwrap();
        let dl = Downloader::new().unwrap();
        // 服务器忽略 Range（返回 200 全量）→ 直接覆盖完整内容
        dl.download(&format!("{}/d.zip", server.addr), &dest, None, |_, _| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    #[tokio::test]
    async fn complete_part_resumes_without_416_loop() {
        // .part 已完整（416）→ 丢弃重下成功，不无限递归
        let data: Vec<u8> = (0..2048).map(|i| (i % 251) as u8).collect();
        let server = TestServer::spawn(data.clone(), true);
        let dir = temp_dir();
        let dest = dir.join("f.zip");
        let part = dest.with_extension("zip.part");
        std::fs::write(&part, &data).unwrap();
        let dl = Downloader::new().unwrap();
        dl.download(
            &format!("{}/f.zip", server.addr),
            &dest,
            Some(&crate::core::repo::Checksum::Sha256(sha256_hex(&data))),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn range_restart_on_content_range_mismatch() {
        // 服务器数据变小：.part 起点超出新资源末尾 → 416 → 从头重下
        let data: Vec<u8> = vec![9u8; 1024];
        let server = TestServer::spawn(data.clone(), true);
        let dir = temp_dir();
        let dest = dir.join("g.zip");
        let part = dest.with_extension("zip.part");
        std::fs::write(&part, vec![0u8; 2048]).unwrap();
        let dl = Downloader::new().unwrap();
        dl.download(&format!("{}/g.zip", server.addr), &dest, None, |_, _| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn wrong_content_range_is_discarded_before_clean_restart() {
        let data = vec![7u8; 2048];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(false).unwrap();
        let url = format!("http://{}/archive.zip", listener.local_addr().unwrap());
        let worker_data = data.clone();
        std::thread::spawn(move || {
            for request_index in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = std::io::Read::read(&mut socket, &mut request);
                if request_index == 0 {
                    let body = &worker_data[512..];
                    std::io::Write::write_all(
                        &mut socket,
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
                            body.len(),
                            worker_data.len() - 1,
                            worker_data.len()
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                    let _ = std::io::Write::write_all(&mut socket, body);
                } else {
                    std::io::Write::write_all(
                        &mut socket,
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            worker_data.len()
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                    std::io::Write::write_all(&mut socket, &worker_data).unwrap();
                }
            }
        });
        let dir = temp_dir();
        let dest = dir.join("wrong-range.zip");
        std::fs::write(dest.with_extension("zip.part"), &data[..512]).unwrap();

        Downloader::new()
            .unwrap()
            .download(&url, &dest, None, |_, _| {})
            .await
            .unwrap();

        assert_eq!(std::fs::read(dest).unwrap(), data);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn http_error_fails_without_retry() {
        let dir = temp_dir();
        let dest = dir.join("e.zip");
        let dl = Downloader::new().unwrap();
        let err = dl
            .download("http://127.0.0.1:1/nope.zip", &dest, None, |_, _| {})
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::Network { .. }));
        let _ = sleep(Duration::from_millis(1)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("liteavd-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

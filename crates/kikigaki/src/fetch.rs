#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io::Write;
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use kikigaki_core::models::fetch::Fetcher;

/// Streaming HTTP model fetcher.
///
/// Downloads always start from byte zero; interrupted downloads are not resumed.
pub(crate) struct ReqwestFetcher {
    client: reqwest::Client,
}

impl ReqwestFetcher {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .context("build model HTTP client")?;
        Ok(Self { client })
    }
}

impl Fetcher for ReqwestFetcher {
    fn fetch(
        &self,
        url: &str,
        dest: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
    ) -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build model download runtime")?;
        runtime.block_on(async {
            let response = self
                .client
                .get(url)
                .send()
                .await
                .with_context(|| format!("request {url}"))?
                .error_for_status()
                .with_context(|| format!("HTTP error for {url}"))?;
            let mut stream = response.bytes_stream();
            let mut total = 0_u64;
            loop {
                let next = tokio::time::timeout(Duration::from_secs(60), stream.next())
                    .await
                    .with_context(|| format!("no data received from {url} for 60 seconds"))?;
                let Some(chunk) = next else {
                    break;
                };
                let chunk = chunk.with_context(|| format!("read response body from {url}"))?;
                dest.write_all(&chunk)
                    .with_context(|| format!("write response body from {url}"))?;
                total += chunk.len() as u64;
                progress(total);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn serve(response: &'static [u8]) -> Option<(String, thread::JoinHandle<()>)> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("SKIPPED: loopback sockets denied by sandbox");
                return None;
            }
            Err(error) => panic!("bind test listener: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(response).unwrap();
        });
        Some((format!("http://{address}/model"), handle))
    }

    #[test]
    fn fetches_successful_body_and_reports_progress() {
        let Some((url, server)) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nmodel")
        else {
            return;
        };
        let fetcher = ReqwestFetcher::new().unwrap();
        let mut output = Vec::new();
        let mut updates = Vec::new();
        fetcher
            .fetch(&url, &mut output, &mut |done| updates.push(done))
            .unwrap();
        server.join().unwrap();
        assert_eq!(output, b"model");
        assert_eq!(updates.last(), Some(&5));
    }

    #[test]
    fn rejects_http_error_status() {
        let Some((url, server)) = serve(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
        else {
            return;
        };
        let fetcher = ReqwestFetcher::new().unwrap();
        let error = fetcher
            .fetch(&url, &mut Vec::new(), &mut |_| {})
            .unwrap_err();
        server.join().unwrap();
        assert!(format!("{error:#}").contains("404"));
    }

    #[test]
    fn rejects_body_that_ends_before_content_length() {
        let Some((url, server)) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort")
        else {
            return;
        };
        let fetcher = ReqwestFetcher::new().unwrap();
        assert!(fetcher.fetch(&url, &mut Vec::new(), &mut |_| {}).is_err());
        server.join().unwrap();
    }
}

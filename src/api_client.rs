use axum::body::Body;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::{io, path::Path, time::Duration};

pub fn request(root: &Path, method: &str, path: &str, body: Value) -> io::Result<Value> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(4), async {
                let socket = tokio::net::UnixStream::connect(root.join("api.sock")).await?;
                let (mut sender, connection) =
                    hyper::client::conn::http1::handshake(TokioIo::new(socket))
                        .await
                        .map_err(io::Error::other)?;
                let task = tokio::spawn(async move {
                    let _ = connection.await;
                });
                let request = hyper::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("Host", "localhost")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body)?))
                    .map_err(io::Error::other)?;
                let response = sender
                    .send_request(request)
                    .await
                    .map_err(io::Error::other)?;
                let status = response.status();
                let mut body = response.into_body();
                let mut bytes = Vec::new();
                while let Some(frame) = body.frame().await {
                    if let Ok(data) = frame.map_err(io::Error::other)?.into_data() {
                        if bytes.len() + data.len() > 65536 {
                            return Err(io::Error::other("API response exceeds 64 KiB"));
                        }
                        bytes.extend_from_slice(&data);
                    }
                }
                task.abort();
                let value: Value = serde_json::from_slice(&bytes)?;
                if !status.is_success() {
                    return Err(io::Error::other(
                        value["error"]["message"]
                            .as_str()
                            .unwrap_or("API request failed"),
                    ));
                }
                Ok(value)
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "API request deadline exceeded"))?
        })
}

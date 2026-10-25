use crate::protocol::{Request, Response};
use std::{
    error::Error,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

pub fn request(root: &Path, req: Request) -> Result<serde_json::Value, Box<dyn Error>> {
    request_with_timeout(root, req, Duration::from_secs(2))
}

pub(crate) fn request_with_timeout(
    root: &Path,
    req: Request,
    timeout: Duration,
) -> Result<serde_json::Value, Box<dyn Error>> {
    const LIMIT: usize = 64 * 1024;
    let mut bytes = serde_json::to_vec(&req)?;
    bytes.push(b'\n');
    if bytes.len() > LIMIT {
        return Err(io::Error::other("request exceeds 64 KiB").into());
    }
    let socket = root.join("control.sock");
    let mut stream = UnixStream::connect(&socket).map_err(|error| io::Error::new(error.kind(), format!("could not connect to {}: {error}; check --root and start the manager: yactl --root '{}' daemon --detach", socket.display(), root.display())))?;
    let deadline = Instant::now() + timeout;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "manager response timed out"))
    };
    let mut pending = bytes.as_slice();
    while !pending.is_empty() {
        stream.set_write_timeout(Some(remaining()?))?;
        let n = stream.write(pending)?;
        if n == 0 {
            return Err(
                io::Error::new(io::ErrorKind::WriteZero, "manager closed the connection").into(),
            );
        }
        pending = &pending[n..];
    }
    let mut line = Vec::new();
    let mut chunk = [0; 4096];
    while line.len() <= LIMIT {
        stream.set_read_timeout(Some(remaining()?))?;
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if let Some(end) = chunk[..n].iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&chunk[..=end]);
            break;
        }
        line.extend_from_slice(&chunk[..n]);
    }
    if line.len() > LIMIT || line.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid or oversized manager response").into());
    }
    let response: Response = serde_json::from_slice(&line)?;
    if response.ok {
        Ok(response.data)
    } else {
        Err(io::Error::other(response.error.unwrap_or_else(|| "manager error".into())).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    #[test]
    fn connection_error_has_launch_hint() {
        let root =
            std::env::temp_dir().join(format!("yactl-client-missing-{}", std::process::id()));
        let error = request(&root, Request::Ping).unwrap_err().to_string();
        assert!(error.contains(&root.display().to_string()));
        assert!(error.contains("daemon --detach"));
    }
    #[test]
    fn trickled_frame_has_total_deadline() {
        let root =
            std::env::temp_dir().join(format!("yactl-client-deadline-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            for _ in 0..20 {
                if stream.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        assert!(request_with_timeout(&root, Request::Ping, Duration::from_millis(100)).is_err());
        assert!(start.elapsed() < Duration::from_millis(350));
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fragmented_and_oversized_responses() {
        let root = std::env::temp_dir().join(format!("yactl-client-frames-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        let server = std::thread::spawn(move || {
            for answer in [
                b"{\"ok\":true,\"data\":1}\n".to_vec(),
                vec![b'x'; 65537],
                b"{\"ok\":true}".to_vec(),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                for part in answer.chunks(7) {
                    if stream.write_all(part).is_err() {
                        break;
                    }
                }
            }
        });
        assert_eq!(request(&root, Request::Ping).unwrap(), 1);
        assert!(request(&root, Request::Ping).is_err());
        assert!(request(&root, Request::Ping).is_err());
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn protocol_success_and_error() {
        let root = std::env::temp_dir().join(format!("yactl-client-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        let server = std::thread::spawn(move || {
            for answer in [
                "{\"ok\":true,\"data\":{\"pid\":123}}\n",
                "{\"ok\":false,\"error\":\"missing\"}\n",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                assert!(matches!(
                    serde_json::from_str::<Request>(&line).unwrap(),
                    Request::Ping
                ));
                stream.write_all(answer.as_bytes()).unwrap();
            }
        });
        assert_eq!(request(&root, Request::Ping).unwrap()["pid"], 123);
        assert_eq!(
            request(&root, Request::Ping).unwrap_err().to_string(),
            "missing"
        );
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

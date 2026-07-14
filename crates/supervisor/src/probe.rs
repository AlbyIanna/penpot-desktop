//! Tiny readiness probes: RESP `PING` for valkey, `GET /readyz` for the
//! backend, plain TCP connect for postgres. No client-library dependencies.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// One RESP `PING`; succeeds on `+PONG`.
pub async fn valkey_ping(port: u16) -> Result<(), String> {
    let fut = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        stream
            .write_all(b"*1\r\n$4\r\nPING\r\n")
            .await
            .map_err(|e| format!("write: {e}"))?;
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.map_err(|e| format!("read: {e}"))?;
        let reply = &buf[..n];
        if reply.starts_with(b"+PONG") {
            Ok(())
        } else {
            Err(format!("unexpected reply: {}", String::from_utf8_lossy(reply)))
        }
    };
    tokio::time::timeout(IO_TIMEOUT, fut)
        .await
        .map_err(|_| "timed out".to_string())?
}

/// Minimal HTTP/1.1 GET; succeeds iff the status code is 200.
pub async fn http_ok(port: u16, path: &str) -> Result<(), String> {
    let fut = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(|e| format!("read: {e}"))?;
        let status_line = response
            .split(|&b| b == b'\n')
            .next()
            .map(|l| String::from_utf8_lossy(l).trim().to_string())
            .unwrap_or_default();
        // "HTTP/1.1 200 OK"
        if status_line.split_whitespace().nth(1) == Some("200") {
            Ok(())
        } else {
            Err(format!("status line: {status_line:?}"))
        }
    };
    tokio::time::timeout(IO_TIMEOUT, fut)
        .await
        .map_err(|_| "timed out".to_string())?
}

/// Plain TCP connect (postgres liveness).
pub async fn tcp_open(port: u16) -> Result<(), String> {
    tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
        .await
        .map_err(|_| "timed out".to_string())?
        .map(|_| ())
        .map_err(|e| format!("connect: {e}"))
}

/// Is something already LISTENing on `port` (loopback-reachable)? A connect
/// probe is used instead of a bind test because BSD `SO_REUSEADDR` semantics
/// let a specific-address bind succeed while a stale process still holds the
/// wildcard `0.0.0.0:<port>` — exactly the stale-exporter case this guards
/// (post-M5 debt #1).
pub async fn port_has_listener(port: u16) -> bool {
    matches!(
        tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(("127.0.0.1", port))).await,
        Ok(Ok(_))
    )
}

/// Pids of the processes LISTENing on `port` (best-effort, via `lsof`; empty
/// when `lsof` is unavailable or reports nothing). Blocking — call from a
/// blocking context or accept the short stall (lsof on one port is fast).
pub fn listener_pids(port: u16) -> Vec<u32> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|t| t.parse::<u32>().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

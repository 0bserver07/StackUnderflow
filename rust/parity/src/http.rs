//! A deliberately tiny, deliberately dumb HTTP/1.1 client.
//!
//! Not `reqwest`, not `hyper`. A byte-parity differ that reads its evidence
//! through a helpful client is measuring the client: every mainstream one
//! transparently decompresses, follows redirects, retries, normalises header
//! casing, or coalesces `Transfer-Encoding`. Any of those would turn a real
//! divergence into a green tick.
//!
//! So: one socket, one request, the response bytes exactly as they arrived.
//! `Connection: close` on every request so neither server can keep-alive its
//! way into a framing subtlety, and both `Content-Length` and `chunked` bodies
//! are decoded because uvicorn and axum do not always agree on which to use.
//!
//! Blocking on purpose — the differ walks its cases in order, because the cases
//! carry state (`POST /api/project-by-dir` is what makes the next `GET`
//! answerable).

use std::io::{BufRead, BufReader, Write as _};
use std::net::TcpStream;
use std::time::Duration;

/// One response, kept as bytes.
#[derive(Debug, Clone)]
pub struct Response {
    /// The numeric status.
    pub status: u16,
    /// Header names lowercased, values verbatim, in arrival order.
    pub headers: Vec<(String, String)>,
    /// The decoded body — de-chunked if it arrived chunked, never decompressed.
    pub body: Vec<u8>,
}

impl Response {
    /// The first value for `name` (already lowercase).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Send one request and read the whole response.
///
/// `body` of `None` sends no entity; `Some` sends it as `application/json`,
/// which is the only content type the ported POST handlers accept.
///
/// # Errors
/// Connection, write, read and framing failures — all of which are harness
/// problems, not parity findings, and are reported as such.
pub fn request(
    port: u16,
    method: &str,
    target: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<Response> {
    let stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: */*\r\n"
    );
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    let mut stream = stream;
    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body)?;
    }
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let status = read_status(&mut reader)?;
    let headers = read_headers(&mut reader)?;
    let body = read_body(&mut reader, &headers)?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn read_line(reader: &mut impl BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed mid-response",
        ));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn read_status(reader: &mut impl BufRead) -> std::io::Result<u16> {
    let line = read_line(reader)?;
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unparseable status line: {line:?}"),
            )
        })
}

fn read_headers(reader: &mut impl BufRead) -> std::io::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            return Ok(headers);
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
}

fn read_body(reader: &mut impl BufRead, headers: &[(String, String)]) -> std::io::Result<Vec<u8>> {
    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    if find("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return read_chunked(reader);
    }
    if let Some(len) = find("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
        let mut body = vec![0_u8; len];
        reader.read_exact(&mut body)?;
        return Ok(body);
    }
    // No framing header: read to EOF, which `Connection: close` guarantees.
    let mut body = Vec::new();
    reader.read_to_end(&mut body)?;
    Ok(body)
}

fn read_chunked(reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader)?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad chunk size: {size_text:?}"),
            )
        })?;
        if size == 0 {
            // Trailers, then the terminating blank line.
            while !read_line(reader)?.is_empty() {}
            return Ok(body);
        }
        let mut chunk = vec![0_u8; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        let mut crlf = [0_u8; 2];
        reader.read_exact(&mut crlf)?;
    }
}

/// Poll `GET /` until the server answers or `deadline` passes.
///
/// Returns the time it took, so the harness can report a slow boot rather than
/// silently absorbing it.
///
/// # Errors
/// When the deadline passes without a response.
pub fn wait_until_up(port: u16, deadline: Duration) -> std::io::Result<Duration> {
    let started = std::time::Instant::now();
    while started.elapsed() < deadline {
        if request(port, "GET", "/", None, Duration::from_secs(5)).is_ok() {
            return Ok(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("port {port} never answered within {deadline:?}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn content_length_bodies_are_read_exactly() {
        let raw = b"5\r\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(raw));
        let headers = vec![("content-length".to_owned(), "3".to_owned())];
        let body = read_body(&mut reader, &headers).expect("body");
        assert_eq!(body, b"5\r\n");
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(raw));
        let body = read_chunked(&mut reader).expect("body");
        assert_eq!(body, b"Wikipedia");
    }

    #[test]
    fn headers_are_lowercased_but_values_are_not() {
        let raw = b"Content-Type: application/json\r\nX-Odd: KeepCase\r\n\r\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(raw));
        let headers = read_headers(&mut reader).expect("headers");
        assert_eq!(headers[0].0, "content-type");
        assert_eq!(headers[1].1, "KeepCase");
    }
}

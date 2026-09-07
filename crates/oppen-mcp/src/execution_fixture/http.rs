use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

// Connection: close makes EOF observable on the wire even for an SSE response.
pub(super) async fn send_http_request(
    addr: std::net::SocketAddr,
    request: Request<Body>,
) -> BufReader<TcpStream> {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.expect("body");
    let mut socket = TcpStream::connect(addr).await.expect("connect");
    let mut head = format!(
        "{} {} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        parts.method,
        parts.uri,
        body.len()
    );
    for (name, value) in &parts.headers {
        head.push_str(&format!("{name}: {}\r\n", value.to_str().expect("header")));
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await.expect("headers");
    socket.write_all(&body).await.expect("body");
    BufReader::new(socket)
}

pub(super) async fn http_request(
    addr: std::net::SocketAddr,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, BufReader<TcpStream>) {
    let mut reader = send_http_request(addr, request).await;
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("status line");
    let status = line.split_whitespace().nth(1).expect("status");
    let status = StatusCode::from_bytes(status.as_bytes()).expect("HTTP status");
    let mut headers = axum::http::HeaderMap::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).await.expect("header"), 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').expect("header field");
        headers.append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            value.trim().parse().expect("header value"),
        );
    }
    (status, headers, reader)
}

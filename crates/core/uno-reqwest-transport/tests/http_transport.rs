use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use bytes::Bytes;
use futures_util::TryStreamExt;
use http::{Request, StatusCode, header::CONTENT_TYPE};
use transport::HttpTransport;
use uno_reqwest_transport::ReqwestTransport;

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];

    loop {
        let count = stream.read(&mut buffer).expect("read request");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);

        let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..headers_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if request.len() >= headers_end + 4 + content_length {
            break;
        }
    }

    request
}

#[tokio::test]
async fn forwards_http_request_and_streams_response_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
    let address = listener.local_addr().expect("local address");
    let (request_tx, request_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let request = read_request(&mut socket);
        request_tx.send(request).expect("send captured request");
        socket
            .write_all(
                b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\nX-Transport: local\r\nConnection: close\r\n\r\nhello",
            )
            .expect("write response");
    });

    let transport = ReqwestTransport::without_proxy().expect("build reqwest client");
    let request = Request::put(format!("http://{address}/transport-check"))
        .header(CONTENT_TYPE, "application/octet-stream")
        .header("x-request-marker", "present")
        .body(Bytes::from_static(b"payload"))
        .expect("build request");
    let response = transport.send(request).await.expect("send request");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-transport"], "local");
    let chunks = response
        .into_body()
        .try_collect::<Vec<_>>()
        .await
        .expect("read response stream");
    assert_eq!(chunks.concat(), b"hello");

    let captured = request_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("server should capture request");
    let captured = String::from_utf8_lossy(&captured);
    assert!(captured.starts_with("PUT /transport-check HTTP/1.1\r\n"));
    assert!(
        captured
            .to_ascii_lowercase()
            .contains("x-request-marker: present")
    );
    assert!(captured.ends_with("payload"));

    server.join().expect("server thread should finish");
}

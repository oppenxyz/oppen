//! Exercise the actual plugin verifier. No GitHub credential or installation.
use std::io::{Read, Write};
use std::net::TcpListener;
use tauri_plugin_updater::UpdaterExt;

async fn download(tamper: bool) -> tauri_plugin_updater::Result<Vec<u8>> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback fixture");
    let address = listener.local_addr().expect("address");
    let manifest = serde_json::json!({
        "version": "99.0.0",
        "platforms": { "test": {
            "url": format!("http://{address}/package"),
            "signature": include_str!("fixtures/update.txt.sig").trim()
        }}
    })
    .to_string();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().expect("request");
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("timeout");
            let mut request = [0; 4096];
            let count = socket.read(&mut request).expect("request headers");
            let body = if std::str::from_utf8(&request[..count])
                .expect("HTTP")
                .starts_with("GET /package")
            {
                if tamper {
                    b"changed after signing".to_vec()
                } else {
                    include_bytes!("fixtures/update.txt").to_vec()
                }
            } else {
                manifest.as_bytes().to_vec()
            };
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("headers");
            socket.write_all(&body).expect("body");
        }
    });
    let mut context = tauri::test::mock_context(tauri::test::noop_assets());
    context.config_mut().plugins.0.insert(
        "updater".into(),
        serde_json::json!({ "pubkey": include_str!("fixtures/update.pub").trim() }),
    );
    let app = tauri::test::mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(context)
        .expect("mock host");
    let update = app
        .updater_builder()
        .pubkey(include_str!("fixtures/update.pub").trim())
        .target("test")
        .timeout(std::time::Duration::from_secs(5))
        .endpoints(vec![
            format!("http://{address}/manifest").parse().expect("URL"),
        ])
        .expect("debug loopback")
        .build()
        .expect("updater")
        .check()
        .await
        .expect("manifest")
        .expect("newer version");
    let result = update.download(|_, _| {}, || {}).await;
    server.join().expect("fixture complete");
    result
}

#[tokio::test]
async fn signed_bytes_are_accepted_and_modified_bytes_are_refused() {
    assert_eq!(
        download(false).await.expect("verified"),
        include_bytes!("fixtures/update.txt")
    );
    assert!(
        download(true).await.is_err(),
        "unverified bytes must not reach the installer"
    );
}

use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const ADDRESS: &str = "0x1111111111111111111111111111111111111111";

// Shape and timestamps pinned to the official Python SDK extraAgents cassette;
// addresses here are synthetic and no request leaves loopback.
const AGENTS: &str = r#"[{"name":"ok","address":"0x1111111111111111111111111111111111111111","validUntil":1767776120478},{"name":"new","address":"0x2222222222222222222222222222222222222222","validUntil":1771174068515}]"#;

async fn fixture(
    response: &'static str,
) -> (InfoClient, tokio::task::JoinHandle<serde_json::Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(line, "POST /info HTTP/1.1\r\n");
            let mut length = None;
            loop {
                line.clear();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" { break; }
                let (name, value) = line.split_once(':').unwrap();
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let length = length.unwrap();
            assert!(length < 1024);
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            let request = serde_json::from_slice(&body).unwrap();
            let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len());
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            request
        }).await.expect("fixture transport must complete")
    });
    let mut info = InfoClient::with_client(
        Network::Testnet,
        Client::builder().no_proxy().build().unwrap(),
    );
    info.url = format!("http://{addr}/info");
    (info, server)
}

#[tokio::test]
async fn activation_reads_send_exact_wire_requests_and_decode_responses() {
    let user = Address::parse(ADDRESS).unwrap();
    let (info, server) = fixture(AGENTS).await;
    let agents = info.extra_agents(user).await.unwrap();
    assert_eq!(agents.len(), 2);
    assert_eq!(
        agents[0],
        ExtraAgent {
            name: "ok".into(),
            address: user,
            valid_until: 1767776120478
        }
    );
    assert_eq!(agents[1].valid_until, 1771174068515);
    assert_eq!(
        server.await.unwrap(),
        serde_json::json!({"type":"extraAgents","user":ADDRESS})
    );

    let (info, server) =
        fixture(r#"{"role":"agent","data":{"user":"0x1111111111111111111111111111111111111111"}}"#)
            .await;
    assert_eq!(
        info.user_role(user).await.unwrap(),
        UserRole::Agent { user }
    );
    assert_eq!(
        server.await.unwrap(),
        serde_json::json!({"type":"userRole","user":ADDRESS})
    );
}

#[tokio::test]
async fn malformed_activation_responses_use_existing_transport_errors_not_defaults() {
    let user = Address::parse(ADDRESS).unwrap();
    for body in ["null", r#"[{"name":"missing identity"}]"#] {
        let (info, server) = fixture(body).await;
        assert!(
            matches!(info.extra_agents(user).await, Err(Error::Http(error)) if error.is_decode())
        );
        server.await.unwrap();
    }
    for body in [r#"{"role":"newRole"}"#, r#"{"role":"agent"}"#] {
        let (info, server) = fixture(body).await;
        assert!(matches!(info.user_role(user).await, Err(Error::Http(error)) if error.is_decode()));
        server.await.unwrap();
    }
    let (info, server) = fixture("[]").await;
    assert!(info.extra_agents(user).await.unwrap().is_empty());
    server.await.unwrap();
}

#[test]
fn every_documented_role_preserves_its_relationship() {
    let address = Address::parse(ADDRESS).unwrap();
    for (wire, expected) in [
        (serde_json::json!({"role":"user"}), UserRole::User),
        (
            serde_json::json!({"role":"agent","data":{"user":ADDRESS}}),
            UserRole::Agent { user: address },
        ),
        (
            serde_json::json!({"role":"subAccount","data":{"master":ADDRESS}}),
            UserRole::SubAccount { master: address },
        ),
        (serde_json::json!({"role":"vault"}), UserRole::Vault),
        (serde_json::json!({"role":"missing"}), UserRole::Missing),
    ] {
        assert_eq!(
            serde_json::from_value::<UserRole>(wire.clone()).unwrap(),
            expected
        );
        assert_eq!(serde_json::to_value(expected).unwrap(), wire);
    }
}

#[test]
fn roles_refuse_missing_malformed_unknown_and_duplicate_identity_fields() {
    for body in [
        "null",
        "[]",
        "{}",
        r#"{"role":null}"#,
        r#"{"role":"unknown"}"#,
        r#"{"role":"agent"}"#,
        r#"{"role":"subAccount"}"#,
        r#"{"role":"agent","data":null}"#,
        r#"{"role":"agent","data":{}}"#,
        r#"{"role":"user","data":{"user":"0x1111111111111111111111111111111111111111"}}"#,
        r#"{"role":"agent","extra":true,"data":{"user":"0x1111111111111111111111111111111111111111"}}"#,
        r#"{"role":"agent","data":{"user":"0x1111111111111111111111111111111111111111","master":"0x2222222222222222222222222222222222222222"}}"#,
        r#"{"role":"agent","data":{"user":"invalid"}}"#,
        r#"{"role":"subAccount","data":{"master":null}}"#,
        r#"{"role":"subAccount","data":{"master":"invalid"}}"#,
        r#"{"role":"subAccount","data":{"user":"0x1111111111111111111111111111111111111111"}}"#,
        r#"{"role":"user","role":"missing"}"#,
        r#"{"role":"agent","data":{"user":"0x1111111111111111111111111111111111111111","user":"0x2222222222222222222222222222222222222222"}}"#,
    ] {
        assert!(
            serde_json::from_str::<UserRole>(body).is_err(),
            "accepted {body}"
        );
    }
}

#[test]
fn extra_agents_require_all_fields_and_unsigned_integral_expiry() {
    let valid = serde_json::json!({"name":"", "address":ADDRESS, "validUntil":0});
    assert_eq!(
        serde_json::from_value::<ExtraAgent>(valid.clone())
            .unwrap()
            .valid_until,
        0
    );
    for field in ["name", "address", "validUntil"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<ExtraAgent>(missing).is_err());
        let mut null = valid.clone();
        null[field] = serde_json::Value::Null;
        assert!(serde_json::from_value::<ExtraAgent>(null).is_err());
    }
    for expiry in [
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!("123"),
        serde_json::json!(true),
    ] {
        let mut malformed = valid.clone();
        malformed["validUntil"] = expiry;
        assert!(serde_json::from_value::<ExtraAgent>(malformed).is_err());
    }
    let mut malformed = valid;
    malformed["address"] = serde_json::json!("0x1234");
    assert!(serde_json::from_value::<ExtraAgent>(malformed).is_err());
}

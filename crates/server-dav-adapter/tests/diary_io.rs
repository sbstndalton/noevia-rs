#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use server_dav_adapter::{
    auth::{AuthConfig, AuthGate},
    diary_io::{encode_file_read_body, encode_list_path, DiaryReadClient, ReadError},
};
use server_store::{
    db_path,
    rusqlite::{params, Connection},
    Store,
};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tenant_assertion::{verify, Request as AssertionRequest};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../server-store/tests/fixtures/node-schema.sql"
));
const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PASSWORD: &str =
    "nv_dav_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.T__o4qEYEzGvCBRCkWGgkJUiECUVkENvm8M2ypgmjQ4";
const HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$htTnVq45aTFtSZ4j6uYg2Q$0dpnt5pAek0kwLdm/v1/iGBKzQ1bxZlOeOT9W9kbBlQ";
const KEY: &str = "synthetic-tenant-key-for-fixtures";
const TOKEN: &str = "synthetic-service-token";

struct Fixture {
    _dir: tempfile::TempDir,
    gate: AuthGate,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(db_path(dir.path())).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute_batch("CREATE TABLE dav_settings(user_id TEXT PRIMARY KEY REFERENCES users(id), scope TEXT NOT NULL, acknowledged_at INTEGER, updated_at INTEGER NOT NULL);").unwrap();
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u-member','Member.One','member.one','Synthetic Member','member','x','w-member',1,1)", []).unwrap();
        db.execute("INSERT INTO app_passwords(id,user_id,name,scope,password_hash,created_at) VALUES(?1,'u-member','Synthetic Device','lan',?2,1)", params![ID,HASH]).unwrap();
        db.execute(
            "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES('u-member',1,1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO dav_settings(user_id,scope,updated_at) VALUES('u-member','lan',1)",
            [],
        )
        .unwrap();
        let gate = AuthGate::new(
            Arc::new(Store::open(dir.path()).unwrap()),
            AuthConfig::new("lan", KEY).unwrap(),
        );
        Self { _dir: dir, gate }
    }
    fn identity(&self) -> server_dav_adapter::BoundIdentity {
        let identity = self
            .gate
            .authenticate("member.one", PASSWORD)
            .unwrap()
            .unwrap();
        assert!(self.gate.allowed(&identity).unwrap());
        identity
    }
}
fn client(addr: SocketAddr) -> DiaryReadClient {
    DiaryReadClient::new(
        addr,
        KEY.to_owned(),
        TOKEN.to_owned(),
        Duration::from_secs(2),
    )
    .unwrap()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn serve_once(listener: TcpListener, status: &str, payload: Vec<u8>) -> (String, Vec<u8>) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut bytes = Vec::new();
    let head_end = loop {
        let mut block = [0u8; 1024];
        let count = stream.read(&mut block).await.unwrap();
        assert!(count > 0 && bytes.len() + count < 8192);
        bytes.extend_from_slice(&block[..count]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8(bytes[..head_end].to_vec()).unwrap();
    let len = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|n| n.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while bytes.len() - head_end < len {
        let mut block = [0u8; 1024];
        let count = stream.read(&mut block).await.unwrap();
        assert!(count > 0 && bytes.len() + count < 8192);
        bytes.extend_from_slice(&block[..count]);
    }
    let body = bytes[head_end..head_end + len].to_vec();
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len());
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.write_all(&payload).await.unwrap();
    (head, body)
}
fn header<'a>(head: &'a str, name: &str) -> &'a str {
    head.lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then_some(value.trim())
        })
        .unwrap()
}
fn verify_signed(head: &str, body: &[u8], method: &str, target: &str) {
    assert!(head.starts_with(&format!("{method} {target} HTTP/1.1\r\n")));
    assert_eq!(header(head, "authorization"), format!("Bearer {TOKEN}"));
    assert_eq!(header(head, "x-cowork-user-id"), "u-member");
    assert_eq!(header(head, "x-cowork-storage"), "eyJraW5kIjoibG9jYWwifQ");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let hash = hex(&Sha256::digest(body));
    let request = AssertionRequest {
        key: KEY,
        user_id: "u-member",
        assertion: header(head, "x-cowork-tenant-assertion"),
        method,
        path,
        query: query.as_bytes(),
        body_hash: &hash,
        storage: header(head, "x-cowork-storage"),
        legacy_owner: "",
        blocked: "",
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64(),
    };
    verify(&request).unwrap();
}

#[test]
fn configuration_and_pure_encoder_refuse_unsafe_inputs() {
    let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 8080);
    assert_eq!(
        DiaryReadClient::new(remote, KEY.into(), TOKEN.into(), Duration::from_secs(1)).err(),
        Some(ReadError::Configuration)
    );
    let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
    for key in ["", " ", "a\nb"] {
        assert_eq!(
            DiaryReadClient::new(local, key.into(), TOKEN.into(), Duration::from_secs(1)).err(),
            Some(ReadError::Configuration)
        );
    }
    assert_eq!(
        DiaryReadClient::new(
            local,
            KEY.into(),
            "bad token".into(),
            Duration::from_secs(1)
        )
        .err(),
        Some(ReadError::Configuration)
    );
    assert_eq!(
        DiaryReadClient::new(local, KEY.into(), TOKEN.into(), Duration::ZERO).err(),
        Some(ReadError::Configuration)
    );
    for path in ["../x", "a//b", "a\\b", "a%2fb", ".hidden", "a\n"] {
        assert_eq!(encode_list_path(path), Err(ReadError::InvalidPath));
    }
    assert_eq!(
        encode_file_read_body("photo.png"),
        Err(ReadError::InvalidPath)
    );
    assert_eq!(encode_list_path("a b").unwrap(), "/api/files?path=a+b");
    assert!(format!("{:?}", client(local)).contains("[redacted]"));
    assert!(!format!("{:?}", client(local)).contains(KEY));
}

#[tokio::test]
async fn signed_fixed_read_routes_and_typed_replies() {
    let fixture = Fixture::new();
    let identity = fixture.identity();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_once(listener, "200 OK", br#"{"files":[{"path":"a b/entry.md","name":"entry.md","isDir":false,"modified":1790000000.5}]}"#.to_vec()));
    let entries = client(address).list(&identity, "a b").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "a b/entry.md");
    assert_eq!(entries[0].modified_unix, Some(1790000000));
    let (head, body) = server.await.unwrap();
    assert!(body.is_empty());
    verify_signed(&head, &body, "GET", "/api/files?path=a+b");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let content = "synthetic entry";
    let hash = hex(&Sha256::digest(content.as_bytes()));
    let reply = format!("{{\"path\":\"a.md\",\"content\":\"{content}\",\"version\":\"{hash}\"}}");
    let server = tokio::spawn(serve_once(listener, "200 OK", reply.into_bytes()));
    let file = client(address).read(&identity, "a.md").await.unwrap();
    assert_eq!(file.content.as_deref(), Some(content));
    assert_eq!(file.version.as_deref(), Some(hash.as_str()));
    let (head, body) = server.await.unwrap();
    assert_eq!(body, br#"{"path":"a.md"}"#);
    verify_signed(&head, &body, "POST", "/api/file");
}

#[tokio::test]
async fn redirect_status_oversize_and_malformed_replies_fail_closed() {
    let fixture = Fixture::new();
    let identity = fixture.identity();
    for (status, payload, expected) in [
        ("302 Found", b"{}".to_vec(), ReadError::Redirect),
        ("403 Forbidden", b"{}".to_vec(), ReadError::Refused(403)),
        ("200 OK", vec![b'x'; 1024 * 1024 + 1], ReadError::TooLarge),
        (
            "200 OK",
            br#"{"files":[{"path":"../bad.md","name":"bad.md","isDir":false}]}"#.to_vec(),
            ReadError::Malformed,
        ),
        (
            "200 OK",
            br#"{"files":[{"path":"nested/file.md","name":"nested/file.md","isDir":false}]}"#
                .to_vec(),
            ReadError::Malformed,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(listener, status, payload));
        let result = client(address).list(&identity, "").await;
        assert_eq!(result.err(), Some(expected));
        let _ = server.await.unwrap();
    }
}

#[tokio::test]
async fn child_limit_is_checked_before_collecting_extra_rows_and_timeout_is_bounded() {
    let fixture = Fixture::new();
    let identity = fixture.identity();
    let rows = (0..1025)
        .map(|i| format!(r#"{{"path":"f{i}.md","name":"f{i}.md","isDir":false}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let payload = format!(r#"{{"files":[{rows}]}}"#).into_bytes();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_once(listener, "200 OK", payload));
    assert_eq!(
        client(address).list(&identity, "").await.err(),
        Some(ReadError::TooLarge)
    );
    let _ = server.await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let slow =
        DiaryReadClient::new(address, KEY.into(), TOKEN.into(), Duration::from_millis(20)).unwrap();
    assert_eq!(
        slow.list(&identity, "").await.err(),
        Some(ReadError::Timeout)
    );
    server.await.unwrap();
}

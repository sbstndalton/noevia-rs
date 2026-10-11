#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use noevia_model_manager::{
    docker::{decode_frames, UnixDocker},
    DockerRead, LOG_CAP,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut raw = vec![kind, 0, 0, 0];
    raw.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    raw.extend_from_slice(payload);
    raw
}
#[test]
fn multiplexed_frames_are_bounded_and_never_panicking() {
    let mut raw = frame(1, b"out\n");
    raw.extend(frame(2, b"err\n"));
    assert_eq!(decode_frames(&raw, false).unwrap(), b"out\nerr\n");
    assert!(decode_frames(&frame(3, b"attack"), false).is_err());
    assert!(decode_frames(&[1, 0, 0, 0, 255, 255, 255, 255], false).is_err());
    assert!(decode_frames(&[1, 0, 0, 0, 0, 0, 0, 20, b'x'], false).is_err());
    assert!(decode_frames(&vec![b'x'; LOG_CAP + 1], true).is_err());
    assert_eq!(decode_frames(b"raw tty", true).unwrap(), b"raw tty");
}

async fn peer(
    responses: Vec<(String, Vec<u8>)>,
) -> (tempfile::TempDir, UnixDocker, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let task = tokio::spawn(async move {
        for (expected, response) in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            while !raw.ends_with(b"\r\n\r\n") {
                let mut chunk = [0u8; 512];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                raw.extend_from_slice(&chunk[..count]);
                assert!(raw.len() < 8192);
            }
            assert!(
                String::from_utf8_lossy(&raw).starts_with(&format!("GET {expected} HTTP/1.1\r\n"))
            );
            stream.write_all(&response).await.unwrap();
        }
    });
    (dir, UnixDocker { socket }, task)
}
fn json_response(raw: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{raw}",
        raw.len()
    )
    .into_bytes()
}
#[tokio::test]
async fn real_adapter_reads_owned_unix_peer_tags_and_stopped_containers() {
    let (_dir, docker, peer) = peer(vec![
        (
            "/containers/json?all=true&limit=1025&size=false".into(),
            json_response(r#"[{"Id":"abc"}]"#),
        ),
        (
            "/containers/abc/json".into(),
            json_response(r#"{"Name":"/stopped-engine","Image":"sha256:abc"}"#),
        ),
        (
            "/images/sha256:abc/json".into(),
            json_response(r#"{"RepoTags":["LLAMA.CPP:server","unrelated"]}"#),
        ),
    ])
    .await;
    let inventory = docker.containers().await.unwrap();
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].name, "stopped-engine");
    assert_eq!(inventory[0].first_image_tag, "LLAMA.CPP:server");
    peer.await.unwrap();
}
#[tokio::test]
async fn adapter_decodes_actual_stdout_and_stderr_frames() {
    let raw = frame(2, b"stderr\n");
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        raw.len()
    )
    .into_bytes();
    response.extend(raw);
    let (_dir,docker,peer)=peer(vec![
        ("/containers/synthetic/json".into(),json_response(r#"{"Config":{"Tty":false}}"#)),
        ("/containers/synthetic/logs?stdout=true&stderr=true&timestamps=false&follow=false&tail=400".into(),response),
    ]).await;
    assert_eq!(docker.logs("synthetic", 400).await.unwrap(), b"stderr\n");
    peer.await.unwrap();
}
#[tokio::test]
async fn protocol_refusals_and_cap_precede_deserialization() {
    for response in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\n\r\n".to_vec(),
        b"HTTP/1.1 302 Found\r\nLocation: http://private-secret\r\nContent-Length: 0\r\n\r\n"
            .to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            .to_vec(),
    ] {
        let (_dir, docker, peer) = peer(vec![(
            "/containers/json?all=true&limit=1025&size=false".into(),
            response,
        )])
        .await;
        assert!(docker.containers().await.is_err());
        peer.await.unwrap();
    }
}
#[tokio::test]
async fn chunked_body_is_bounded_before_parse() {
    let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n100001\r\n".to_vec();
    response.extend(vec![b'x'; LOG_CAP + 1]);
    response.extend(b"\r\n0\r\n\r\n");
    let (_dir, docker, peer) = peer(vec![(
        "/containers/json?all=true&limit=1025&size=false".into(),
        response,
    )])
    .await;
    assert!(docker.containers().await.is_err());
    peer.await.unwrap();
}

#[tokio::test]
async fn oversized_valid_json_never_reaches_parser() {
    let raw = format!("[]{}", " ".repeat(LOG_CAP));
    let (_dir, docker, peer) = peer(vec![(
        "/containers/json?all=true&limit=1025&size=false".into(),
        json_response(&raw),
    )])
    .await;
    assert_eq!(
        docker.containers().await.unwrap_err(),
        "Docker response exceeded limit"
    );
    // The bounded reader may close before the test peer finishes sending.
    let _ = peer.await;
}

#[tokio::test]
async fn inventory_count_is_capped_while_deserializing() {
    // A malformed Id in the sentinel must never enter ContainerSummary deserialization:
    // otherwise the error changes to a protocol refusal before the later vector count.
    for last in [r#"{"Id":"abc"}"#, r#"{"Id":42}"#] {
        let raw = format!("[{},{}]", vec![r#"{"Id":"abc"}"#; 1024].join(","), last);
        let (_dir, docker, peer) = peer(vec![(
            "/containers/json?all=true&limit=1025&size=false".into(),
            json_response(&raw),
        )])
        .await;
        assert_eq!(
            docker.containers().await.unwrap_err(),
            "container inventory exceeded limit"
        );
        peer.await.unwrap();
    }
}
proptest::proptest! {
    #[test]
    fn hostile_frames_always_return(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(),0..4096)) {let _=decode_frames(&bytes,false);}
}

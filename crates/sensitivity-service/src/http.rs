use crate::{Failure, Pool, BODY_CAP};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
pub const CONNECTION_CAP: usize = 64;
struct Headers {
    method: String,
    path: String,
    length: Option<String>,
}
async fn headers(socket: &mut TcpStream) -> Result<Headers, ()> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(16384).map_err(|_| ())?;
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() == 16384 {
            return Err(());
        }
        bytes.push(socket.read_u8().await.map_err(|_| ())?);
    }
    let text: String = bytes.iter().map(|b| char::from(*b)).collect();
    let mut lines = text.split("\r\n");
    let mut first = lines.next().ok_or(())?.split_whitespace();
    let method = first.next().ok_or(())?.to_owned();
    let mut path = first.next().ok_or(())?.to_owned();
    if !matches!(first.next(), Some("HTTP/1.0" | "HTTP/1.1")) || first.next().is_some() {
        return Err(());
    }
    if path.starts_with("//") {
        path = format!("/{}", path.trim_start_matches('/'));
    }
    let mut length = None;
    for (n, line) in lines.filter(|s| !s.is_empty()).enumerate() {
        if n >= 99 {
            return Err(());
        }
        let (key, value) = line.split_once(':').ok_or(())?;
        if key.eq_ignore_ascii_case("transfer-encoding") {
            return Err(());
        }
        if key.eq_ignore_ascii_case("content-length") && length.is_none() {
            length = Some(value.trim_start_matches([' ', '\t']).to_owned());
        }
    }
    Ok(Headers {
        method,
        path,
        length,
    })
}
fn size(value: Option<&str>) -> Result<usize, u16> {
    let value = value.unwrap_or("0").trim();
    let value = value.strip_prefix('+').unwrap_or(value);
    let negative = value.starts_with('-');
    let value = value.strip_prefix('-').unwrap_or(value);
    let mut n = 0usize;
    let mut digit = false;
    let mut overflow = false;
    for b in value.bytes() {
        if b == b'_' && digit {
            digit = false;
            continue;
        }
        if !b.is_ascii_digit() {
            return Err(422);
        }
        digit = true;
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(usize::from(b - b'0')))
            .unwrap_or_else(|| {
                overflow = true;
                BODY_CAP + 1
            });
        if n > BODY_CAP {
            overflow = true;
            n = BODY_CAP + 1;
        }
    }
    if !digit {
        return Err(422);
    }
    if negative || overflow || n == 0 {
        return Err(413);
    }
    Ok(n)
}
async fn bytes_reply(
    socket: &mut TcpStream,
    status: u16,
    kind: &str,
    body: &[u8],
    retry: bool,
    head: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Request Entity Too Large",
        422 => "Unprocessable Entity",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let header=format!("HTTP/1.0 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",body.len(),if retry{"Retry-After: 1\r\n"}else{""});
    tokio::time::timeout(Duration::from_secs(3), async {
        socket.write_all(header.as_bytes()).await?;
        if !head {
            socket.write_all(body).await?;
        }
        Ok(())
    })
    .await
    .map_err(|_| std::io::Error::other("response deadline"))?
}
async fn reply(
    socket: &mut TcpStream,
    status: u16,
    body: Value,
    retry: bool,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(&body)?;
    if body.len() > BODY_CAP {
        return Err(std::io::Error::other("response cap"));
    }
    bytes_reply(socket, status, "application/json", &body, retry, false).await
}
async fn connection(mut socket: TcpStream, pool: Arc<Pool>) -> std::io::Result<()> {
    let parsed = tokio::time::timeout(Duration::from_secs(3), headers(&mut socket)).await;
    let Ok(Ok(headers)) = parsed else {
        return reply(&mut socket, 400, json!({"error":"Invalid request"}), false).await;
    };
    if headers.method == "GET" {
        if headers.path != "/health" {
            return reply(&mut socket, 404, json!({"error":"Unknown route"}), false).await;
        }
        let ready = pool.ready();
        return reply(
            &mut socket,
            if ready { 200 } else { 503 },
            json!({"ready":ready}),
            false,
        )
        .await;
    }
    if headers.method != "POST" {
        let method = headers
            .method
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let body=format!("<!DOCTYPE HTML>\n<html lang=\"en\">\n    <head>\n        <meta charset=\"utf-8\">\n        <title>Error response</title>\n    </head>\n    <body>\n        <h1>Error response</h1>\n        <p>Error code: 501</p>\n        <p>Message: Unsupported method ('{method}').</p>\n        <p>Error code explanation: 501 - Server does not support this operation.</p>\n    </body>\n</html>\n");
        return bytes_reply(
            &mut socket,
            501,
            "text/html;charset=utf-8",
            body.as_bytes(),
            false,
            headers.method == "HEAD",
        )
        .await;
    }
    if headers.path != "/v1/decisions" {
        return reply(&mut socket, 404, json!({"error":"Unknown route"}), false).await;
    }
    let length=match size(headers.length.as_deref()){Ok(n)=>n,Err(status)=>return reply(&mut socket,status,json!({"error":if status==413{"Request too large"}else{"Invalid or over-budget decision request"}}),false).await};
    let mut body = Vec::new();
    body.try_reserve_exact(length)
        .map_err(|_| std::io::Error::other("body allocation"))?;
    body.resize(length, 0);
    if !matches!(
        tokio::time::timeout(Duration::from_secs(3), socket.read_exact(&mut body)).await,
        Ok(Ok(_))
    ) {
        return reply(
            &mut socket,
            503,
            json!({"error":"Decision unavailable"}),
            false,
        )
        .await;
    }
    let mut probe = [0u8; 1];
    let result = tokio::select! {biased;_=socket.read(&mut probe)=>return Ok(()),result=pool.decide(body)=>result};
    match result {
        Ok(body) => reply(&mut socket, 200, body, false).await,
        Err(Failure::Invalid) => {
            reply(
                &mut socket,
                422,
                json!({"error":"Invalid or over-budget decision request"}),
                false,
            )
            .await
        }
        Err(Failure::Busy) => {
            reply(
                &mut socket,
                503,
                json!({"error":"Decision service busy"}),
                true,
            )
            .await
        }
        Err(_) => {
            reply(
                &mut socket,
                503,
                json!({"error":"Decision unavailable"}),
                false,
            )
            .await
        }
    }
}
pub async fn serve_with_shutdown(
    listener: TcpListener,
    pool: Arc<Pool>,
    shutdown: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    let connections = Arc::new(tokio::sync::Semaphore::new(CONNECTION_CAP));
    let mut tasks = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);
    let mut fatal = tokio::time::interval(Duration::from_millis(50));
    loop {
        let accepted = tokio::select! {_= &mut shutdown=>break,_=fatal.tick()=>{if pool.failed(){break;}continue;},done=tasks.join_next(),if !tasks.is_empty()=>{let _=done;continue;},accepted=listener.accept()=>accepted};
        let (socket, _) = accepted?;
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            drop(socket);
            continue;
        };
        let pool = pool.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let _ = connection(socket, pool).await;
        });
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    pool.stop().await;
    if pool.failed() {
        Err(std::io::Error::other("Laya worker recovery exhausted"))
    } else {
        Ok(())
    }
}

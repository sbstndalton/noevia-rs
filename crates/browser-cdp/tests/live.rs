//! The engine against a real headless Chromium:
//! `BROWSER_CDP_LIVE=http://127.0.0.1:9222 cargo test -p browser-cdp --test live -- --ignored`
//! (for example with `google-chrome --headless=new --remote-debugging-port=9222`). Two synthetic QA
//! pages are served from 127.0.0.1; nothing else is reached.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use browser_cdp::CdpLaunch;
use browser_task::engine::{Launch, Page, Request, Route};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(15);

const ONE: &str = r#"<!doctype html><html><head><title>QA one</title></head><body>
<h1>QA form page</h1>
<form method="post" action="/done"><input id="q" name="q"><button id="send" type="submit">Send QA form</button></form>
<p><a id="next" href="/two">Next QA page</a></p>
</body></html>"#;
const TWO: &str = "<!doctype html><html><head><title>QA two</title></head><body><p>Second QA page</p></body></html>";

struct Site {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn answer(mut sock: TcpStream) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if sock.read_exact(&mut b).is_err() {
            return;
        }
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let path = head.split(' ').nth(1).unwrap_or("/");
    let (status, body) = match path {
        "/" => ("200 OK", ONE),
        "/two" => ("200 OK", TWO),
        _ => ("404 Not Found", "not found"),
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(reply.as_bytes());
}

impl Site {
    fn start() -> Site {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        let _ = sock.set_nonblocking(false);
                        std::thread::spawn(move || answer(sock));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Site {
            port,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + T;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "needs BROWSER_CDP_LIVE=<http endpoint of a headless Chromium>"]
fn live_session() {
    let Ok(endpoint) = std::env::var("BROWSER_CDP_LIVE") else {
        eprintln!("BROWSER_CDP_LIVE is not set; skipped");
        return;
    };
    let site = Site::start();
    let base = format!("http://127.0.0.1:{}", site.port);

    let browser = CdpLaunch::new(endpoint).launch().unwrap();
    let context = browser.new_context(None).unwrap();
    let refused: Arc<Mutex<Vec<String>>> = Arc::default();
    let r = refused.clone();
    context
        .route(Arc::new(move |req: &Request| {
            let host = url::Url::parse(&req.url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string));
            if host.as_deref() == Some("127.0.0.1") {
                Route::Continue
            } else {
                r.lock().unwrap().push(req.url.clone());
                Route::Abort
            }
        }))
        .unwrap();
    let hooked: Arc<Mutex<Vec<u64>>> = Arc::default();
    let h = hooked.clone();
    context.on_page(Arc::new(move |page: Arc<dyn Page>| {
        h.lock().unwrap().push(page.id())
    }));
    let page = context.new_page().unwrap();
    assert_eq!(*hooked.lock().unwrap(), [page.id()]);
    assert_eq!(page.url(), "about:blank");

    page.goto(&format!("{base}/"), T).unwrap();
    assert_eq!(page.url(), format!("{base}/"));
    let send = page.locate("#send", T).unwrap().expect("the submit button");
    let facts = send.describe().unwrap();
    assert_eq!(facts.get("tag").as_str(), Some("button"));
    assert_eq!(facts.get("type").as_str(), Some("submit"));
    assert_eq!(facts.get("inForm").as_bool(), Some(true));
    assert_eq!(facts.get("formMethod").as_str(), Some("post"));
    assert_eq!(
        facts.get("formAction").as_str(),
        Some(format!("{base}/done").as_str())
    );
    assert_eq!(facts.get("text").as_str(), Some("Send QA form"));
    assert_eq!(facts.get("frameOrigin").as_str(), Some(base.as_str()));
    assert!(send.is_connected().unwrap());
    send.dispose().unwrap();
    assert!(page
        .locate("#missing", Duration::from_millis(300))
        .unwrap()
        .is_none());

    let input = page.locate("#q", T).unwrap().unwrap();
    input.fill("qa words", T).unwrap();
    assert!(input.is_focused().unwrap());
    input.dispose().unwrap();

    let link = page.locate("#next", T).unwrap().unwrap();
    link.click(T).unwrap();
    link.dispose().unwrap();
    wait_for("the second page", || {
        let _ = page.wait_for_load_state(T);
        page.url() == format!("{base}/two")
    });
    assert!(page.body_text(T).unwrap().contains("Second QA page"));
    assert_eq!(page.locator_text("p", T).unwrap(), "Second QA page");
    let png = page.screenshot().unwrap();
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    page.wheel(200.0).unwrap();
    page.keyboard_press("Tab").unwrap();

    page.go_back(T).unwrap();
    assert_eq!(page.url(), format!("{base}/"));

    let blocked = page.goto("http://qa.invalid/", T).unwrap_err();
    assert!(
        blocked.message.contains("ERR_BLOCKED_BY_CLIENT"),
        "{}",
        blocked.message
    );
    assert!(refused
        .lock()
        .unwrap()
        .iter()
        .any(|u| u == "http://qa.invalid/"));

    context.close().unwrap();
    wait_for("the page to close", || page.is_closed());
    assert!(context.pages().is_empty());
    browser.close().unwrap();
    drop(site);
}

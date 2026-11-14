//! Track R14 step 6: the front's MCP call path asks `Backend::inapp_write` for a call to the
//! in-process server, with the token's account, project and edit pin and the write flag, and uses
//! its result in place of the server's; `None` calls the server as before, and other servers are
//! never asked.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use js_json::JValue;
use noevia_server::mcp_call::{execute, Backend, Credential, Failed, Request, Session};
use noevia_server::mcp_discovery_actor::ServerSpec;
use std::sync::Mutex;
use tool_rate_limit::RateLimit;

type Asked = (String, Option<String>, String, Option<String>, bool);

struct Fake {
    answer: Option<JValue>,
    asked: Mutex<Vec<Asked>>,
    called: Mutex<Vec<String>>,
}

fn text_result(text: &str) -> JValue {
    js_json::parse(&format!(
        r#"{{"content":[{{"type":"text","text":"{text}"}}]}}"#
    ))
    .unwrap()
}

impl Backend for Fake {
    fn now_ms(&self) -> f64 {
        1_700_000_000_000.0
    }
    async fn servers(&self) -> Vec<ServerSpec> {
        ["noevia", "remote"]
            .iter()
            .map(|id| ServerSpec {
                id: (*id).to_owned(),
                url: format!("http://127.0.0.1:9/{id}"),
                auth: if *id == "noevia" { "internal" } else { "none" }.to_owned(),
                directory: false,
                title: None,
                added_by: None,
                token_env: None,
            })
            .collect()
    }
    async fn key_headers(&self, _: &str, _: &str) -> Credential<Vec<(String, String)>> {
        Credential::Absent
    }
    async fn oauth_session(&self, _: &str, _: &str, _: Option<&str>) -> Credential<Session> {
        Credential::Absent
    }
    async fn reject(&self, _: &str, _: &str, _: &str) {}
    async fn directory_headers(&self, _: &str) -> Vec<(String, String)> {
        Vec::new()
    }
    fn static_token(&self, _: &str) -> Option<String> {
        None
    }
    fn internal_token(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: bool,
        _: Option<&str>,
    ) -> Option<String> {
        Some("qa-synthetic-token".into())
    }
    async fn url_public(&self, _: &str) -> bool {
        true
    }
    async fn inapp_write(
        &self,
        uid: &str,
        pid: Option<&str>,
        name: &str,
        _: &JValue,
        target: Option<&str>,
        write: bool,
    ) -> Option<JValue> {
        self.asked.lock().unwrap().push((
            uid.to_owned(),
            pid.map(str::to_owned),
            name.to_owned(),
            target.map(str::to_owned),
            write,
        ));
        self.answer.clone()
    }
    async fn call(
        &self,
        server: &ServerSpec,
        _: &[(String, String)],
        name: &str,
        _: &JValue,
    ) -> Result<JValue, Failed> {
        self.called
            .lock()
            .unwrap()
            .push(format!("{}:{name}", server.id));
        Ok(text_result("from the server"))
    }
}

fn request(server: &str, name: &str, write: bool, target: Option<&str>) -> Request {
    Request {
        user: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        workspace_user: Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into()),
        server: server.into(),
        name: name.into(),
        arguments: js_json::parse(r#"{"name":"qa.md","text":"QA"}"#).unwrap(),
        project: Some("qa-p".into()),
        edit_target: target.map(str::to_owned),
        write,
        nextcloud: None,
    }
}

#[tokio::test]
async fn the_front_answers_an_in_app_write_and_otherwise_the_server_does() {
    let limits = Mutex::new(RateLimit::default());
    let native = Fake {
        answer: Some(text_result("Created \\\"qa.md\\\" (2 chars).")),
        asked: Mutex::new(Vec::new()),
        called: Mutex::new(Vec::new()),
    };
    let a = execute(
        &native,
        &limits,
        &request("noevia", "project_append_file", true, Some("pin")),
    )
    .await;
    assert!(a.text.contains("Created"), "{}", a.text);
    assert!(
        native.called.lock().unwrap().is_empty(),
        "the server is not called"
    );
    assert_eq!(
        native.asked.lock().unwrap().clone(),
        vec![(
            "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_owned(),
            Some("qa-p".to_owned()),
            "project_append_file".to_owned(),
            Some("pin".to_owned()),
            true
        )]
    );
    // Another server is never asked.
    execute(
        &native,
        &limits,
        &request("remote", "project_append_file", true, None),
    )
    .await;
    assert_eq!(native.asked.lock().unwrap().len(), 1);
    assert_eq!(
        native.called.lock().unwrap().clone(),
        vec!["remote:project_append_file".to_owned()]
    );
    // `None`: the in-process server answers as before.
    let core = Fake {
        answer: None,
        asked: Mutex::new(Vec::new()),
        called: Mutex::new(Vec::new()),
    };
    let a = execute(
        &core,
        &limits,
        &request("noevia", "project_create_file", true, None),
    )
    .await;
    assert!(a.text.contains("from the server"), "{}", a.text);
    assert_eq!(
        core.called.lock().unwrap().clone(),
        vec!["noevia:project_create_file".to_owned()]
    );
    // A call without write approval carries no pin; one without a workspace is never the front's.
    let quiet = Fake {
        answer: None,
        asked: Mutex::new(Vec::new()),
        called: Mutex::new(Vec::new()),
    };
    execute(
        &quiet,
        &limits,
        &request("noevia", "project_append_file", false, Some("pin")),
    )
    .await;
    assert_eq!(quiet.asked.lock().unwrap()[0].3, None);
    assert!(!quiet.asked.lock().unwrap()[0].4);
    let mut nobody = request("noevia", "project_append_file", true, Some("pin"));
    nobody.workspace_user = Some(String::new());
    execute(&quiet, &limits, &nobody).await;
    assert_eq!(quiet.asked.lock().unwrap().len(), 1);
}

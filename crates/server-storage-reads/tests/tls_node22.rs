//! Narrow actual builtin-fetch TLS observations, not npm Undici or route parity.
#![allow(clippy::duplicate_mod)] // Both included fixture and oracle own the fixed runner.
#[allow(dead_code)]
#[path = "support/tls_lab.rs"]
mod lab;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{net::TcpListener, process::Command, time::Duration};
const NODE: &str = "/opt/homebrew/opt/node@22/bin/node";
const SEAM: &str = r#"const fs=require('node:fs');const x=JSON.parse(fs.readFileSync(0,'utf8'));const C=require('node:crypto').X509Certificate;const ca=new C(fs.readFileSync(x.ca));const leaf=new C(fs.readFileSync(x.leaf));const v={node:process.version,undici:process.versions.undici,leafSelfIssued:leaf.subject===leaf.issuer,issuerMatchesCA:leaf.issuer===ca.subject};fetch(x.url,{method:x.method,headers:x.override?{host:'different.synthetic.invalid'}:{},redirect:'manual'}).then(async r=>{const b=await r.text();console.log(JSON.stringify({...v,status:r.status,body:b}));}).catch(e=>{const c=e?.cause?.code;const known=['ERR_TLS_CERT_ALTNAME_INVALID','UNABLE_TO_VERIFY_LEAF_SIGNATURE','SELF_SIGNED_CERT_IN_CHAIN','UNABLE_TO_GET_ISSUER_CERT_LOCALLY','DEPTH_ZERO_SELF_SIGNED_CERT','CERT_SIGNATURE_FAILURE','CERT_HAS_EXPIRED','ERR_SSL_NO_SUITABLE_SIGNATURE_ALGORITHM','ERR_SSL_TLSV1_ALERT_INTERNAL_ERROR','ERR_SSL_SSLV3_ALERT_HANDSHAKE_FAILURE','ECONNREFUSED','ECONNRESET','EPROTO','UND_ERR_SOCKET','UND_ERR_CONNECT_TIMEOUT'];console.log(JSON.stringify({...v,refused:true,failure:known.includes(c)?c:'other',causePresent:!!e.cause,causeCodePresent:typeof c==='string'}));});"#;
#[test]
fn actual_builtin_fetch_owned_tls_observations() {
    // Fixed installed Node 22 observations write retained captures under /tmp; they run only on
    // an explicitly prepared host (M4_NODE22_ORACLE=1), never in ordinary CI.
    if std::env::var("M4_NODE22_ORACLE").as_deref() != Ok("1") {
        node22_skipped("actual_builtin_fetch_owned_tls_observations");
        return;
    }
    let fixture = lab::pki::Fixture::generate().unwrap_or_else(|_| unreachable!());
    for (trusted, method, host_override) in [
        (true, "GET", true),
        (true, "GET", false),
        (true, "PROPFIND", false),
        (false, "GET", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|_| unreachable!());
        listener
            .set_nonblocking(true)
            .unwrap_or_else(|_| unreachable!());
        let port = listener
            .local_addr()
            .unwrap_or_else(|_| unreachable!())
            .port();
        let config = fixture.server(false).unwrap_or_else(|_| unreachable!());
        std::thread::scope(|scope| {
            let server = scope.spawn(move || {
                server_storage_reads::child_http::runtime()
                    .unwrap_or_else(|_| unreachable!())
                    .block_on(lab::serve(listener, config, false, lab::response(1), false))
            });
            let mut command = Command::new(NODE);
            command.env_clear().args(["-e", SEAM]);
            if trusted {
                command.env("NODE_EXTRA_CA_CERTS", fixture.directory.join("ca.pem"));
            }
            let input=process::json_input(&serde_json::json!({"url":format!("https://127.0.0.1:{port}/signed%2Fpath?X=a%2Fb"),"method":method,"override":host_override,"ca":fixture.directory.join("ca.pem"),"leaf":fixture.directory.join("ip.der")})).unwrap_or_else(|_|unreachable!());
            let output = process::bounded(command, input, Duration::from_secs(15))
                .unwrap_or_else(|_| unreachable!());
            assert!(output.status.success(), "CONTROL Node process");
            let observed: serde_json::Value =
                serde_json::from_slice(&output.stdout).unwrap_or_else(|_| unreachable!());
            assert_eq!(
                observed.get("node").and_then(serde_json::Value::as_str),
                Some("v22.23.3")
            );
            assert!(observed
                .get("undici")
                .is_some_and(serde_json::Value::is_string));
            let report = server.join().unwrap_or_else(|_| unreachable!());
            println!("Node22 trusted={trusted} method={method} host_override={host_override} observed={observed} accepted={} identity={} input_bytes={}",report.accepted,report.identity,report.input.len());
            assert_eq!(
                observed
                    .get("leafSelfIssued")
                    .and_then(serde_json::Value::as_bool),
                Some(false)
            );
            assert_eq!(
                observed
                    .get("issuerMatchesCA")
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );
            if trusted {
                assert_eq!(
                    observed.get("status").and_then(serde_json::Value::as_u64),
                    Some(200)
                );
                assert_eq!(
                    observed.get("body").and_then(serde_json::Value::as_str),
                    Some("x")
                );
                assert!(report.accepted && report.identity);
            } else {
                assert_eq!(
                    observed.get("refused").and_then(serde_json::Value::as_bool),
                    Some(true)
                );
                assert!(report.input.is_empty());
            }
            println!(
                "Node22 builtin TLS trusted={trusted} method={method} undici={}",
                observed.get("undici").unwrap_or(&serde_json::Value::Null)
            );
        });
    }
}

/// Record a skipped Node 22 observation so CI can count and show it (noevia#1827).
fn node22_skipped(test: &str) {
    eprintln!("M4 oracle skipped: {test} (set M4_NODE22_ORACLE=1 to run it)");
    if let Some(log) = std::env::var_os("M4_ORACLE_SKIP_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(f, "{test}");
        }
    }
}

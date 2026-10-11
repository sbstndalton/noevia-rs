//! Actual pinned storage-client oracle. No policy is copied into JavaScript.
//! Requires explicit synthetic-only checkout; refuses source drift. No server starts.
use server_storage_reads::{json, protocol::*};
use std::{collections::VecDeque, error::Error, process::Command, time::Duration};
use zeroize::Zeroizing;

mod support;
const SEAM: &str = r#"
const fs=require('node:fs'), client=require(process.argv[1]+'/server/storage-client.cjs');
(async()=>{const cases=JSON.parse(fs.readFileSync(0,'utf8')),out=[];
for(const c of cases){const requests=[];let i=0;global.fetch=async(u,o)=>{requests.push([o.method||'GET',String(u)]);const r=c.responses[i++];if(!r)throw Error('fixture exhausted');return new Response(Buffer.from(r.body,'utf8'),{status:r.status});};
try{const value=await (c.op==='list'?client.listFiles:client.readTextFile)(c.conn,JSON.parse(c.path));out.push({body:JSON.stringify(value),requests});}
catch(e){out.push({body:JSON.stringify({error:e.message}),status:e.status||502,requests});}}
process.stdout.write(JSON.stringify(out));})().catch(()=>{process.exitCode=1});
"#;

struct FixtureTransport {
    responses: VecDeque<(u16, String)>,
    requests: Vec<(String, String)>,
}
impl Transport for FixtureTransport {
    fn elapsed(&self) -> Duration {
        Duration::ZERO
    }
    fn amz_date(&self) -> String {
        "20261011T120000Z".into()
    }
    fn delay(&mut self, _: Duration) -> Result<(), TransportFailure> {
        Err(TransportFailure::Other)
    }
    fn attempt(&mut self, r: Request) -> Result<Response, TransportFailure> {
        let cap = r.body_cap;
        self.requests.push((
            if r.method == Method::Get {
                "GET"
            } else {
                "PROPFIND"
            }
            .into(),
            r.url,
        ));
        let (status, body) = self.responses.pop_front().ok_or(TransportFailure::Other)?;
        Ok(Response {
            status,
            body: decoded_fixture(body.as_bytes(), cap).map_err(|_| TransportFailure::Body)?,
        })
    }
}

#[test]
fn actual_client_deterministic_domain_zero_diff() -> Result<(), Box<dyn Error>> {
    let root = std::env::var("M4_CORE_ORACLE")?;
    // These are the indirect local collaborators loaded by the actual client.
    // Compare bytes, not HEAD labels; active M6 commits are not the source base.
    for file in [
        "storage-client.cjs",
        "storage-path.cjs",
        "dav-listing.cjs",
        "dav-parse-wasm.cjs",
        "dav-parse.lock",
        "s3-listing.cjs",
        "s3-sign.cjs",
        "s3-region.cjs",
        "http.cjs",
    ] {
        support::checked_source(&root, file)?;
    }
    let dav = "<multistatus><response><href>/dav/docs/Z.txt</href><getcontentlength>007</getcontentlength></response><response><href>/dav/docs/a.md</href><getcontentlength>9007199254740993</getcontentlength></response><response><href>https://elsewhere.invalid/dav/docs/skip.txt</href></response></multistatus>";
    let s3 = "<ListBucketResult><CommonPrefixes><Prefix>diary/docs/sub/</Prefix></CommonPrefixes><Contents><Key>diary/docs/z.txt</Key><Size>3</Size></Contents></ListBucketResult>";
    let mut cases = Vec::new();
    let mut native = Vec::new();
    for kind in [Kind::WebDav, Kind::Nextcloud, Kind::S3] {
        for (op, path, status, body) in [
            (
                Operation::List,
                "\"docs\"",
                207,
                if kind == Kind::S3 { s3 } else { dav },
            ),
            (Operation::List, "\"../x\"", 200, ""),
            (Operation::Read, "\"docs/é.txt\"", 200, "hello 😀\n"),
            (Operation::Read, "\"x.bin\"", 200, ""),
            (Operation::Read, "\"\\ud800.txt\"", 200, ""),
            (Operation::Read, "\"\\ud800\"", 200, ""),
            (Operation::Read, "[\"x.txt\"]", 200, "array coerced"),
            (Operation::Read, "{\"valueOf\":false}", 200, ""),
            (Operation::Read, "{\"toString\":false}", 200, ""),
            (Operation::Read, "\"x.txt\"", 404, ""),
            (Operation::List, "\"docs\"", 500, ""),
        ] {
            let kind_name = match kind {
                Kind::WebDav => "webdav",
                Kind::Nextcloud => "nextcloud",
                Kind::S3 => "s3",
                Kind::Local => "local",
            };
            let conn = serde_json::json!({"kind":kind_name,"baseUrl":"https://storage.invalid/dav","username":"synthetic","secret":"fixture-only","corpusRoot":"diary","bucket":"bucket","region":"us-east-1"});
            cases.push(serde_json::json!({"op":if op == Operation::List { "list" } else { "read" },"path":path,"conn":conn,"responses":[{"status":status,"body":body}]}));
            let c = Connection {
                kind,
                base_url: "https://storage.invalid/dav".into(),
                username: "synthetic".into(),
                secret: Zeroizing::new(b"fixture-only".to_vec()),
                corpus_root: "diary".into(),
                bucket: "bucket".into(),
                region: "us-east-1".into(),
            };
            let mut transport = FixtureTransport {
                responses: vec![(status, body.into())].into(),
                requests: Vec::new(),
            };
            let path_value = json::parse(path.as_bytes())?;
            let result =
                json::coerce_path(&path_value).and_then(|p| execute(&c, &p, op, &mut transport));
            native.push(match result {
                Ok(body) => serde_json::json!({"body":body,"requests":transport.requests}),
                Err(e) => serde_json::json!({"body":e.json(),"status":e.status,"requests":transport.requests}),
            });
        }
    }
    let mut command =
        Command::new(std::env::var("M4_NODE").unwrap_or_else(|_| "/opt/homebrew/bin/node".into()));
    command
        .args(["-e", SEAM, &root])
        .env("STORAGE_PATH_IMPL", "js")
        .env("S3_SIGN_IMPL", "js");
    let input = support::process::json_input(&serde_json::Value::Array(cases.clone()))?;
    let output = support::run(command, input)?;
    if !output.status.success() {
        return Err("actual Node oracle failed (diagnostics withheld)".into());
    }
    let expected: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    assert_eq!(expected.len(), native.len());
    for (i, (expected, actual)) in expected.iter().zip(&native).enumerate() {
        assert_eq!(actual, expected, "actual client case {i}");
    }
    // Deliberately wrong response bytes and request destination must be detected.
    let mut response_mutant = native.clone();
    if let Some(case) = response_mutant.first_mut() {
        *case = serde_json::json!({"body":"[]","requests":[]});
    }
    assert_ne!(response_mutant, expected, "response omission control");
    let mut destination_mutant = native.clone();
    if let Some(case) = destination_mutant.get_mut(2) {
        *case = serde_json::json!({"body":case.get("body"),"requests":[["GET","https://elsewhere.invalid/"]]});
    }
    assert_ne!(destination_mutant, expected, "destination control");
    Ok(())
}

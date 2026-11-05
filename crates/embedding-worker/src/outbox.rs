//! Durable source indexing; embedding refresh/migration remains the embedding worker's job.
use crate::{
    fill::Store,
    sqlite::{SourceStore, SqlStore},
    Control, Index,
};
use server_projects::outbox::IndexClaim;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

fn fault() -> String {
    "source indexing unavailable".into()
}
fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// The Node text chunker counts UTF-16 code units, including its overlap.
pub fn chunks(text: &str) -> Vec<String> {
    fn space(c: char) -> bool {
        matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')
    }

    fn plain(text: &str) -> Vec<String> {
        let clean = text.replace("\r\n", "\n");
        let units: Vec<u16> = clean.trim_matches(space).encode_utf16().collect();
        let mut out = Vec::new();
        let mut start = 0;
        while start < units.len() {
            let mut end = (start + 1200).min(units.len());
            if end < units.len() {
                let window = units.get(start..end).unwrap_or_default();
                let para = window.windows(2).rposition(|w| w == [10, 10]);
                if let Some(p) = para.filter(|p| *p > 600) {
                    end = start + p + 1;
                } else if let Some(p) = (600..window.len())
                    .rev()
                    .find(|p| matches!(window.get(*p), Some(46 | 33 | 63 | 10)))
                {
                    end = start + p + 1;
                }
            }
            let piece = String::from_utf16_lossy(units.get(start..end).unwrap_or_default())
                .trim_matches(space)
                .to_owned();
            if !piece.is_empty() {
                out.push(piece);
            }
            if end == units.len() {
                break;
            }
            start = end.saturating_sub(150).max(start + 1);
        }
        out
    }
    fn marker(text: &str) -> Option<usize> {
        let rest = text.strip_prefix("[Page ")?;
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0 && rest.get(digits..)?.starts_with(']')).then_some(7 + digits)
    }
    if marker(text).is_none() {
        return plain(text);
    }
    let mut offsets = vec![0];
    for (i, _) in text.match_indices("[Page ") {
        if i > 0
            && marker(text.get(i..).unwrap_or_default())
                .is_some_and(|n| text.get(i + n..).unwrap_or_default().starts_with('\n'))
        {
            offsets.push(i);
        }
    }
    offsets.push(text.len());
    offsets
        .windows(2)
        .flat_map(|w| {
            let start = w.first().copied().unwrap_or(0);
            let end = w.get(1).copied().unwrap_or(start);
            let part = text.get(start..end).unwrap_or_default();
            let n = marker(part).unwrap_or(0);
            plain(part.get(n..).unwrap_or_default())
                .into_iter()
                .map(move |s| format!("{}\n{s}", part.get(..n).unwrap_or_default()))
        })
        .collect()
}

impl SqlStore {
    /// Update only the still-current source's document status under the shared
    /// projects lock. Account removal is fenced by the auth writer as in vector publication.
    pub fn mark_index_source(
        &self,
        user: &str,
        row: &IndexClaim,
        success: bool,
        prepared: bool,
    ) -> Result<(), String> {
        if row.kind != "index" {
            return Ok(());
        }
        let dir = server_projects::store::user_dir(&self.root, user).map_err(|_| fault())?;
        let writer = server_store::Writer::open(&self.root, server_store::RustAuth::owned())
            .map_err(|_| fault())?;
        writer.write(|auth| {
            if auth.reader().row("SELECT id FROM users WHERE id=?",[user],|r|r.get::<_,String>(0))?.is_none() {return Ok(Ok(()));}
            let result=server_projects::store::update(&dir,server_store::RustProjects::owned(),|projects| {
                let current=crate::sqlite::open_existing(&dir.join("index-outbox.db")).ok().and_then(|db|server_projects::outbox::index_current(&db,row).ok()).unwrap_or(false);
                if !current {return ((),false);}
                let Some(project)=server_projects::store::find_mut(projects,&row.project) else {return ((),false)};
                let js_json::JValue::Obj(fields)=project else {return ((),false)};
                let Some((_,js_json::JValue::Arr(files)))=fields.iter_mut().find(|(k,_)|k=="files") else {return ((),false)};
                let Some(file)=files.iter_mut().find(|f|f.get("name").as_str()==row.file.as_deref()) else {return ((),false)};
                let state=if !prepared {"unavailable"} else if success {
                    let index=Index{user:Some(user.into()),project:row.project.clone()};
                    let counts=self.index_path(&index).ok().and_then(|p|crate::sqlite::open_existing(&p).ok()).and_then(|db|db.query_row("SELECT COUNT(*),COALESCE(MAX(direct),0) FROM chunks WHERE file=? AND COALESCE(kind,'text')='text'",[row.file.as_deref().unwrap_or_default()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).ok());
                    match counts {Some((0,_))=>"unavailable",Some((_,1))=>"direct",Some(_)=>{
                        let remaining=self.config().ok().and_then(|c|SourceStore{store:self,row}.remaining(&index,&c.active).ok());
                        if remaining==Some(0) {"ready"} else {"partial"}
                    },None=>"unavailable"}
                } else {"partial"};
                let js_json::JValue::Obj(_) = file.get("document") else {return ((),false)};
                if file.get("document").get("indexing").as_str()==Some(state) {return ((),false);}
                if let js_json::JValue::Obj(fields)=file {
                    if let Some((_,document))=fields.iter_mut().find(|(k,_)|k=="document") {server_projects::store::set(document,"indexing",js_json::JValue::Str(state.into()));}
                }
                ((),true)
            }).map_err(|_|fault());
            Ok(result)
        }).map_err(|_|fault())?
    }

    /// Authorize, lock the project snapshot, then fence the outbox generation
    /// through the SQLite commit. No lock is retained during embedding HTTP.
    pub fn index_source(
        &self,
        user: &str,
        row: &IndexClaim,
        control: &Control,
    ) -> Result<Option<Index>, String> {
        let index = Index {
            user: Some(user.into()),
            project: row.project.clone(),
        };
        let dir = server_projects::store::user_dir(&self.root, user).map_err(|_| fault())?;
        // Match the vector publisher's config -> auth -> projects -> outbox ->
        // index lock order, including legacy slot adoption during index creation.
        let _config = server_projects::lock::acquire(&self.root.join("embedding-config.json"))
            .map_err(|_| fault())?;
        let writer = server_store::Writer::open(&self.root, server_store::RustAuth::owned())
            .map_err(|_| fault())?;
        writer.write(|auth| {
            if auth.reader().row("SELECT id FROM users WHERE id=?",[user],|r|r.get::<_,String>(0))?.is_none() {return Ok(Ok(None));}
            Ok((|| {
                let _projects=server_projects::lock::acquire(&dir.join("projects.json")).map_err(|_|fault())?;
                let mut outbox=crate::sqlite::open_existing(&dir.join("index-outbox.db"))?;
                let fence=outbox.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|_|fault())?;
                if !server_projects::outbox::index_current(&fence,row).map_err(|_|fault())? {return Ok(None);}
                let projects=server_projects::store::read(&dir).map_err(|_|fault())?;
                let Some(project)=server_projects::store::find(&projects,&row.project) else {return Ok(None)};
                let Some(name)=row.file.as_deref() else {return Ok(None)};
                let files=match project.get("files") {js_json::JValue::Arr(files)=>files.as_slice(),_=>&[]};
                let file=files.iter().find(|f|f.get("name").as_str()==Some(name));
                if (row.kind=="index" && file.is_none()) || (row.kind=="unindex" && file.is_some()) {return Ok(None);}
                let (text,unreadable,skill)=if let Some(file)=file {
                    let text=server_projects::sources::text(&dir,&row.project,file).map_err(|_|fault())?;
                    // The lossless project reader maps lone UTF-16 surrogates to
                    // private scalars; SQLite/UTF-8 hashing use Node's replacement bytes.
                    let text=if matches!(file.get("content"),js_json::JValue::Str(_)) {text.chars().map(|c|if js_json::wtf16::lone_unit(c).is_some() {'\u{fffd}'} else {c}).collect()} else {text};
                    let unreadable=file.get("attachment").get("state").as_str()==Some("stored") && matches!(file.get("attachment").get("group").as_str(),Some("Text"|"Documents"));
                    let skill=server_projects::skills::is_front_matter_skill(name,&text) || file.get("skillHeader")==&js_json::JValue::Bool(true);
                    (text,unreadable,skill)
                } else {(String::new(),false,false)};
                let empty=text.is_empty() || unreadable || skill;
                control.renew_publication_proof()?;
                if !control.ownership_fresh() {return Err(fault());}
                let path=self.index_path(&index)?;
                // Removing unreadable text must not create a new empty database.
                if empty && !path.exists() {return Ok(None);}
                std::fs::create_dir_all(self.index_dir(&index)?).map_err(|_|fault())?;
                crate::sqlite_vec_loader::register()?;
                let mut db=rusqlite::Connection::open(path).map_err(|_|fault())?;
                db.busy_timeout(std::time::Duration::from_secs(5)).map_err(|_|fault())?;
                db.pragma_update(None,"journal_mode","WAL").map_err(|_|fault())?;
                let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|_|fault())?;
                tx.execute_batch("CREATE TABLE IF NOT EXISTS chunks(id INTEGER PRIMARY KEY,file TEXT NOT NULL,chunk_no INTEGER NOT NULL,body TEXT NOT NULL,content_hash TEXT NOT NULL,updated_at TEXT NOT NULL DEFAULT(datetime('now'))); CREATE UNIQUE INDEX IF NOT EXISTS idx_chunks_file_hash ON chunks(file,content_hash); CREATE TABLE IF NOT EXISTS vec_meta(key TEXT PRIMARY KEY,value TEXT);").map_err(|_|fault())?;
                self.prepare(&tx)?;
                let version=hash(&text);
                let direct=text.encode_utf16().count()<=2400;
                // Keep same-version document tail rows and every media row.
                let keep_tail=!empty && !direct;
                let ids={let mut stmt=tx.prepare("SELECT id FROM chunks WHERE file=?1 AND (?4 OR COALESCE(kind,'text')='text') AND NOT (?2 AND COALESCE(meta,'') LIKE '{\"tail\":1,%' AND version IS ?3)").map_err(|_|fault())?;
                    let collected=stmt.query_map(rusqlite::params![name,keep_tail,version,row.kind=="unindex"],|r|r.get::<_,i64>(0)).map_err(|_|fault())?.collect::<Result<Vec<_>,_>>().map_err(|_|fault())?; collected};
                self.drop_chunks(&tx,&ids)?;
                let mut seen=BTreeSet::new();
                let pieces=if empty {Vec::new()} else if direct {vec![text]} else {chunks(&text)};
                for (n,body) in pieces.into_iter().filter(|b|seen.insert(hash(b))).enumerate() {
                    // Head/tail duplicate hashes are replaced by the head.
                    let clash=tx.query_row("SELECT id FROM chunks WHERE file=? AND content_hash=? AND COALESCE(meta,'') LIKE '{\"tail\":1,%'",rusqlite::params![name,hash(&body)],|r|r.get::<_,i64>(0));
                    if let Ok(id)=clash {self.drop_chunks(&tx,&[id])?;}
                    tx.execute("INSERT INTO chunks(file,chunk_no,body,content_hash,version,direct) VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![name,i64::try_from(n).map_err(|_|fault())?,body,hash(&body),version,direct]).map_err(|_|fault())?;
                }
                control.renew_publication_proof()?;
                tx.commit().map_err(|_|fault())?;
                Ok(if empty {None} else {Some(index)})
            })())
        }).map_err(|_|fault())?
    }
}

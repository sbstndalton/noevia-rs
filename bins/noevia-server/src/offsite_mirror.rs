//! The copy of the local encrypted backup store to the administrator's Google Drive
//! (sbstndalton/noevia#2190 round 7), ported from core `gdrive.cjs` `mirror`. Scope `drive.file`:
//! noevia sees only what it created. Every object is already sealed by the engine, so Drive holds
//! nothing readable.
//!
//! The safety rules are core's: refuse a store that is not a healthy backup (a wiped disk must never
//! wipe Drive), upload first and prune second, never delete more than 500 objects or a quarter of
//! what is on Drive in one run (a bigger share means the local store is not the one copied there),
//! never prune a folder that holds a different store (identified by a hash of its `config` object,
//! kept as a folder property and in the token file), and treat a same-named object of a different
//! size as corruption rather than overwriting it. A folder that holds another store is left alone and
//! the new one goes to a sibling `noevia-offsite (2)`.
use crate::gdrive::{
    answered, encode_uri_component, hex_of, random, ApiRequest, Drive, Fail, Slot,
};
use crate::offsite_store::DirStore;
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

pub const FOLDER: &str = "noevia-offsite";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
pub const MAX_DELETE: usize = 500;
const MAX_DELETE_SHARE: f64 = 0.25;
const SHARE_FLOOR: f64 = 8.0;
const STORE_PROP: &str = "noeviaStoreId";

pub struct MirrorResult {
    pub uploaded: usize,
    pub bytes: u64,
    pub removed: usize,
    pub snapshots: usize,
    pub folder: String,
    pub new_folder: bool,
}

struct Folder {
    id: String,
    name: String,
    created: bool,
}

struct Remote {
    id: String,
    size: Option<u64>,
}

fn store_empty() -> Fail {
    Fail::new(
        "The local backup store looks empty, so nothing was copied (this protects the copy on Drive).",
        409,
    )
    .with_id("storeEmpty", vec![])
}

/// `^noevia-offsite(?: \((\d+)\))?$`: the folder's number (1 for the plain name).
fn folder_number(name: &str) -> Option<u64> {
    if name == FOLDER {
        return Some(1);
    }
    let rest = name
        .strip_prefix(FOLDER)?
        .strip_prefix(" (")?
        .strip_suffix(')')?;
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .then(|| rest.parse().ok())
        .flatten()
}

struct Mirror<'a> {
    drive: &'a Arc<Drive>,
    api: String,
    upload: String,
}

impl Mirror<'_> {
    async fn call(
        &self,
        method: &str,
        url: &str,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> Result<JValue, Fail> {
        self.drive
            .call(&Slot::Backup, method, url, content_type, body)
            .await
    }

    fn query(&self, q: &str) -> String {
        format!("{}/files?q={}", self.api, encode_uri_component(q))
    }

    /// Whether a Drive folder holds a copy of the store whose `config` object is `local_config`.
    async fn folder_matches(
        &self,
        folder: &JValue,
        store_id: &str,
        local_config: &[u8],
    ) -> Result<bool, Fail> {
        if let Some(stamped) = folder
            .get("appProperties")
            .get(STORE_PROP)
            .as_str()
            .filter(|s| !s.is_empty())
        {
            return Ok(stamped == store_id);
        }
        // An older (unstamped) folder: compare its `config` object byte for byte.
        let id = folder.get("id").as_str().unwrap_or("");
        let q = format!("'{id}' in parents and trashed=false");
        let listed = self
            .call(
                "GET",
                &format!("{}&fields=files(id,name)&pageSize=1000", self.query(&q)),
                None,
                None,
            )
            .await?;
        let JValue::Arr(files) = listed.get("files") else {
            return Ok(true);
        };
        if files.is_empty() {
            return Ok(true); // empty: nothing to lose, adopt it
        }
        let Some(cfg) = files
            .iter()
            .find(|f| f.get("name").as_str() == Some("config"))
        else {
            return Ok(false);
        };
        let url = format!(
            "{}/files/{}?alt=media",
            self.api,
            encode_uri_component(cfg.get("id").as_str().unwrap_or(""))
        );
        // Only a reply of exactly the local config's size can match, so read one byte more than that.
        let r = self
            .drive
            .api(
                &Slot::Backup,
                ApiRequest {
                    method: "GET",
                    url: &url,
                    headers: vec![],
                    body: None,
                    cap: local_config.len() + 1,
                    budget: crate::gdrive::Budget::Quick,
                },
            )
            .await?;
        if !(200..300).contains(&r.status) {
            return Err(answered(r.status));
        }
        Ok(!r.capped && r.body == local_config)
    }

    /// The Drive folder that holds THIS store.
    async fn folder_id(&self, store_id: &str, local_config: &[u8]) -> Result<Folder, Fail> {
        let drive = self.drive;
        if let (Some(fid), Some(sid)) = (
            drive.saved_field(&Slot::Backup, "folderId"),
            drive.saved_field(&Slot::Backup, "storeId"),
        ) {
            if sid == store_id {
                let url = format!(
                    "{}/files/{}?fields=id,name,trashed",
                    self.api,
                    encode_uri_component(&fid)
                );
                if let Ok(f) = self.call("GET", &url, None, None).await {
                    if f.get("trashed").as_bool() != Some(true) {
                        let name = f
                            .get("name")
                            .as_str()
                            .filter(|n| !n.is_empty())
                            .unwrap_or(FOLDER)
                            .to_owned();
                        return Ok(Folder {
                            id: fid,
                            name,
                            created: false,
                        });
                    }
                }
            }
        }
        let q = format!("name contains '{FOLDER}' and mimeType='{FOLDER_MIME}' and trashed=false");
        let found = self
            .call(
                "GET",
                &format!(
                    "{}&fields=files(id,name,appProperties)&pageSize=100",
                    self.query(&q)
                ),
                None,
                None,
            )
            .await?;
        let mut candidates: Vec<(u64, JValue)> = match found.get("files") {
            JValue::Arr(files) => files
                .iter()
                .filter_map(|f| {
                    folder_number(f.get("name").as_str().unwrap_or("")).map(|n| (n, f.clone()))
                })
                .collect(),
            _ => Vec::new(),
        };
        candidates.sort_by_key(|(n, _)| *n);
        let mut hit: Option<JValue> = None;
        for (_, f) in &candidates {
            if self.folder_matches(f, store_id, local_config).await? {
                hit = Some(f.clone());
                break;
            }
        }
        let folder = if let Some(f) = hit {
            let id = f.get("id").as_str().unwrap_or("").to_owned();
            if f.get("appProperties").get(STORE_PROP).as_str() != Some(store_id) {
                let body = js_json::stringify(&JValue::obj([(
                    "appProperties",
                    JValue::obj([(STORE_PROP, JValue::Str(store_id.to_owned()))]),
                )]))
                .unwrap_or_default();
                self.call(
                    "PATCH",
                    &format!("{}/files/{}?fields=id", self.api, encode_uri_component(&id)),
                    Some("application/json"),
                    Some(body.into_bytes()),
                )
                .await?;
            }
            Folder {
                id,
                name: f.get("name").as_str().unwrap_or(FOLDER).to_owned(),
                created: false,
            }
        } else {
            let used: std::collections::HashSet<u64> = candidates.iter().map(|(n, _)| *n).collect();
            let n = (1..).find(|n| !used.contains(n)).unwrap_or(1);
            let name = if n == 1 {
                FOLDER.to_owned()
            } else {
                format!("{FOLDER} ({n})")
            };
            let body = js_json::stringify(&JValue::obj([
                ("name", JValue::Str(name.clone())),
                ("mimeType", JValue::Str(FOLDER_MIME.into())),
                (
                    "appProperties",
                    JValue::obj([(STORE_PROP, JValue::Str(store_id.to_owned()))]),
                ),
            ]))
            .unwrap_or_default();
            let r = self
                .call(
                    "POST",
                    &format!("{}/files?fields=id", self.api),
                    Some("application/json"),
                    Some(body.into_bytes()),
                )
                .await?;
            if !candidates.is_empty() {
                crate::offsite_engine::log(
                    "gdrive.new-folder",
                    vec![(
                        "reason",
                        JValue::Str("existing folders hold a different backup store".into()),
                    )],
                );
            }
            Folder {
                id: r.get("id").as_str().unwrap_or("").to_owned(),
                name,
                created: true,
            }
        };
        drive.save_patch(
            &Slot::Backup,
            vec![
                ("folderId", JValue::Str(folder.id.clone())),
                ("storeId", JValue::Str(store_id.to_owned())),
            ],
        );
        Ok(folder)
    }

    async fn remote_files(&self, parent: &str) -> Result<HashMap<String, Remote>, Fail> {
        let mut out = HashMap::new();
        let mut page = String::new();
        loop {
            let q = encode_uri_component(&format!("'{parent}' in parents and trashed=false"));
            let token = if page.is_empty() {
                String::new()
            } else {
                format!("&pageToken={}", encode_uri_component(&page))
            };
            let url = format!(
                "{}/files?q={q}&fields=nextPageToken,files(id,name,size)&pageSize=1000{token}",
                self.api
            );
            let r = self.call("GET", &url, None, None).await?;
            if let JValue::Arr(files) = r.get("files") {
                for f in files {
                    let size = f
                        .get("size")
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .or_else(|| f.get("size").as_num().map(|n| n as u64));
                    out.insert(
                        f.get("name").as_str().unwrap_or("").to_owned(),
                        Remote {
                            id: f.get("id").as_str().unwrap_or("").to_owned(),
                            size,
                        },
                    );
                }
            }
            page = r.get("nextPageToken").as_str().unwrap_or("").to_owned();
            if page.is_empty() {
                return Ok(out);
            }
        }
    }
}

/// Copies the encrypted store to Drive. Runs must be serialized by the caller.
pub async fn mirror(
    drive: &Arc<Drive>,
    store: &DirStore,
    max_delete: usize,
    pace: &crate::offsite_engine::Pace,
) -> Result<MirrorResult, Fail> {
    let m = Mirror {
        drive,
        api: drive.settings.api_base.clone(),
        upload: drive.settings.upload_base.clone(),
    };
    let keys = store.list("").await?;
    if !keys.iter().any(|k| k == "config") {
        return Err(store_empty());
    }
    let snapshots = keys.iter().filter(|k| k.starts_with("snapshots/")).count();
    if snapshots == 0 {
        return Err(
            Fail::new("Waiting for the first backup before copying.", 409)
                .with_id("waitingFirstBackup", vec![]),
        );
    }
    let local_config = store.get("config").await?.ok_or_else(store_empty)?;
    let store_id = hex_of(&Sha256::digest(&local_config));
    let folder = m.folder_id(&store_id, &local_config).await?;
    let parent = folder.id.clone();
    let remote = m.remote_files(&parent).await?;
    // A same-named `config` of another size means another store: never mix or prune it.
    if remote
        .get("config")
        .is_some_and(|c| c.size != Some(local_config.len() as u64))
    {
        return Err(Fail::new(
            "The Drive folder holds a different backup store; nothing was copied or removed.",
            409,
        )
        .with_id("differentStore", vec![]));
    }
    let (mut uploaded, mut bytes) = (0usize, 0u64);
    for key in &keys {
        if let Some(have) = remote.get(key) {
            // Names are content hashes, so a size difference can only be corruption on one side.
            if have.size != store.size_of(key).await? {
                let name = key.split('/').next().unwrap_or_default().to_owned();
                return Err(Fail::new(
                    format!("A file on Drive differs from the local copy ({name}); nothing was replaced or pruned."),
                    502,
                )
                .with_id("fileDiffers", vec![("name", JValue::Str(name))]));
            }
            continue;
        }
        pace.checkpoint().await;
        let Some(body) = store.get(key).await? else {
            continue;
        };
        let boundary = format!("noevia{}", hex_of(&random::<8>()));
        let meta = js_json::stringify(&JValue::obj([
            ("name", JValue::Str(key.clone())),
            ("parents", JValue::Arr(vec![JValue::Str(parent.clone())])),
        ]))
        .unwrap_or_default();
        let mut multipart = format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        let len = body.len() as u64;
        multipart.extend_from_slice(&body);
        multipart.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
        // A 4 MiB chunk over a slow uplink takes minutes: the transfer budget, not the quick one.
        m.drive
            .upload(
                &Slot::Backup,
                "POST",
                &format!("{}/files?uploadType=multipart&fields=id", m.upload),
                Some(&format!("multipart/related; boundary={boundary}")),
                Some(multipart),
            )
            .await?;
        uploaded += 1;
        bytes += len;
    }
    let local: std::collections::HashSet<&String> = keys.iter().collect();
    let stale: Vec<&Remote> = remote
        .iter()
        .filter(|(name, _)| !local.contains(name))
        .map(|(_, r)| r)
        .collect();
    // Decide before deleting anything: a run either prunes everything stale or nothing.
    if stale.len() > max_delete {
        return Err(Fail::new(
            format!("Copied, but more than {max_delete} old files would be removed from Drive at once; nothing was removed, to be safe."),
            502,
        )
        .with_id("tooManyStale", vec![("max", JValue::Num(max_delete as f64))]));
    }
    if stale.len() as f64 > SHARE_FLOOR.max(remote.len() as f64 * MAX_DELETE_SHARE) {
        return Err(Fail::new(
            format!(
                "Copied, but {} of the {} files on Drive are not in the local store; nothing was removed, to be safe.",
                stale.len(),
                remote.len()
            ),
            502,
        )
        .with_id(
            "staleShare",
            vec![("stale", JValue::Num(stale.len() as f64)), ("total", JValue::Num(remote.len() as f64))],
        ));
    }
    for f in &stale {
        m.call(
            "DELETE",
            &format!("{}/files/{}", m.api, encode_uri_component(&f.id)),
            None,
            None,
        )
        .await?;
    }
    crate::offsite_engine::log(
        "gdrive.mirrored",
        vec![
            ("uploaded", JValue::Num(uploaded as f64)),
            ("removed", JValue::Num(stale.len() as f64)),
        ],
    );
    Ok(MirrorResult {
        uploaded,
        bytes,
        removed: stale.len(),
        snapshots,
        folder: folder.name,
        new_folder: folder.created,
    })
}

#[cfg(test)]
mod tests {
    use super::folder_number;

    #[test]
    fn folder_names_are_cores_pattern() {
        assert_eq!(folder_number("noevia-offsite"), Some(1));
        assert_eq!(folder_number("noevia-offsite (2)"), Some(2));
        assert_eq!(folder_number("noevia-offsite (12)"), Some(12));
        for bad in [
            "noevia-offsite ()",
            "noevia-offsite (x)",
            "noevia-offsite(2)",
            "my noevia-offsite",
            "noevia-offsite (2) ",
        ] {
            assert_eq!(folder_number(bad), None, "{bad}");
        }
    }
}

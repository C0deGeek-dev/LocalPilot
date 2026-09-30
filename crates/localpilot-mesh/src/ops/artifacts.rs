//! Immutable artifact references on participant mail.
use super::{refused, sid, Mesh, Obj};
use crate::MeshError;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::path::Path;

fn id_ok(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn linked(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            m.file_attributes() & 0x400 != 0
        }
        #[cfg(not(windows))]
        {
            m.file_type().is_symlink()
        }
    })
}

impl Mesh {
    /// Verify each referenced bundle in the currently named session.
    pub fn artifact_refs(&self, role: &str, ids: &[String]) -> Result<Value, MeshError> {
        let s = self.require(role, true)?;
        if ids.len() > 16 {
            return Err(refused("at most 16 artifacts on one message"));
        }
        let base = self.mb.session_dir(sid(&s)).join("artifacts");
        let mut refs = Vec::new();
        for id in ids {
            if !id_ok(id) {
                return Err(refused(format!("unknown artifact {id:?}")));
            }
            let d = base.join(id);
            let corrupt = || refused(format!("ARTIFACT_CORRUPT {id}"));
            if linked(&base)
                || linked(&d)
                || linked(&d.join("files"))
                || linked(&d.join("artifact.json"))
            {
                return Err(corrupt());
            }
            let raw = std::fs::read(d.join("artifact.json")).map_err(|_| corrupt())?;
            if format!("{:x}", Sha256::digest(&raw)) != *id {
                return Err(corrupt());
            }
            let m: Value = serde_json::from_slice(&raw).map_err(|_| corrupt())?;
            let typ = m["type"].as_str().ok_or_else(corrupt)?;
            if ![
                "review-pack",
                "plan",
                "design",
                "document",
                "dataset",
                "summary",
            ]
            .contains(&typ)
            {
                return Err(corrupt());
            }
            for key in ["files", "generated"] {
                for e in m[key].as_array().ok_or_else(corrupt)? {
                    if e["deleted"] == true {
                        continue;
                    }
                    let h = e["sha256"]
                        .as_str()
                        .filter(|h| id_ok(h))
                        .ok_or_else(corrupt)?;
                    let p = d.join("files").join(h);
                    if linked(&p) {
                        return Err(corrupt());
                    }
                    let b = std::fs::read(p).map_err(|_| corrupt())?;
                    if format!("{:x}", Sha256::digest(&b)) != h
                        || e["size"].as_u64() != Some(b.len() as u64)
                    {
                        return Err(corrupt());
                    }
                }
            }
            refs.push(json!({"id": id, "type": typ}));
        }
        Ok(Value::Array(refs))
    }

    pub(super) fn check_artifact_refs(&self, role: &str, extra: &Obj) -> Result<(), MeshError> {
        if let Some(v) = extra.get("artifacts") {
            let refs = v
                .as_array()
                .ok_or_else(|| refused("artifacts must be a list"))?;
            let ids = refs
                .iter()
                .map(|r| {
                    r["id"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| refused("artifact id required"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if self.artifact_refs(role, &ids)? != *v {
                return Err(refused("artifact type differs from the stored one"));
            }
        }
        Ok(())
    }
}

pub(super) fn lines(m: &Obj) -> String {
    m.get("artifacts")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter().fold(String::new(), |mut out, r| {
                let _ = write!(
                    out,
                    "\nARTIFACT {} type={}",
                    r["id"].as_str().unwrap_or("None"),
                    r["type"].as_str().unwrap_or("None")
                );
                out
            })
        })
        .unwrap_or_default()
}

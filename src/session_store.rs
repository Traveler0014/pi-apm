//! Session-scoped claim-token storage.
//!
//! Claim tokens are bearer secrets: the database keeps only a hash, and the
//! plaintext is returned once. A long-lived agent session needs to reuse its
//! token across CLI invocations (and a run-end side-run needs it too), so the
//! CLI persists it in `<data_dir>/sessions/<session>.json` with mode 0600.
//!
//! This does not weaken the claim rules: possession of the file is the same
//! local trust boundary as possession of the SQLite file, and every write still
//! verifies the token against the live claim.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToken {
    pub session_id: String,
    pub claim_id: String,
    pub item_id: String,
    pub identifier: String,
    pub token: String,
    pub expires_at: String,
    pub saved_at: String,
}

#[derive(Debug, Clone)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    pub fn new(data_dir: impl AsRef<Path>) -> TokenStore {
        TokenStore {
            dir: data_dir.as_ref().join("sessions"),
        }
    }

    fn file(&self, session: &str) -> PathBuf {
        self.dir.join(format!("{}.json", safe_name(session)))
    }

    pub fn load(&self, session: &str) -> Vec<SessionToken> {
        let path = self.file(session);
        let Ok(raw) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<SessionToken>>(&raw).unwrap_or_default()
    }

    /// Find a token for an item, accepted by id or identifier.
    pub fn find(&self, session: &str, item_id: &str, identifier: &str) -> Option<SessionToken> {
        self.load(session)
            .into_iter()
            .find(|t| t.item_id == item_id || t.identifier == identifier)
    }

    pub fn save(&self, token: &SessionToken) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .map_err(|e| Error::Internal(format!("cannot create {}: {e}", self.dir.display())))?;
        let path = self.file(&token.session_id);
        let mut tokens = self.load(&token.session_id);
        tokens.retain(|t| t.item_id != token.item_id);
        tokens.push(token.clone());
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&tokens)?)
            .map_err(|e| Error::Internal(format!("cannot write {}: {e}", tmp.display())))?;
        restrict(&tmp)?;
        fs::rename(&tmp, &path)
            .map_err(|e| Error::Internal(format!("cannot replace {}: {e}", path.display())))?;
        Ok(())
    }

    pub fn forget(&self, session: &str, item_id: &str) {
        let path = self.file(session);
        let mut tokens = self.load(session);
        let before = tokens.len();
        tokens.retain(|t| t.item_id != item_id);
        if tokens.len() != before
            && fs::write(&path, serde_json::to_vec_pretty(&tokens).unwrap()).is_ok()
        {
            let _ = restrict(&path);
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn safe_name(session: &str) -> String {
    let cleaned: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(session.as_bytes());
    let digest = hex::encode(hasher.finalize());
    let trimmed = if cleaned.len() > 64 {
        cleaned[..64].to_string()
    } else {
        cleaned
    };
    format!("{trimmed}-{}", &digest[..8])
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::Internal(format!("cannot chmod {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

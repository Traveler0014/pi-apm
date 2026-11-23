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
use std::io::Write;
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

    /// Persist one claim token, preserving the session's other entries.
    ///
    /// The read-modify-write runs under a per-file lock, so two processes that
    /// share a session id (a REST server and a CLI, two REST instances, a
    /// side-run and its parent) cannot lose each other's tokens. The file format
    /// and the API are unchanged.
    pub fn save(&self, token: &SessionToken) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .map_err(|e| Error::Internal(format!("cannot create {}: {e}", self.dir.display())))?;
        let path = self.file(&token.session_id);
        let _lock = self.lock(&path)?;
        let mut tokens = self.load(&token.session_id);
        tokens.retain(|t| t.item_id != token.item_id);
        tokens.push(token.clone());
        self.replace_tokens(&token.session_id, &tokens)
    }

    pub fn forget(&self, session: &str, item_id: &str) {
        let path = self.file(session);
        let Ok(_lock) = self.lock(&path) else {
            // Best effort, exactly like before: a lock we cannot take means some
            // other process is mid-write and will re-read the file anyway.
            return;
        };
        let mut tokens = self.load(session);
        let before = tokens.len();
        tokens.retain(|t| t.item_id != item_id);
        if tokens.len() != before {
            // Same atomic replace as `save`: a reader never sees a half-written
            // session file, and a crash mid-forget leaves the old file intact.
            let _ = self.replace_tokens(session, &tokens);
        }
    }

    /// Serialise the session's tokens to a private temp file, then swap it in.
    fn replace_tokens(&self, session: &str, tokens: &[SessionToken]) -> Result<()> {
        let path = self.file(session);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(tokens)?;
        if let Err(err) = write_private(&tmp, &bytes).and_then(|()| replace_file(&tmp, &path)) {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        Ok(())
    }

    /// Exclusive advisory lock for one session file.
    ///
    /// `create_new` is the atomic primitive: exactly one process wins, the rest
    /// spin for a few milliseconds. A lock older than [`LOCK_STALE`] is treated
    /// as a crashed writer and removed, so a `kill -9` cannot wedge the session
    /// forever. The guard removes the file on drop.
    fn lock(&self, path: &Path) -> Result<LockGuard> {
        let lock_path = path.with_extension("lock");
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(file) => {
                    drop(file);
                    return Ok(LockGuard { path: lock_path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&lock_path) {
                        let _ = fs::remove_file(&lock_path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::Internal(format!(
                            "timed out waiting for {} (another process is writing the same session file); retry",
                            lock_path.display()
                        )));
                    }
                    std::thread::sleep(LOCK_SPIN);
                }
                Err(err) => {
                    return Err(Error::Internal(format!(
                        "cannot create {}: {err}",
                        lock_path.display()
                    )));
                }
            }
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// How long to wait for another process's write before failing. Longer than
/// [`LOCK_STALE`] so a crashed writer's lock is always reclaimed inside one call
/// rather than surfacing as a spurious timeout.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Back-off between attempts while another writer holds the file.
const LOCK_SPIN: std::time::Duration = std::time::Duration::from_millis(5);
/// A lock file this old belongs to a crashed writer: a token-file write takes
/// microseconds, so anything older is debris from a `kill -9`.
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(5);

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    match fs::metadata(path).and_then(|meta| meta.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|age| age > LOCK_STALE)
            .unwrap_or(false),
        // Cannot stat it (yet, or it just vanished): let the loop retry.
        Err(_) => false,
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

/// Write `bytes` to `path` so the file is never readable by another user, not
/// even briefly: on Unix the create itself carries mode 0600, so there is no
/// window between `create` (default umask) and a later `chmod`.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| Error::Internal(format!("cannot write {}: {e}", path.display())))?;
    // OpenOptions' mode only applies at creation: a temp file left behind by a
    // crashed writer may already exist with wider permissions, so narrow it
    // *before* writing any token into it.
    restrict(path)?;
    file.write_all(bytes)
        .map_err(|e| Error::Internal(format!("cannot write {}: {e}", path.display())))?;
    // Flush to disk before the rename, so a crash cannot publish an empty file.
    file.sync_all()
        .map_err(|e| Error::Internal(format!("cannot sync {}: {e}", path.display())))?;
    Ok(())
}

/// Swap `src` in for `dest`.
///
/// Unix `rename(2)` replaces an existing destination atomically. Windows
/// `fs::rename` fails when the destination exists, so remove it first and retry:
/// the file is a session-local cache of bearer tokens (the claims themselves are
/// authoritative in SQLite), so the tiny non-atomic window is acceptable there
/// and never exposes another user's data.
fn replace_file(src: &Path, dest: &Path) -> Result<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(_) if dest.exists() => {
            fs::remove_file(dest)
                .map_err(|e| Error::Internal(format!("cannot replace {}: {e}", dest.display())))?;
            fs::rename(src, dest)
                .map_err(|e| Error::Internal(format!("cannot replace {}: {e}", dest.display())))
        }
        Err(err) => Err(Error::Internal(format!(
            "cannot replace {}: {err}",
            dest.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(session: &str, item: &str, token: &str) -> SessionToken {
        SessionToken {
            session_id: session.to_string(),
            claim_id: format!("claim-{item}"),
            item_id: item.to_string(),
            identifier: item.to_uppercase(),
            token: token.to_string(),
            expires_at: "2026-01-01T00:00:00.000Z".to_string(),
            saved_at: "2026-01-01T00:00:00.000Z".to_string(),
        }
    }

    #[test]
    fn concurrent_saves_never_lose_a_token() {
        // The regression: read-modify-write without a lock let two writers race
        // and keep only one token, stranding the other claim.
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let store = store.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .save(&token(
                            "shared",
                            &format!("item-{index}"),
                            &format!("apm_{index}"),
                        ))
                        .expect("save");
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let saved = store.load("shared");
        assert_eq!(saved.len(), 8, "every concurrent save must survive");
        for index in 0..8 {
            assert!(
                saved.iter().any(|t| t.item_id == format!("item-{index}")),
                "missing item-{index}"
            );
        }
        // No lock file left behind.
        let leftovers: Vec<String> = std::fs::read_dir(store.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".lock"))
            .collect();
        assert!(leftovers.is_empty(), "stale locks: {leftovers:?}");
    }

    #[test]
    fn a_crashed_writers_lock_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        let path = store.dir().join(format!("{}.json", safe_name("shared")));
        let lock = path.with_extension("lock");
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(&lock, b"").unwrap();
        // Backdate it so it looks like debris from a killed process.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30);
        let file = std::fs::OpenOptions::new().write(true).open(&lock).unwrap();
        file.set_modified(old).unwrap();

        store
            .save(&token("shared", "item-1", "apm_x"))
            .expect("save");
        assert_eq!(store.load("shared").len(), 1);
        assert!(!lock.exists(), "the stale lock must be removed");
    }

    #[test]
    fn sessions_do_not_share_token_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        store.save(&token("a", "item-1", "apm_a")).unwrap();
        store.save(&token("b", "item-1", "apm_b")).unwrap();
        assert_eq!(store.find("a", "item-1", "ITEM-1").unwrap().token, "apm_a");
        assert_eq!(store.find("b", "item-1", "ITEM-1").unwrap().token, "apm_b");
        assert!(store.find("c", "item-1", "ITEM-1").is_none());
        store.forget("a", "item-1");
        assert!(store.find("a", "item-1", "ITEM-1").is_none());
        assert!(store.find("b", "item-1", "ITEM-1").is_some());
    }

    fn leftovers(store: &TokenStore) -> Vec<String> {
        std::fs::read_dir(store.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp") || name.ends_with(".lock"))
            .collect()
    }

    #[test]
    fn a_second_save_replaces_the_file_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        store.save(&token("shared", "item-1", "apm_first")).unwrap();
        store
            .save(&token("shared", "item-1", "apm_second"))
            .unwrap();
        let saved = store.load("shared");
        assert_eq!(saved.len(), 1, "the same item is replaced, not duplicated");
        assert_eq!(saved[0].token, "apm_second");
        assert!(leftovers(&store).is_empty(), "{:?}", leftovers(&store));
    }

    #[test]
    fn forgetting_a_token_is_atomic_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        store.save(&token("shared", "keep", "apm_keep")).unwrap();
        store.save(&token("shared", "drop", "apm_drop")).unwrap();
        store.forget("shared", "drop");
        let saved = store.load("shared");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].item_id, "keep");
        assert!(leftovers(&store).is_empty(), "{:?}", leftovers(&store));
    }

    #[cfg(unix)]
    #[test]
    fn saved_files_are_private_even_when_a_temp_file_already_exists() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        let path = store.dir().join(format!("{}.json", safe_name("shared")));
        let tmp = path.with_extension("json.tmp");
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(&tmp, b"stale").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();

        store.save(&token("shared", "item-1", "apm_x")).unwrap();

        assert!(!tmp.exists(), "the temp file must be renamed away");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the token file must never be group-readable"
        );
        assert_eq!(store.load("shared").len(), 1);
    }
}

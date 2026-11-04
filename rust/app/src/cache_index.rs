//! The EXE owns the index. Explorer's DLL continues to read PNGs directly.
use crate::{Config, common};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

struct Index {
    db: Connection,
    directories: HashSet<String>,
    count: u64,
    bytes: u64,
}
static READY: AtomicBool = AtomicBool::new(false);
static COUNT: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static INVENTORIES: AtomicU64 = AtomicU64::new(0);
pub fn inventory_passes() -> u64 {
    INVENTORIES.load(Ordering::Relaxed)
}
pub fn usage() -> Option<(u64, u64)> {
    READY
        .load(Ordering::Acquire)
        .then(|| (COUNT.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed)))
}
static INDEX: OnceLock<Result<Mutex<Index>, String>> = OnceLock::new();
fn index() -> Result<&'static Mutex<Index>> {
    INDEX
        .get_or_init(|| Index::open().map(Mutex::new).map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!(e.clone()))
}
impl Index {
    fn open() -> Result<Self> {
        fs::create_dir_all(common::cache::directory())?;
        let mut db = Connection::open(common::cache::directory().join("index.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(2))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            PRAGMA foreign_keys=ON; PRAGMA cache_size=-512; PRAGMA wal_autocheckpoint=256;
            CREATE TABLE IF NOT EXISTS sources(path_key TEXT PRIMARY KEY, path TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS images(cache_key TEXT PRIMARY KEY, source_key TEXT NOT NULL REFERENCES sources(path_key) ON DELETE CASCADE,
                source_size INTEGER NOT NULL, source_time TEXT NOT NULL, profile TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS image_source ON images(source_key);
            CREATE TABLE IF NOT EXISTS parents(parent_key TEXT NOT NULL, source_key TEXT NOT NULL REFERENCES sources(path_key) ON DELETE CASCADE,
                PRIMARY KEY(parent_key,source_key));
            CREATE TABLE IF NOT EXISTS cached_files(cache_key TEXT PRIMARY KEY, bytes INTEGER NOT NULL, modified INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS cache_age ON cached_files(modified);
            CREATE TABLE IF NOT EXISTS failures(cache_key TEXT PRIMARY KEY, error TEXT NOT NULL);")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS failure_sources(cache_key TEXT PRIMARY KEY, path TEXT NOT NULL);")?;
        let directories = {
            let mut stmt = db.prepare("SELECT DISTINCT parent_key FROM parents")?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<HashSet<String>>>()?
        };
        // One directory inventory at startup. All later accounting and pruning
        // use individual changed entries and the database's age index.
        let tx = db.transaction()?;
        tx.execute("DELETE FROM cached_files", [])?;
        INVENTORIES.fetch_add(1, Ordering::Relaxed);
        let (mut count, mut bytes) = (0, 0);
        for entry in fs::read_dir(common::cache::directory())?.flatten() {
            let path = entry.path();
            let Some(key) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().is_none_or(|e| e != "png") || !valid_key(key) {
                continue;
            }
            let Ok(m) = entry.metadata() else { continue };
            let size = m.len()
                + fs::metadata(path.with_extension("txt"))
                    .map(|m| m.len())
                    .unwrap_or(0);
            tx.execute(
                "INSERT INTO cached_files VALUES(?1,?2,?3)",
                params![key, size as i64, m.last_write_time() as i64],
            )?;
            count += 1;
            bytes += size;
        }
        tx.commit()?;
        let result = Self {
            db,
            directories,
            count,
            bytes,
        };
        result.publish();
        Ok(result)
    }
    fn publish(&self) {
        COUNT.store(self.count, Ordering::Relaxed);
        BYTES.store(self.bytes, Ordering::Relaxed);
        READY.store(true, Ordering::Release);
        crate::ui::update();
    }
    fn record_image(&mut self, key: &str, path: &Path) -> Result<()> {
        let m = fs::metadata(path)?;
        let bytes = m.len()
            + fs::metadata(path.with_extension("txt"))
                .map(|m| m.len())
                .unwrap_or(0);
        let old: Option<i64> = self
            .db
            .query_row(
                "SELECT bytes FROM cached_files WHERE cache_key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        self.db.execute("INSERT INTO cached_files VALUES(?1,?2,?3) ON CONFLICT(cache_key) DO UPDATE SET bytes=excluded.bytes,modified=excluded.modified", params![key,bytes as i64,m.last_write_time() as i64])?;
        self.count += u64::from(old.is_none());
        self.bytes = self
            .bytes
            .saturating_sub(old.unwrap_or(0) as u64)
            .saturating_add(bytes);
        self.publish();
        Ok(())
    }
    fn remove_image(&mut self, key: &str) -> Result<bool> {
        anyhow::ensure!(valid_key(key), "Invalid key in cache index");
        for extension in ["png", "txt"] {
            let cached = common::cache::directory().join(format!("{key}.{extension}"));
            match fs::remove_file(cached) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let old: Option<i64> = self
            .db
            .query_row(
                "SELECT bytes FROM cached_files WHERE cache_key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        self.db
            .execute("DELETE FROM cached_files WHERE cache_key=?1", [key])?;
        if let Some(bytes) = old {
            self.count = self.count.saturating_sub(1);
            self.bytes = self.bytes.saturating_sub(bytes as u64);
            self.publish();
        }
        Ok(old.is_some())
    }
    fn remove_source(&mut self, key: &str, path: &Path) -> Result<()> {
        self.db
            .execute("DELETE FROM sources WHERE path_key=?1", [key])?;
        for parent in path.ancestors().skip(1).map(common::path_key) {
            let retained: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM parents WHERE parent_key=?1)",
                [&parent],
                |r| r.get(0),
            )?;
            if !retained {
                self.directories.remove(&parent);
            }
        }
        Ok(())
    }
}
fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|c| c.is_ascii_hexdigit())
}
pub fn file_link(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let raw = raw
        .strip_prefix(r"\\?\UNC\")
        .map(|s| format!(r"\\{s}"))
        .unwrap_or_else(|| raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_owned())
        .replace('\\', "/");
    let mut out = if raw.starts_with("//") {
        "file:".to_owned()
    } else {
        "file:///".to_owned()
    };
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
pub fn remember(path: &Path, image: &Path, cfg: &Config) -> Result<()> {
    let key = image
        .file_stem()
        .and_then(|s| s.to_str())
        .context("Cache image has no name")?;
    anyhow::ensure!(valid_key(key), "Invalid cache key");
    anyhow::ensure!(
        common::cache::identity(path, cfg)? == key,
        "Source changed while linking its cache"
    );
    let metadata = fs::metadata(path)?;
    let source = common::path_key(path);
    if image.with_extension("txt").is_file() {
        let linked: bool = index()?.lock().unwrap().db.query_row(
            "SELECT EXISTS(SELECT 1 FROM images JOIN cached_files USING(cache_key) WHERE cache_key=?1)", [key], |r| r.get(0))?;
        if linked {
            crate::monitor::track_source(path.to_owned());
            return Ok(());
        }
    }
    let parents = path
        .ancestors()
        .skip(1)
        .map(common::path_key)
        .collect::<Vec<_>>();
    {
        let mut index = index()?.lock().unwrap();
        let tx = index.db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO sources VALUES(?1,?2)",
            params![source, path.to_string_lossy()],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO images VALUES(?1,?2,?3,?4,?5)",
            params![
                key,
                source,
                metadata.file_size() as i64,
                metadata.last_write_time().to_string(),
                cfg.profile()
            ],
        )?;
        for parent in &parents {
            tx.execute(
                "INSERT OR IGNORE INTO parents VALUES(?1,?2)",
                params![parent, source],
            )?;
        }
        tx.commit()?;
        index.directories.extend(parents);
    }
    let companion = image.with_extension("txt");
    if !companion.is_file() {
        let text = format!(
            "StepThumbs thumbnail\r\nSource: {}\r\nLink: {}\r\nRenderer: {}\r\nSize: 512 x 512\r\n",
            path.display(),
            file_link(path),
            cfg.renderer
        );
        common::cache::atomic_write(&companion, text.as_bytes())?;
    }
    index()?.lock().unwrap().record_image(key, image)?;
    if confirmed_missing(path) {
        remove_deleted(path)?;
        anyhow::bail!("Source was deleted while linking its cache");
    }
    crate::monitor::track_source(path.to_owned());
    Ok(())
}
fn confirmed_missing(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => return !metadata.is_file(),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(_) => return false,
    }
    for parent in path.ancestors().skip(1) {
        match fs::metadata(parent) {
            Ok(_) => return true,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    false
}
pub fn tracks_directory(path: &Path) -> bool {
    INDEX.get().and_then(|v| v.as_ref().ok()).is_some_and(|v| {
        v.lock()
            .unwrap()
            .directories
            .contains(&common::path_key(path))
    })
}
pub fn sources() -> Result<Vec<PathBuf>> {
    let index = index()?.lock().unwrap();
    let mut stmt = index.db.prepare("SELECT path FROM sources")?;
    Ok(stmt
        .query_map([], |r| r.get::<_, String>(0).map(PathBuf::from))?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn remove_deleted(path: &Path) -> Result<usize> {
    let mut index = index()?.lock().unwrap();
    let source = common::path_key(path);
    let affected = {
        let mut stmt=index.db.prepare("SELECT path_key,path FROM sources WHERE path_key=?1 OR path_key IN (SELECT source_key FROM parents WHERE parent_key=?1)")?;
        stmt.query_map([&source], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PathBuf::from(r.get::<_, String>(1)?),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut removed = 0;
    for (key, path) in affected {
        if !confirmed_missing(&path) {
            continue;
        }
        let images = {
            let mut stmt = index
                .db
                .prepare("SELECT cache_key FROM images WHERE source_key=?1")?;
            stmt.query_map([&key], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for image in images {
            removed += usize::from(index.remove_image(&image)?);
        }
        index.remove_source(&key, &path)?;
    }
    Ok(removed)
}
pub fn reconcile() {
    match sources() {
        Ok(paths) => {
            for source in paths {
                if confirmed_missing(&source) {
                    if let Err(e) = remove_deleted(&source) {
                        log(&e);
                    }
                }
            }
        }
        Err(e) => log(&e),
    }
}
pub fn prune(limit: u64) {
    if limit == 0 || usage().is_some_and(|(_, bytes)| bytes <= limit.saturating_mul(1024 * 1024)) {
        return;
    }
    let result = (|| -> Result<()> {
        let mut index = index()?.lock().unwrap();
        while index.bytes > limit.saturating_mul(1024 * 1024) {
            let key: Option<String> = index
                .db
                .query_row(
                    "SELECT cache_key FROM cached_files ORDER BY modified LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(key) = key else { break };
            let source: Option<(String,PathBuf)> = index.db.query_row("SELECT sources.path_key,sources.path FROM images JOIN sources ON images.source_key=sources.path_key WHERE images.cache_key=?1", [&key], |r| Ok((r.get(0)?, PathBuf::from(r.get::<_,String>(1)?)))).optional()?;
            index.remove_image(&key)?;
            index
                .db
                .execute("DELETE FROM images WHERE cache_key=?1", [&key])?;
            if let Some((source, path)) = source {
                let retained: bool = index.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM images WHERE source_key=?1)",
                    [&source],
                    |r| r.get(0),
                )?;
                if !retained {
                    index.remove_source(&source, &path)?;
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        log(&e);
    }
}
pub fn failed(key: &str) -> Result<Option<String>> {
    let key = format!("{}:{key}", env!("CARGO_PKG_VERSION"));
    Ok(index()?
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT error FROM failures WHERE cache_key=?1",
            [&key],
            |r| r.get(0),
        )
        .optional()?)
}
pub fn mark_failed(key: &str, path: &Path, error: &str) -> Result<()> {
    let key = format!("{}:{key}", env!("CARGO_PKG_VERSION"));
    let mut index = index()?.lock().unwrap();
    let tx = index.db.transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO failures VALUES(?1,?2)",
        params![&key, error],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO failure_sources VALUES(?1,?2)",
        params![&key, path.to_string_lossy()],
    )?;
    tx.commit()?;
    Ok(())
}
pub fn retry_failures() -> Result<Vec<PathBuf>> {
    let mut index = index()?.lock().unwrap();
    let paths = {
        let mut stmt = index
            .db
            .prepare("SELECT DISTINCT path FROM failure_sources")?;
        stmt.query_map([], |r| Ok(PathBuf::from(r.get::<_, String>(0)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let tx = index.db.transaction()?;
    tx.execute("DELETE FROM failures", [])?;
    tx.execute("DELETE FROM failure_sources", [])?;
    tx.commit()?;
    Ok(paths)
}
pub fn clear_images() -> Result<()> {
    let mut index = index()?.lock().unwrap();
    // Explicit maintenance uses the index, never a recursive folder traversal.
    let keys = {
        let mut stmt = index.db.prepare("SELECT cache_key FROM cached_files")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for key in keys {
        index.remove_image(&key)?;
    }
    index.db.execute("DELETE FROM sources", [])?;
    index.directories.clear();
    Ok(())
}
pub fn log(error: &anyhow::Error) {
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(common::data_dir().join("activity.log"))
    {
        let _ = writeln!(f, "Cache index: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn links_escape_spaces_and_keep_drive_and_unc_roots() {
        assert_eq!(
            file_link(Path::new(r"D:\parts\my part.step")),
            "file:///D:/parts/my%20part.step"
        );
        assert_eq!(
            file_link(Path::new(r"\\server\share\my part.step")),
            "file://server/share/my%20part.step"
        );
    }
    #[test]
    fn disconnected_volume_is_not_a_deletion() {
        let drives = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        if let Some(letter) = (b'D'..=b'Z')
            .rev()
            .find(|l| drives & (1u32 << (*l - b'A')) == 0)
        {
            assert!(!confirmed_missing(Path::new(&format!(
                "{}:\\missing\\model.step",
                letter as char
            ))));
        }
    }
}

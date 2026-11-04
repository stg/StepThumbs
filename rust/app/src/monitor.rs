use crate::{
    Broker, Config, common,
    journal::{Cursor, Journal},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ptr::{null, null_mut};
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Checkpoint<P = String> {
    version: u32,
    signature: String,
    cursors: HashMap<String, Cursor>,
    files: Vec<P>,
    directories: Vec<P>,
    renderer_profile: String,
    known_files: Vec<P>,
    max_file_mb: Option<u64>,
    failed_files: Vec<P>,
}
enum Event {
    File(PathBuf, bool),
    Directory(PathBuf),
    Deleted(PathBuf),
    Scanned(PathBuf),
    Finished(PathBuf, bool, bool),
    Journal(String),
    Overflow(String),
    Wake,
    Track(PathBuf),
    Rescan,
    RetryFailed,
    Shutdown(mpsc::SyncSender<()>),
}
fn signature(cfg: &Config, extensions: bool) -> String {
    let mut roots = cfg.roots.clone();
    let mut exclude = cfg.exclude.clone();
    exclude.push(common::data_dir().to_string_lossy().into());
    roots.sort_by_key(|x| x.to_uppercase());
    exclude.sort_by_key(|x| x.to_uppercase());
    roots.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    exclude.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    let mut values = roots;
    values.extend(exclude);
    if extensions {
        let mut e = cfg.extensions.clone();
        e.sort_by_key(|x| x.to_uppercase());
        values.extend(e);
    }
    format!(
        "{:X}",
        Sha256::digest(values.join("|").to_uppercase().as_bytes())
    )
}
fn background() {
    unsafe {
        windows_sys::Win32::System::Threading::SetThreadPriority(
            windows_sys::Win32::System::Threading::GetCurrentThread(),
            0x10000,
        );
    }
}
fn legacy_inventory(cfg: &Config) -> HashSet<PathBuf> {
    use std::io::BufRead;
    let mut found = HashSet::new();
    if let Ok(file) = fs::File::open(common::data_dir().join("worker.log")) {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Some((_, path)) = line.split_once(" Generated: ") {
                let p = PathBuf::from(path.trim());
                if eligible(cfg, &p) {
                    found.insert(p);
                }
            }
        }
    }
    found
}
fn eligible(cfg: &Config, path: &Path) -> bool {
    cfg.included(path)
        && cfg.supports(path)
        && fs::symlink_metadata(path)
            .is_ok_and(|m| m.is_file() && m.file_attributes() & (0x400 | 0x1000) == 0)
}
fn scan(root: PathBuf, cfg: &Config, tx: &mpsc::SyncSender<Event>, broker: &Broker) {
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let mut paused = broker.state.paused.lock().unwrap();
        while *paused {
            paused = broker.state.resumed.wait(paused).unwrap()
        }
        drop(paused);
        let Ok(meta) = fs::symlink_metadata(&dir) else {
            continue;
        };
        if meta.file_attributes() & (0x400 | 0x1000) != 0 {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if !cfg.included(&path) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_attributes() & (0x400 | 0x1000) != 0 {
                continue;
            }
            if meta.is_dir() {
                stack.push(path)
            } else if meta.is_file() && cfg.supports(&path) {
                let _ = tx.send(Event::File(path, false));
            }
        }
    }
    let _ = tx.send(Event::Scanned(root));
}
fn watch(root: PathBuf, tx: mpsc::SyncSender<Event>, cfg: Config, recursive: bool) {
    thread::spawn(move || {
        background();
        unsafe {
            let handle = CreateFileW(
                common::wide(&root).as_ptr(),
                1,
                7,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return;
            }
            let mut bytes = vec![0u8; 65536];
            loop {
                let mut returned = 0;
                let ok = ReadDirectoryChangesW(
                    handle,
                    bytes.as_mut_ptr() as _,
                    bytes.len() as u32,
                    i32::from(recursive),
                    0x01 | 0x02 | 0x08 | 0x10,
                    &mut returned,
                    null_mut(),
                    None,
                );
                let error = if ok == 0 { GetLastError() } else { 0 };
                let volume = volume(&root);
                if ok == 0 || returned == 0 {
                    let _ = tx.send(Event::Overflow(volume));
                    if ok == 0 && error != ERROR_NOTIFY_ENUM_DIR {
                        if let Err(e) = crate::cache_index::remove_deleted(&root) {
                            crate::cache_index::log(&e);
                        }
                        use std::io::Write;
                        if let Ok(mut f) = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(common::data_dir().join("activity.log"))
                        {
                            let _ = writeln!(
                                f,
                                "Folder watcher stopped for {}: Windows error {}",
                                root.display(),
                                error
                            );
                        }
                        break;
                    }
                    continue;
                }
                let mut offset = 0;
                let mut relevant = false;
                while offset + 12 <= returned as usize {
                    let next =
                        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                    let action =
                        u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
                    let len = u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap())
                        as usize;
                    if len % 2 != 0 || offset + 12 + len > returned as usize {
                        let _ = tx.send(Event::Overflow(volume.clone()));
                        break;
                    }
                    let words = bytes[offset + 12..offset + 12 + len]
                        .chunks_exact(2)
                        .map(|x| u16::from_le_bytes([x[0], x[1]]))
                        .collect::<Vec<_>>();
                    let path = root.join(String::from_utf16_lossy(&words));
                    if action == 2 || action == 4 {
                        if path.extension().is_some_and(|e| {
                            common::hooks::EXTENSIONS
                                .iter()
                                .any(|x| x[1..].eq_ignore_ascii_case(&e.to_string_lossy()))
                        }) || crate::cache_index::tracks_directory(&path)
                        {
                            let _ = tx.send(Event::Deleted(path));
                            relevant = true;
                        }
                    } else if cfg.included(&path) {
                        if action == 1 || action == 5 {
                            if path.is_dir() {
                                let _ = tx.send(Event::Directory(path));
                                relevant = true;
                            } else if cfg.supports(&path) {
                                let _ = tx.send(Event::File(path, true));
                                relevant = true;
                            }
                        } else if action == 3 && cfg.supports(&path) {
                            let _ = tx.send(Event::File(path, true));
                            relevant = true;
                        }
                    }
                    if next == 0 {
                        break;
                    }
                    offset += next;
                }
                if relevant {
                    let _ = tx.send(Event::Journal(volume));
                }
            }
            CloseHandle(handle);
        }
    });
}
fn volume(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        format!("{}\\", &s[..2])
    } else {
        String::new()
    }
}
fn save(
    cfg: &Config,
    cursors: &HashMap<String, Cursor>,
    files: &HashMap<PathBuf, (Instant, u32)>,
    dirs: &HashSet<PathBuf>,
    known: &HashSet<PathBuf>,
    failed: &HashSet<PathBuf>,
) -> anyhow::Result<()> {
    let c = Checkpoint::<std::borrow::Cow<'_, str>> {
        version: 2,
        signature: signature(cfg, true),
        cursors: cursors.clone(),
        files: files.keys().map(|p| p.to_string_lossy()).collect(),
        directories: dirs.iter().map(|p| p.to_string_lossy()).collect(),
        renderer_profile: cfg.profile(),
        known_files: known.iter().map(|p| p.to_string_lossy()).collect(),
        max_file_mb: Some(cfg.max_file_mb),
        failed_files: failed.iter().map(|p| p.to_string_lossy()).collect(),
    };
    common::cache::atomic_write(
        &common::data_dir().join("checkpoint-v2.json"),
        &serde_json::to_vec(&c)?,
    )
}
static NOTIFY: std::sync::OnceLock<mpsc::SyncSender<Event>> = std::sync::OnceLock::new();
pub fn shutdown() {
    if let Some(tx) = NOTIFY.get() {
        let (a, b) = mpsc::sync_channel(1);
        if tx.send(Event::Shutdown(a)).is_ok() {
            let _ = b.recv_timeout(Duration::from_secs(10));
        }
    }
}
pub fn wake() {
    if let Some(tx) = NOTIFY.get() {
        let _ = tx.send(Event::Wake);
    }
}
pub fn rescan() {
    if let Some(tx) = NOTIFY.get() {
        let _ = tx.try_send(Event::Rescan);
    }
}
pub fn retry_failed() {
    if let Some(tx) = NOTIFY.get() {
        let _ = tx.try_send(Event::RetryFailed);
    }
}
fn writer_open(path: &Path) -> bool {
    // A brief metadata open detects an existing writer without holding a handle
    // while importing, so CAD applications remain free to save the model.
    unsafe {
        let handle = CreateFileW(
            common::wide(path).as_ptr(),
            FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            matches!(
                GetLastError(),
                ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION
            )
        } else {
            CloseHandle(handle);
            false
        }
    }
}
pub fn track_source(path: PathBuf) {
    if let Some(tx) = NOTIFY.get() {
        let _ = tx.try_send(Event::Track(path));
    }
}
pub fn start(broker: Arc<Broker>) {
    thread::spawn(move || {
        background();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(broker)));
        let error = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("Preflight stopped: {e:#}")),
            Err(_) => Some("Preflight stopped after a panic".to_string()),
        };
        if let Some(error) = error {
            use std::io::Write;
            if let Ok(mut f) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(common::data_dir().join("activity.log"))
            {
                let _ = writeln!(f, "{error}");
            }
        }
    });
}
fn cleanup_only(cfg: &Config) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::sync_channel(4096);
    let _ = NOTIFY.set(tx.clone());
    let roots = cfg.roots_available();
    for root in &roots {
        watch(root.clone(), tx.clone(), cfg.clone(), true);
    }
    let mut watched = HashSet::new();
    let mut track = |source: PathBuf| {
        if roots.iter().any(|root| common::below(&source, root)) {
            return;
        }
        if let Some(parent) = source.parent() {
            if watched.insert(parent.to_owned()) {
                watch(parent.to_owned(), tx.clone(), cfg.clone(), false);
            }
        }
    };
    for path in crate::cache_index::sources().unwrap_or_default() {
        track(path);
    }
    while let Ok(event) = rx.recv() {
        match event {
            Event::Deleted(path) => {
                if let Err(e) = crate::cache_index::remove_deleted(&path) {
                    crate::cache_index::log(&e);
                }
            }
            Event::Track(path) => track(path),
            Event::Shutdown(ack) => {
                let _ = ack.send(());
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
fn run(broker: Arc<Broker>) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    crate::cache_index::reconcile();
    if cfg.extensions.is_empty() {
        return cleanup_only(&cfg);
    }
    let sig = signature(&cfg, true);
    let legacy = signature(&cfg, false);
    let saved = fs::read(common::data_dir().join("checkpoint-v2.json"))
        .ok()
        .and_then(|v| {
            serde_json::from_slice::<Checkpoint>(v.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&v))
                .ok()
        })
        .filter(|v| {
            v.version == 2
                && (v.signature == sig
                    || (v.signature == legacy && cfg.extensions == [".step", ".stp"]))
        });
    let mut cursors = HashMap::new();
    let mut files: HashMap<PathBuf, (Instant, u32)> = HashMap::new();
    let mut dirs = HashSet::new();
    let mut known: HashSet<PathBuf> = HashSet::new();
    let mut failed: HashSet<PathBuf> = HashSet::new();
    if let Some(s) = &saved {
        failed.extend(
            s.failed_files
                .iter()
                .map(PathBuf::from)
                .filter(|p| eligible(&cfg, p)),
        );
        for p in s.known_files.iter().chain(s.files.iter()) {
            let p = PathBuf::from(p);
            if eligible(&cfg, &p) {
                known.insert(p);
            }
        }
        if s.renderer_profile.is_empty() && s.known_files.is_empty() {
            known.extend(legacy_inventory(&cfg));
        }
        if s.renderer_profile != cfg.profile() || s.max_file_mb.unwrap_or(0) != cfg.max_file_mb {
            for p in &known {
                files.insert(p.clone(), (Instant::now(), 0));
            }
        }
        for p in &s.files {
            let p = PathBuf::from(p);
            if eligible(&cfg, &p) {
                files.insert(p, (Instant::now(), 0));
            }
        }
        for p in &s.directories {
            let p = PathBuf::from(p);
            if cfg.included(&p) {
                dirs.insert(p);
            }
        }
    }
    // Attach links to already cached images using metadata, without importing
    // a model or requesting a scan. The DLL's cache-hit path stays unchanged.
    for source in known.iter().cloned().chain(
        saved
            .iter()
            .flat_map(|s| s.known_files.iter().map(PathBuf::from))
            .filter(|p| !known.contains(p)),
    ) {
        if let Some(image) = common::cache::find(&source, &cfg) {
            if let Err(e) = crate::cache_index::remember(&source, &image, &cfg) {
                crate::cache_index::log(&e);
            }
        }
    }
    let (tx, rx) = mpsc::sync_channel(4096);
    let _ = NOTIFY.set(tx.clone());
    let pipeline = cfg.worker_count().saturating_add(1).max(2);
    let (render_tx, render_rx) = mpsc::sync_channel::<PathBuf>(pipeline);
    let render_rx = Arc::new(std::sync::Mutex::new(render_rx));
    let (scan_tx, scan_rx) = mpsc::sync_channel::<PathBuf>(1);
    for _ in 0..pipeline {
        let b = broker.clone();
        let t = tx.clone();
        let rx = render_rx.clone();
        thread::spawn(move || {
            background();
            loop {
                let p = match rx.lock().unwrap().recv() {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let result = if writer_open(&p) {
                    Err("Model is still open for writing".to_string())
                } else {
                    b.thumbnail_priority(p.clone(), true)
                };
                let ok = result.is_ok()
                    || result.as_ref().err().is_some_and(|e| {
                        e.starts_with("Skipped:") && !e.contains("Look import failed:")
                    });
                let permanent = result
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.contains("Look import failed:"));
                let _ = t.send(Event::Finished(p, ok, permanent));
            }
        });
    }
    {
        let b = broker.clone();
        let c = cfg.clone();
        let t = tx.clone();
        thread::spawn(move || {
            background();
            while let Ok(p) = scan_rx.recv() {
                scan(p, &c, &t, &b);
            }
        });
    }
    let watched = cfg.roots_available();
    for root in &watched {
        watch(root.clone(), tx.clone(), cfg.clone(), true);
    }
    let mut auxiliary = HashSet::new();
    for source in crate::cache_index::sources().unwrap_or_default() {
        if !watched.iter().any(|r| common::below(&source, r)) {
            if let Some(parent) = source.parent() {
                if auxiliary.insert(parent.to_owned()) {
                    watch(parent.to_owned(), tx.clone(), cfg.clone(), false);
                }
            }
        }
    }
    let volumes = cfg
        .roots_available()
        .iter()
        .map(|p| volume(p))
        .filter(|p| !p.is_empty())
        .collect::<HashSet<_>>();
    for root in cfg
        .roots_available()
        .into_iter()
        .filter(|r| volume(r).is_empty())
    {
        dirs.insert(root);
    }
    let mut journals = HashMap::new();
    for v in volumes {
        match Journal::open(&v) {
            Ok(j) => {
                let old = saved.as_ref().and_then(|s| {
                    s.cursors
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(&v))
                        .map(|(_, c)| *c)
                });
                if let Some(old) = old {
                    broker
                        .state
                        .journal_passes
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    match j.changes(old, &cfg.extensions) {
                        Ok((new, changes)) => {
                            for (p, dir) in changes {
                                if dir && cfg.included(&p) {
                                    dirs.insert(p);
                                } else if eligible(&cfg, &p) {
                                    known.insert(p.clone());
                                    files.insert(p, (Instant::now(), 0));
                                }
                            }
                            cursors.insert(v.clone(), new);
                        }
                        Err(_) => {
                            cursors.insert(v.clone(), j.query()?.0);
                            for root in cfg
                                .roots_available()
                                .into_iter()
                                .filter(|r| volume(r).eq_ignore_ascii_case(&v))
                            {
                                dirs.insert(root);
                            }
                        }
                    }
                } else {
                    cursors.insert(v.clone(), j.query()?.0);
                    for root in cfg
                        .roots_available()
                        .into_iter()
                        .filter(|r| volume(r).eq_ignore_ascii_case(&v))
                    {
                        dirs.insert(root);
                    }
                }
                journals.insert(v, j);
            }
            Err(_) => {
                for root in cfg
                    .roots_available()
                    .into_iter()
                    .filter(|r| volume(r).eq_ignore_ascii_case(&v))
                {
                    dirs.insert(root);
                }
            }
        }
    }
    // The restored checkpoint is only needed during startup. Keeping it here
    // otherwise duplicates every known source path for the lifetime of the app.
    drop(saved);
    let mut active_files: HashSet<PathBuf> = HashSet::new();
    let mut changed_active: HashSet<PathBuf> = HashSet::new();
    let mut active_dir: Option<PathBuf> = None;
    let mut changed_active_dir = false;
    let mut dirty = true;
    let mut save_at = Instant::now();
    let mut journals_due: HashMap<String, Instant> = HashMap::new();
    let mut last_ui = Instant::now();
    loop {
        broker
            .state
            .preflight_failed
            .store(failed.len() as u64, std::sync::atomic::Ordering::Relaxed);
        broker
            .state
            .preflight_pending
            .store(files.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let now = Instant::now();
        if dirty && now >= save_at {
            if save(&cfg, &cursors, &files, &dirs, &known, &failed).is_ok() {
                dirty = false
            } else {
                save_at = now + Duration::from_secs(5)
            }
        }
        let due = journals_due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(v, _)| v.clone())
            .collect::<Vec<_>>();
        for v in due {
            journals_due.remove(&v);
            if let (Some(j), Some(c)) = (journals.get(&v), cursors.get(&v).copied()) {
                broker
                    .state
                    .journal_passes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                match j.changes(c, &cfg.extensions) {
                    Ok((new, changes)) => {
                        cursors.insert(v.clone(), new);
                        for (p, dir) in changes {
                            if dir && cfg.included(&p) {
                                dirs.insert(p);
                            } else if eligible(&cfg, &p) {
                                known.insert(p.clone());
                                if active_files.contains(&p) {
                                    changed_active.insert(p.clone());
                                }
                                let due = now;
                                files
                                    .entry(p)
                                    .and_modify(|v| v.0 = v.0.max(due))
                                    .or_insert((due, 0));
                            }
                        }
                    }
                    Err(_) => {
                        if let Ok((c, _)) = j.query() {
                            cursors.insert(v.clone(), c);
                        }
                        for r in cfg
                            .roots_available()
                            .into_iter()
                            .filter(|r| volume(r).eq_ignore_ascii_case(&v))
                        {
                            dirs.insert(r);
                        }
                    }
                }
                dirty = true;
                save_at = now + Duration::from_secs(2);
            }
        }
        if active_dir.is_none() {
            if let Some(p) = dirs.iter().next().cloned() {
                broker
                    .state
                    .scans_started
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                scan_tx.send(p.clone())?;
                *broker.state.scanning.lock().unwrap() = Some(p.clone());
                crate::ui::update();
                active_dir = Some(p);
            }
        }
        while active_files.len() < pipeline && !*broker.state.paused.lock().unwrap() {
            if let Some(p) = files
                .iter()
                .find(|(p, v)| v.0 <= now && !active_files.contains(*p))
                .map(|(p, _)| p.clone())
            {
                render_tx.send(p.clone())?;
                active_files.insert(p);
            } else {
                break;
            }
        }
        let mut wake = None;
        let mut consider = |t: Instant| wake = Some(wake.map_or(t, |x: Instant| x.min(t)));
        if dirty {
            consider(save_at)
        }
        for t in journals_due.values() {
            consider(*t)
        }
        if active_files.len() < pipeline && !*broker.state.paused.lock().unwrap() {
            for (p, (v, _)) in &files {
                if !active_files.contains(p) {
                    consider(*v)
                }
            }
        }
        let event = match wake {
            Some(t) => match rx.recv_timeout(t.saturating_duration_since(Instant::now())) {
                Ok(v) => Some(v),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            },
            None => match rx.recv() {
                Ok(v) => Some(v),
                Err(_) => break,
            },
        };
        if let Some(event) = event {
            if last_ui.elapsed() >= Duration::from_millis(500) {
                crate::ui::update();
                last_ui = Instant::now();
            }
            let was_dirty = dirty;
            match event {
                Event::File(p, changed) => {
                    if eligible(&cfg, &p) {
                        known.insert(p.clone());
                        if changed && active_files.contains(&p) {
                            changed_active.insert(p.clone());
                        }
                        let due = Instant::now();
                        failed.remove(&p);
                        files
                            .entry(p)
                            .and_modify(|v| {
                                if changed {
                                    *v = (due, 0);
                                }
                            })
                            .or_insert((due, 0));
                        dirty = true;
                    }
                }
                Event::Deleted(p) => {
                    if let Err(e) = crate::cache_index::remove_deleted(&p) {
                        crate::cache_index::log(&e);
                    }
                    if cfg.included(&p) {
                        if !known.remove(&p) {
                            known.retain(|v| !common::below(v, &p));
                        }
                        files.retain(|v, _| !common::below(v, &p));
                        failed.retain(|v| !common::below(v, &p));
                        dirs.retain(|v| !common::below(v, &p));
                        dirty = true;
                    }
                }
                Event::Directory(p) => {
                    if cfg.included(&p) {
                        if active_dir.as_ref().is_some_and(|a| a == &p) {
                            changed_active_dir = true;
                        }
                        dirs.insert(p);
                        dirty = true;
                    }
                }
                Event::Scanned(p) => {
                    if changed_active_dir {
                        changed_active_dir = false;
                    } else {
                        dirs.remove(&p);
                    }
                    active_dir = None;
                    *broker.state.scanning.lock().unwrap() = None;
                    crate::ui::update();
                    dirty = true;
                }
                Event::Finished(p, ok, permanent) => {
                    if changed_active.remove(&p) {
                    } else if ok || !p.exists() {
                        files.remove(&p);
                        failed.remove(&p);
                    } else if let Some(v) = files.get_mut(&p) {
                        v.1 += 1;
                        if permanent || v.1 >= 6 {
                            files.remove(&p);
                            failed.insert(p.clone());
                        } else {
                            v.0 = Instant::now()
                                + Duration::from_secs([1, 2, 5, 15, 30][v.1 as usize - 1])
                        }
                    }
                    active_files.remove(&p);
                    dirty = true;
                }
                Event::Journal(v) => {
                    if journals.contains_key(&v) {
                        journals_due.insert(v, Instant::now() + Duration::from_secs(2));
                    }
                }
                Event::Overflow(v) => {
                    if journals.contains_key(&v) {
                        journals_due.insert(v, Instant::now());
                    } else {
                        for r in cfg
                            .roots_available()
                            .into_iter()
                            .filter(|r| volume(r).eq_ignore_ascii_case(&v))
                        {
                            dirs.insert(r);
                        }
                        dirty = true;
                    }
                }
                Event::Wake => {}
                Event::Rescan => {
                    dirs.extend(cfg.roots_available());
                    dirty = true;
                }
                Event::RetryFailed => {
                    match crate::cache_index::retry_failures() {
                        Ok(paths) => failed.extend(paths.into_iter().filter(|p| eligible(&cfg, p))),
                        Err(e) => crate::cache_index::log(&e),
                    }
                    for p in failed.drain() {
                        files.entry(p).or_insert((Instant::now(), 0));
                    }
                    dirty = true;
                }
                Event::Track(source) => {
                    if !watched.iter().any(|r| common::below(&source, r)) {
                        if let Some(parent) = source.parent() {
                            if auxiliary.insert(parent.to_owned()) {
                                watch(parent.to_owned(), tx.clone(), cfg.clone(), false);
                            }
                        }
                    }
                }
                Event::Shutdown(ack) => {
                    let _ = save(&cfg, &cursors, &files, &dirs, &known, &failed);
                    let _ = ack.send(());
                    break;
                }
            }
            if dirty && !was_dirty {
                save_at = Instant::now() + Duration::from_secs(2);
            }
        }
    }
    Ok(())
}

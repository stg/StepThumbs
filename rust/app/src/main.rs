#![allow(non_snake_case)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod cache_index;
mod hooks_test;
mod import_worker;
mod journal;
mod monitor;
mod ui;
use anyhow::{Context, Result, bail};
use look::{
    camera::prepare_camera,
    config::{
        CameraKind, LightingConfig, LightingPreset, MaterialMode, NamedView, RenderConfig,
        ViewConfig,
    },
    renderer::{Renderer, WgpuRenderer},
    scene::CompiledScene,
};
use std::{
    collections::HashMap,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
};
use stepthumbnail_common::{
    self as common, Config,
    ipc::{Request, Response},
};
type Outcome = Result<PathBuf, String>;
fn check_model_size(path: &Path, config: &Config) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Not a model file".into());
    }
    if config.max_file_mb > 0 && metadata.len() > config.max_file_mb.saturating_mul(1024 * 1024) {
        return Err(format!(
            "Skipped: {} (over {} MiB limit)",
            path.display(),
            config.max_file_mb
        ));
    }
    Ok(())
}
struct Job {
    path: PathBuf,
    key: String,
    config: Config,
    result: Mutex<Option<Outcome>>,
    ready: Condvar,
    started: std::sync::atomic::AtomicBool,
    background: std::sync::atomic::AtomicBool,
    child: Mutex<Option<usize>>,
}
struct State {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    gpu_active: std::sync::atomic::AtomicBool,
    success: std::sync::atomic::AtomicU64,
    failures: std::sync::atomic::AtomicU64,
    hits: std::sync::atomic::AtomicU64,
    paused: Mutex<bool>,
    resumed: Condvar,
    drained: Condvar,
    quiescing: std::sync::atomic::AtomicBool,
    source_bytes: std::sync::atomic::AtomicU64,
    promotions: std::sync::atomic::AtomicU64,
    preflight_pending: std::sync::atomic::AtomicU64,
    scans_started: std::sync::atomic::AtomicU64,
    journal_passes: std::sync::atomic::AtomicU64,
    current: Mutex<std::collections::HashSet<PathBuf>>,
    scanning: Mutex<Option<PathBuf>>,
    recent: Mutex<std::collections::VecDeque<Activity>>,
}
struct Activity {
    key: String,
    path: PathBuf,
    text: String,
    done: bool,
}
fn activity(state: &State, key: &str, path: &Path, text: String, done: bool) {
    let text = common::brief_error(&text);
    let mut recent = state.recent.lock().unwrap();
    if done {
        if let Some(entry) = recent.iter_mut().find(|entry| entry.key == key) {
            entry.text = text;
            entry.done = true;
        } else if !recent.iter().any(|entry| entry.path == path) {
            recent.push_front(Activity {
                key: key.into(),
                path: path.into(),
                text,
                done,
            });
        }
    } else {
        recent.retain(|entry| entry.path != path);
        recent.push_front(Activity {
            key: key.into(),
            path: path.into(),
            text,
            done,
        });
    }
    while recent.len() > 32 {
        let Some(index) = recent.iter().rposition(|entry| entry.done) else {
            break;
        };
        recent.remove(index);
    }
}
enum GpuJob {
    Ready(Arc<Job>, CompiledScene, look::import_control::ImportControl),
    Failed(Arc<Job>, String),
    Finished(Arc<Job>, Outcome),
}
struct Broker {
    state: Arc<State>,
    cpu: crossbeam_channel::Sender<Arc<Job>>,
    background: crossbeam_channel::Sender<Arc<Job>>,
    shared_stats: Option<common::shared_stats::SharedStats>,
    cache_limit_mb: u64,
}
fn finish(state: &State, job: &Arc<Job>, result: Outcome) {
    use std::sync::atomic::Ordering;
    if result.is_ok() {
        state.success.fetch_add(1, Ordering::Relaxed);
    } else if !result
        .as_ref()
        .err()
        .is_some_and(|e| e.starts_with("Skipped:"))
    {
        state.failures.fetch_add(1, Ordering::Relaxed);
    }
    activity(
        state,
        &job.key,
        &job.path,
        match &result {
            Ok(_) => format!("Ready: {}", job.path.display()),
            Err(error) if error.starts_with("Skipped:") => error.clone(),
            Err(error) => format!("Failed: {} ({error})", job.path.display()),
        },
        true,
    );
    if let Err(ref error) = result {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(common::data_dir().join("activity.log"))
        {
            let _ = writeln!(f, "{}: {}", job.path.display(), error);
        }
    }
    state.current.lock().unwrap().remove(&job.path);
    *job.result.lock().unwrap() = Some(result);
    job.ready.notify_all();
    {
        let mut jobs = state.jobs.lock().unwrap();
        jobs.remove(&job.key);
        if jobs.is_empty() {
            state.drained.notify_all();
        }
    }
    ui::update();
}
fn render(gpu: &mut WgpuRenderer, job: &Job, scene: &CompiledScene) -> Result<PathBuf> {
    let c = &job.config;
    let material = if c.source_colours {
        MaterialMode::Source
    } else {
        MaterialMode::Technical
    };
    let render = RenderConfig {
        resolution: [512, 512],
        background: if c.transparent {
            format!("{}00", c.background)
        } else {
            c.background.clone()
        },
        material_mode: material,
        antialias: c.antialias,
        ambient_occlusion: c.ambient_occlusion,
        ao_strength: c.ao_strength,
        ao_radius: c.ao_radius,
        edges: c.edges,
        edge_strength: c.edge_strength,
        edge_width: c.edge_width,
        ..Default::default()
    };
    let lighting = LightingConfig {
        preset: if c.lighting_preset == "f3d-match" {
            LightingPreset::F3dMatch
        } else {
            LightingPreset::Technical
        },
        ambient: c.ambient,
        direction: c.light_vector()?,
        intensity: c.light_intensity,
        color: c.light_colour.clone(),
    };
    let view = ViewConfig::named(NamedView::Iso, CameraKind::Orthographic);
    let camera = prepare_camera(&view, &scene.bounds, render.resolution);
    let batch = gpu.render_views(scene, &[camera], &render, &lighting)?;
    let image = batch
        .images
        .into_iter()
        .next()
        .context("No image produced")?;
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(
        image::RgbaImage::from_raw(image.width, image.height, image.rgba)
            .context("Invalid pixel data")?,
    )
    .write_to(&mut bytes, image::ImageFormat::Png)?;
    if common::cache::identity(&job.path, c)? != job.key {
        bail!("Source changed during rendering")
    };
    let path = common::cache::directory().join(format!("{}.png", job.key));
    common::cache::atomic_write(&path, bytes.get_ref())?;
    if common::cache::identity(&job.path, c).ok().as_deref() != Some(job.key.as_str()) {
        let _ = fs::remove_file(&path);
        bail!("Source changed during rendering")
    }
    cache_index::remember(&job.path, &path, c).unwrap_or_else(|e| {
        cache_index::log(&e);
    });
    cache_index::prune(c.max_cache_mb);
    Ok(path)
}
// Windows does not expose child-process I/O priority through the public
// background-mode API. This NT information class is best-effort; failure is
// logged and the documented CPU priority remains in effect.
type NtSetProcessInfo = unsafe extern "system" fn(
    windows_sys::Win32::Foundation::HANDLE,
    u32,
    *const std::ffi::c_void,
    u32,
) -> i32;
fn child_priority(handle: usize, low: bool) {
    static NT_SET: std::sync::OnceLock<Option<NtSetProcessInfo>> = std::sync::OnceLock::new();
    unsafe {
        let h = handle as windows_sys::Win32::Foundation::HANDLE;
        windows_sys::Win32::System::Threading::SetPriorityClass(h, if low { 0x40 } else { 0x20 });
        let function = NT_SET.get_or_init(|| {
            let module = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(
                common::wide("ntdll.dll").as_ptr(),
            );
            if module.is_null() {
                None
            } else {
                windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                    module,
                    b"NtSetInformationProcess\0".as_ptr(),
                )
                .map(|f| {
                    std::mem::transmute::<unsafe extern "system" fn() -> isize, NtSetProcessInfo>(f)
                })
            }
        });
        let priority: u32 = if low { 0 } else { 2 };
        let status = function.map(|f| {
            f(
                h,
                33,
                &priority as *const _ as _,
                std::mem::size_of::<u32>() as u32,
            )
        });
        if status.is_none_or(|code| code < 0) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(common::data_dir().join("activity.log"))
            {
                let _ = writeln!(f, "Could not adjust worker I/O priority: {status:?}");
            }
        }
    }
}

fn render_f3d(job: &Job) -> Result<PathBuf> {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let executable = Path::new(r"C:\Program Files\F3D\bin\f3d.exe");
    if !executable.is_file() {
        bail!("F3D is not installed")
    }
    fs::create_dir_all(common::cache::directory())?;
    let temp =
        common::cache::directory().join(format!("{}.f3d-{}.png", job.key, std::process::id()));
    let mut cmd = Command::new(executable);
    cmd.args([
        "--config=thumbnail",
        "--verbose=quiet",
        "--resolution",
        "512,512",
        "--output",
    ])
    .arg(&temp)
    .arg(&job.path)
    .creation_flags(
        0x08000000
            | if job.background.load(std::sync::atomic::Ordering::Acquire) {
                0x40
            } else {
                0
            },
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    if job.config.transparent {
        cmd.arg("--no-background");
    } else {
        let c = &job.config.background;
        let rgb = (0..3)
            .map(|i| u8::from_str_radix(&c[1 + i * 2..3 + i * 2], 16).unwrap_or(255) as f32 / 255.0)
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",");
        cmd.arg("--bg-color").arg(rgb);
    }
    if !job.config.source_colours {
        cmd.args(["--color", "0.8,0.8,0.8"]);
    }
    if job.config.antialias {
        cmd.arg("--anti-aliasing");
    }
    let mut child = cmd.spawn()?;
    use std::os::windows::io::AsRawHandle;
    struct ChildRegistration<'a>(&'a Job);
    impl Drop for ChildRegistration<'_> {
        fn drop(&mut self) {
            *self.0.child.lock().unwrap() = None;
        }
    }
    {
        let mut slot = job.child.lock().unwrap();
        *slot = Some(child.as_raw_handle() as usize);
        child_priority(
            child.as_raw_handle() as usize,
            job.background.load(std::sync::atomic::Ordering::Acquire),
        );
    }
    let _registration = ChildRegistration(job);
    let deadline = Instant::now() + Duration::from_secs(90);
    let result = loop {
        let status = match child.try_wait() {
            Ok(v) => v,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e.into());
            }
        };
        if let Some(status) = status {
            break if status.success() {
                Ok(())
            } else {
                Err(anyhow::anyhow!("F3D failed: {status}"))
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Err(anyhow::anyhow!("F3D timed out"));
        }
        thread::sleep(Duration::from_millis(100));
    };
    let outcome = (|| -> Result<PathBuf> {
        result?;
        let bytes = fs::read(&temp)?;
        image::load_from_memory(&bytes)?;
        if common::cache::identity(&job.path, &job.config)? != job.key {
            bail!("Source changed during rendering")
        };
        let dst = common::cache::directory().join(format!("{}.png", job.key));
        common::cache::atomic_write(&dst, &bytes)?;
        if common::cache::identity(&job.path, &job.config)
            .ok()
            .as_deref()
            != Some(job.key.as_str())
        {
            let _ = fs::remove_file(&dst);
            bail!("Source changed during rendering")
        }
        cache_index::remember(&job.path, &dst, &job.config).unwrap_or_else(|e| {
            cache_index::log(&e);
        });
        cache_index::prune(job.config.max_cache_mb);
        Ok(dst)
    })();
    let _ = fs::remove_file(temp);
    outcome
}
thread_local! { static IMPORT_BACKGROUND:std::cell::Cell<bool>=const{std::cell::Cell::new(false)}; }
fn import_background(enabled: bool) -> bool {
    IMPORT_BACKGROUND.with(|flag| {
        if flag.get() == enabled {
            return false;
        }
        let mode = if enabled { 0x00010000 } else { 0x00020000 };
        let ok = unsafe {
            windows_sys::Win32::System::Threading::SetThreadPriority(
                windows_sys::Win32::System::Threading::GetCurrentThread(),
                mode,
            )
        };
        if ok != 0 {
            flag.set(enabled);
            true
        } else {
            false
        }
    })
}

fn release_last(state: &State, renderer: &mut Option<WgpuRenderer>, job: &Job) {
    let last = {
        let jobs = state.jobs.lock().unwrap();
        jobs.len() == 1 && jobs.contains_key(&job.key)
    };
    if last {
        drop(renderer.take());
        state
            .gpu_active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
fn process_metrics() -> (u64, f64) {
    let (memory, cpu) =
        process_metrics_for(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() });
    let (workers_memory, workers_cpu) = import_worker::metrics();
    (memory + workers_memory, cpu + workers_cpu)
}
fn process_metrics_for(h: windows_sys::Win32::Foundation::HANDLE) -> (u64, f64) {
    unsafe {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::{ProcessStatus::*, Threading::*};
        let mut m: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let n = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        m.cb = n;
        let _ = GetProcessMemoryInfo(h, &mut m, n);
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) =
            std::mem::zeroed();
        let _ = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let ticks = |v: FILETIME| ((v.dwHighDateTime as u64) << 32) | v.dwLowDateTime as u64;
        (
            m.WorkingSetSize as u64,
            (ticks(k) + ticks(u)) as f64 / 10_000_000.0,
        )
    }
}
impl Broker {
    fn new() -> Arc<Self> {
        let config = Config::load().unwrap_or_default();
        let workers = config.worker_count();
        let state = Arc::new(State {
            jobs: Mutex::new(HashMap::new()),
            gpu_active: false.into(),
            success: 0.into(),
            failures: 0.into(),
            hits: 0.into(),
            paused: Mutex::new(false),
            resumed: Condvar::new(),
            drained: Condvar::new(),
            quiescing: false.into(),
            source_bytes: 0.into(),
            promotions: 0.into(),
            preflight_pending: 0.into(),
            scans_started: 0.into(),
            journal_passes: 0.into(),
            current: Mutex::new(std::collections::HashSet::new()),
            scanning: Mutex::new(None),
            recent: Mutex::new(std::collections::VecDeque::new()),
        });
        let (cpu_tx, cpu_rx) = crossbeam_channel::unbounded::<Arc<Job>>();

        let (bg_tx, bg_rx) = crossbeam_channel::unbounded::<Arc<Job>>();

        let (gpu_tx, gpu_rx) = mpsc::channel::<GpuJob>();
        let dispatchers = [(false, cpu_rx.clone()), (false, cpu_rx.clone())]
            .into_iter()
            .chain((0..workers).map(|_| (true, bg_rx.clone())));
        for (low, rx) in dispatchers {
            let rx = rx.clone();
            let tx = gpu_tx.clone();
            let state = state.clone();
            thread::spawn(move || {
                let mut importer: Option<import_worker::Importer> = None;
                if low {
                    unsafe {
                        windows_sys::Win32::System::Threading::SetThreadPriority(
                            windows_sys::Win32::System::Threading::GetCurrentThread(),
                            0x00010000,
                        );
                    }
                }
                loop {
                    let job = match if importer.is_some() {
                        rx.recv_timeout(std::time::Duration::from_secs(10))
                    } else {
                        rx.recv()
                            .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected)
                    } {
                        Ok(j) => j,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            importer = None;
                            continue;
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    };
                    if low {
                        let mut paused = state.paused.lock().unwrap();
                        while *paused {
                            importer = None;
                            paused = state.resumed.wait(paused).unwrap();
                        }
                    }
                    if job.started.swap(true, std::sync::atomic::Ordering::AcqRel) {
                        continue;
                    }
                    state.current.lock().unwrap().insert(job.path.clone());
                    activity(
                        &state,
                        &job.key,
                        &job.path,
                        format!("Rendering: {}", job.path.display()),
                        false,
                    );
                    ui::update();
                    // The file may have grown while waiting in the queue.
                    if let Err(error) = check_model_size(&job.path, &job.config) {
                        let result = Err(error);
                        if tx
                            .send(GpuJob::Finished(job.clone(), result.clone()))
                            .is_err()
                        {
                            finish(&state, &job, result);
                        }
                        continue;
                    }
                    if job.config.renderer == "f3d" {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            render_f3d(&job)
                        }))
                        .map_err(|_| "F3D job panicked".to_string())
                        .and_then(|r| r.map_err(|e| format!("{e:#}")));
                        if result.is_ok() {
                            if let Ok(m) = std::fs::metadata(&job.path) {
                                state
                                    .source_bytes
                                    .fetch_add(m.len(), std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        if tx
                            .send(GpuJob::Finished(job.clone(), result.clone()))
                            .is_err()
                        {
                            finish(&state, &job, result);
                        }
                        continue;
                    }
                    let control = look::import_control::ImportControl::with_timeout(
                        std::time::Duration::from_secs(120),
                    );
                    let parsed = (|| -> Result<CompiledScene> {
                        if importer.is_none() {
                            importer =
                                Some(import_worker::Importer::start(if low { 1 } else { 2 })?);
                        }
                        importer.as_mut().unwrap().import(&job, &state)
                    })();
                    if parsed.is_err() {
                        importer = None;
                    }
                    match parsed {
                        Ok(scene) => {
                            if let Ok(m) = std::fs::metadata(&job.path) {
                                state
                                    .source_bytes
                                    .fetch_add(m.len(), std::sync::atomic::Ordering::Relaxed);
                            }
                            if tx.send(GpuJob::Ready(job.clone(), scene, control)).is_err() {
                                finish(&state, &job, Err("GPU worker stopped".into()))
                            }
                        }
                        Err(error) => {
                            let error =
                                if state.quiescing.load(std::sync::atomic::Ordering::Acquire) {
                                    "Import cancelled for shutdown".to_owned()
                                } else {
                                    let details = common::brief_error(&error);
                                    let permanent = details.contains(import_worker::TIMED_OUT)
                                        || details.contains(": failed to parse STEP:");
                                    let message = format!(
                                        "Look import {}: {details}",
                                        if permanent { "failed" } else { "unavailable" }
                                    );
                                    // A sharing violation or worker startup failure must
                                    // remain retryable when the file becomes readable.
                                    if permanent {
                                        if let Err(e) = cache_index::mark_failed(&job.key, &message)
                                        {
                                            cache_index::log(&e);
                                        }
                                    }
                                    message
                                };
                            let _ = tx.send(GpuJob::Failed(job, error));
                        }
                    }
                }
            });
        }
        {
            let state = state.clone();

            thread::spawn(move || {
                use std::sync::atomic::Ordering;
                let mut renderer: Option<WgpuRenderer> = None;
                let mut pending = std::collections::VecDeque::new();
                loop {
                    if pending.is_empty() {
                        let next = if renderer.is_some() {
                            gpu_rx.recv_timeout(std::time::Duration::from_secs(2))
                        } else {
                            gpu_rx
                                .recv()
                                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                        };
                        let item = match next {
                            Ok(item) => item,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                drop(renderer.take());
                                state.gpu_active.store(false, Ordering::SeqCst);
                                ui::update();
                                continue;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        };
                        pending.push_back(item);
                    }
                    pending.extend(gpu_rx.try_iter());
                    // A larger preflight pool must not put Explorer behind
                    // every model that has already finished importing.
                    let next = pending
                        .iter()
                        .position(|item| {
                            let job = match item {
                                GpuJob::Ready(job, ..)
                                | GpuJob::Failed(job, ..)
                                | GpuJob::Finished(job, ..) => job,
                            };
                            !job.background.load(Ordering::Acquire)
                        })
                        .unwrap_or(0);
                    let item = pending.remove(next).unwrap();
                    match item {
                        GpuJob::Ready(job, scene, control) => {
                            if let Err(e) = control.check() {
                                release_last(&state, &mut renderer, &job);
                                finish(
                                    &state,
                                    &job,
                                    Err(format!("Import cancelled or timed out: {e:#}")),
                                );
                                continue;
                            }
                            let low = job.background.load(std::sync::atomic::Ordering::Acquire);
                            if low {
                                import_background(true);
                            }
                            if renderer.is_none() {
                                renderer = std::panic::catch_unwind(WgpuRenderer::new)
                                    .ok()
                                    .and_then(Result::ok);
                                state.gpu_active.store(renderer.is_some(), Ordering::SeqCst);
                            }
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    match renderer.as_mut() {
                                        Some(gpu) => {
                                            render(gpu, &job, &scene).map_err(|e| format!("{e:#}"))
                                        }
                                        None => Err("GPU initialization failed".into()),
                                    }
                                }))
                                .unwrap_or_else(|_| Err("GPU render panicked".into()));
                            if low {
                                import_background(false);
                            }
                            match result {
                                Ok(path) => {
                                    release_last(&state, &mut renderer, &job);
                                    finish(&state, &job, Ok(path));
                                }
                                Err(e) => {
                                    release_last(&state, &mut renderer, &job);
                                    finish(&state, &job, Err(e));
                                }
                            }
                        }
                        GpuJob::Failed(job, e) => {
                            release_last(&state, &mut renderer, &job);
                            finish(&state, &job, Err(e));
                        }
                        GpuJob::Finished(job, result) => {
                            release_last(&state, &mut renderer, &job);
                            finish(&state, &job, result)
                        }
                    }
                    let empty = state.jobs.lock().unwrap().is_empty();
                    if empty {
                        drop(renderer.take());
                        state.gpu_active.store(false, Ordering::SeqCst);
                        ui::update();
                    }
                }
            });
        }
        Arc::new(Self {
            state,
            cpu: cpu_tx,
            background: bg_tx,
            shared_stats: common::shared_stats::SharedStats::create(),
            cache_limit_mb: config.max_cache_mb,
        })
    }
    fn thumbnail(&self, path: PathBuf) -> Outcome {
        self.thumbnail_priority(path, false)
    }
    fn thumbnail_priority(&self, path: PathBuf, background: bool) -> Outcome {
        let c = Config::load().map_err(|e| format!("{e:#}"))?;
        if let Some(image) = common::cache::find(&path, &c) {
            if let Err(error) = cache_index::remember(&path, &image, &c) {
                cache_index::log(&error);
            }
            self.state
                .hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(image);
        }
        if let Err(message) = check_model_size(&path, &c) {
            let key = common::cache::identity(&path, &c).unwrap_or_default();
            activity(&self.state, &key, &path, message.clone(), false);
            activity(&self.state, &key, &path, message.clone(), true);
            ui::update();
            return Err(message);
        }
        let key = common::cache::identity(&path, &c).map_err(|e| format!("{e:#}"))?;
        if c.renderer == "look" {
            if let Some(error) = cache_index::failed(&key).map_err(|e| format!("{e:#}"))? {
                return Err(format!("Skipped: {} ({error})", path.display()));
            }
        }
        let job = {
            let mut jobs = self.state.jobs.lock().unwrap();
            if self
                .state
                .quiescing
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err("StepThumbs is applying settings".into());
            }
            if let Some(job) = jobs.get(&key) {
                if !background {
                    if job
                        .background
                        .swap(false, std::sync::atomic::Ordering::AcqRel)
                        && job.started.load(std::sync::atomic::Ordering::Acquire)
                    {
                        self.state
                            .promotions
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Some(handle) = *job.child.lock().unwrap() {
                        child_priority(handle, false);
                    }
                }
                if !background && !job.started.load(std::sync::atomic::Ordering::Acquire) {
                    self.cpu.send(job.clone()).map_err(|e| e.to_string())?;
                }
                job.clone()
            } else {
                let job = Arc::new(Job {
                    path,
                    key: key.clone(),
                    config: c,
                    result: Mutex::new(None),
                    ready: Condvar::new(),
                    started: false.into(),
                    background: background.into(),
                    child: Mutex::new(None),
                });
                jobs.insert(key, job.clone());
                if background {
                    self.background.send(job.clone())
                } else {
                    self.cpu.send(job.clone())
                }
                .map_err(|e| e.to_string())?;
                job
            }
        };
        let mut result = job.result.lock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while result.is_none() {
            if background {
                result = job.ready.wait(result).unwrap();
            } else {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err("Thumbnail request timed out while rendering continues".into());
                }
                let (guard, timeout) = job.ready.wait_timeout(result, remaining).unwrap();
                result = guard;
                if timeout.timed_out() && result.is_none() {
                    return Err("Thumbnail request timed out while rendering continues".into());
                }
            }
        }
        result.clone().unwrap()
    }
    fn drain(&self) {
        self.state
            .quiescing
            .store(true, std::sync::atomic::Ordering::Release);
        {
            let mut p = self.state.paused.lock().unwrap();
            *p = false;
            self.state.resumed.notify_all();
        }
        monitor::wake();
        monitor::shutdown();
        let mut jobs = self.state.jobs.lock().unwrap();
        while !jobs.is_empty() {
            jobs = self.state.drained.wait(jobs).unwrap();
        }
    }
    fn toggle_pause(&self) {
        let mut p = self.state.paused.lock().unwrap();
        *p = !*p;
        if !*p {
            self.state.resumed.notify_all();
        }
        monitor::wake();
        ui::update();
    }
    fn status(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering;
        let usage = cache_index::usage();
        let (count, bytes) = usage.unwrap_or_default();
        let (memory, cpu) = process_metrics();
        let current_files = self
            .state
            .current
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        let current = current_files.first();
        let scanning = self
            .state
            .scanning
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let limit = self.cache_limit_mb;
        serde_json::json!({"queued":self.state.jobs.lock().unwrap().len(),"gpu":if self.state.gpu_active.load(Ordering::SeqCst){"in use"}else{"released"},"success":self.state.success.load(Ordering::Relaxed),"failures":self.state.failures.load(Ordering::Relaxed),"cache_hits":self.state.hits.load(Ordering::Relaxed)+self.shared_stats.as_ref().map(|v|v.hits()).unwrap_or(0),"cache_inventory_passes":cache_index::inventory_passes(),"cache_ready":usage.is_some(),"cache_files":count,"cache_bytes":bytes,"paused":*self.state.paused.lock().unwrap(),"preflight_pending":self.state.preflight_pending.load(Ordering::Relaxed),"scans_started":self.state.scans_started.load(Ordering::Relaxed),"journal_passes":self.state.journal_passes.load(Ordering::Relaxed),"source_bytes":self.state.source_bytes.load(Ordering::Relaxed),"promotions":self.state.promotions.load(Ordering::Relaxed),"current":current,"current_files":current_files,"scanning":scanning,"memory_bytes":memory,"cpu_seconds":cpu,"cache_limit_mb":limit,"recent":self.state.recent.lock().unwrap().iter().map(|entry|entry.text.clone()).collect::<Vec<_>>()})
    }
}
fn serve(broker: Arc<Broker>) -> Result<()> {
    loop {
        let mut pipe = common::ipc::accept()?;
        let b = broker.clone();
        thread::spawn(move || {
            let mut quitting = false;
            let response = match common::ipc::read(&mut pipe) {
                Ok(Request::Thumbnail { path }) => match b.thumbnail(path.into()) {
                    Ok(path) => Response::image(&path),
                    Err(e) => Response::error(e),
                },
                Ok(Request::Status) => Response::status(b.status()),
                Ok(Request::Pause { paused }) => {
                    let mut p = b.state.paused.lock().unwrap();
                    *p = paused;
                    if !paused {
                        b.state.resumed.notify_all();
                    }
                    drop(p);
                    monitor::wake();
                    ui::update();
                    Response::ok()
                }
                Ok(Request::Shutdown) => {
                    b.drain();
                    quitting = true;
                    Response::ok()
                }
                Ok(Request::Show) => {
                    ui::show();
                    Response::ok()
                }
                Err(e) => Response::error(e),
            };
            let _ = common::ipc::write(&mut pipe, &response);
            if quitting {
                ui::quit();
            }
        });
    }
}
fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("--import-worker") {
        return import_worker::run(args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1));
    }
    if args.get(1).map(String::as_str) == Some("--wait-for") {
        if let Some(pid) = args.get(2).and_then(|v| v.parse::<u32>().ok()) {
            unsafe {
                let h = windows_sys::Win32::System::Threading::OpenProcess(0x00100000, 0, pid);
                if !h.is_null() {
                    let result =
                        windows_sys::Win32::System::Threading::WaitForSingleObject(h, 120000);
                    windows_sys::Win32::Foundation::CloseHandle(h);
                    if result != 0 {
                        return Ok(());
                    }
                }
            }
        }
    }
    if matches!(
        args.get(1).map(String::as_str),
        Some("--status" | "--render-test" | "--promotion-test" | "--hooks-self-test")
    ) {
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(u32::MAX);
        }
    }
    if matches!(
        args.get(1).map(String::as_str),
        Some("--pause" | "--resume")
    ) {
        let response = common::ipc::call(
            &Request::Pause {
                paused: args[1] == "--pause",
            },
            None,
        )?;
        if let Some(e) = response.error {
            bail!(e)
        }
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("--shutdown") {
        if common::ipc::exists() {
            let _ = common::ipc::call(&Request::Shutdown, None)?;
        }
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("--hooks-self-test") {
        return hooks_test::run(&std::env::current_exe()?.with_file_name("StepThumbsShell.dll"));
    }
    if args.get(1).map(String::as_str) == Some("--restore-hooks") {
        let dll = std::env::current_exe()?.with_file_name("StepThumbsShell.dll");
        return common::hooks::install(&[], &dll);
    }
    if args.get(1).map(String::as_str) == Some("--status") {
        println!(
            "{}",
            serde_json::to_string_pretty(&common::ipc::call(&Request::Status, None)?.status)?
        );
        return Ok(());
    }
    if matches!(
        args.get(1).map(String::as_str),
        Some("--snapshot" | "--snapshot-filetypes")
    ) {
        let path = Path::new(args.get(2).context("Expected snapshot directory")?);
        return ui::snapshot(
            Broker::new(),
            path,
            args[1] == "--snapshot-filetypes",
            args.get(3).and_then(|v| v.parse().ok()).unwrap_or(96),
        );
    }
    if args.get(1).map(String::as_str) == Some("--promotion-test") {
        let path = Path::new(args.get(2).context("Expected model path")?).to_path_buf();
        let broker = Broker::new();
        let bg = broker.clone();
        let source = path.clone();
        let worker = thread::spawn(move || bg.thumbnail_priority(source, true));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !worker.is_finished()
            && !broker.state.current.lock().unwrap().contains(&path)
            && std::time::Instant::now() < deadline
        {
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let foreground = broker.thumbnail(path);
        let background = worker
            .join()
            .map_err(|_| anyhow::anyhow!("background worker panicked"))?;
        println!("{}", broker.status());
        foreground.map_err(anyhow::Error::msg)?;
        background.map_err(anyhow::Error::msg)?;
        if broker
            .state
            .promotions
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
        {
            bail!("Import completed before a promotion checkpoint was observed")
        };
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("--render-test") {
        let path = Path::new(args.get(2).context("Expected source path")?).to_path_buf();
        let b = Broker::new();
        let output = b.thumbnail(path).map_err(anyhow::Error::msg)?;
        if let Some(destination) = args.get(3) {
            fs::copy(&output, destination)?;
        }
        println!("{}\n{}", output.display(), b.status());
        return Ok(());
    }
    // A mutex prevents simultaneous Explorer misses from starting competing brokers.
    let name = format!(
        "Local\\StepThumbnail-{}",
        &blake3::hash(common::data_dir().to_string_lossy().as_bytes()).to_hex()[..20]
    );
    let handle = unsafe {
        windows_sys::Win32::System::Threading::CreateMutexW(
            std::ptr::null(),
            0,
            common::wide(name).as_ptr(),
        )
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { windows_sys::Win32::Foundation::GetLastError() }
        == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS
    {
        if args.get(1).map(String::as_str) != Some("--background") {
            let _ = common::ipc::call(&Request::Show, None);
        }
        return Ok(());
    }
    fs::create_dir_all(common::data_dir())?;
    Config::load()?;
    let broker = Broker::new();
    monitor::start(broker.clone());
    let serving = broker.clone();
    thread::spawn(move || {
        let _ = serve(serving);
    });
    ui::run(
        broker,
        args.get(1).map(String::as_str) != Some("--background"),
        if args.get(1).map(String::as_str) == Some("--file-types") {
            2
        } else if args.get(1).map(String::as_str) == Some("--wait-for") {
            args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0)
        } else {
            0
        },
    )
}

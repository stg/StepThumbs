//! Reuse a CPU-only importer while jobs arrive. A stuck kernel call can be
//! terminated without taking down the tray, the GPU thread, or other imports.
use crate::{Job, State, child_priority, common};
use anyhow::{Context, Result, bail, ensure};
use look::{
    config::{MaterialMode, UpAxis},
    scene::{CompiledScene, compile_scene_for_render_with_control},
    timing::Timings,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::*, System::JobObjects::*};
const MAX_FRAME: u64 = 512 * 1024 * 1024;
static PROCESSES: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static FINISHED_CPU: AtomicU64 = AtomicU64::new(0);
pub fn metrics() -> (u64, f64) {
    let processes = PROCESSES.lock().unwrap();
    let mut memory = 0;
    let mut cpu = FINISHED_CPU.load(Ordering::Relaxed) as f64 / 10_000_000.0;
    for handle in processes.iter() {
        let (m, c) = crate::process_metrics_for(*handle as HANDLE);
        memory += m;
        cpu += c;
    }
    (memory, cpu)
}
pub const TIMED_OUT: &str = "Look import timed out";
#[derive(Serialize, Deserialize)]
struct Request {
    path: std::path::PathBuf,
    colours: bool,
    seconds: u64,
}
struct Process {
    child: Child,
    job: HANDLE,
    registered: bool,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.registered {
            let mut processes = PROCESSES.lock().unwrap();
            let handle = self.child.as_raw_handle() as usize;
            let (_, cpu) = crate::process_metrics_for(handle as HANDLE);
            FINISHED_CPU.fetch_add((cpu * 10_000_000.0) as u64, Ordering::Relaxed);
            processes.retain(|h| *h != handle);
        }
        unsafe {
            CloseHandle(self.job);
        }
        crate::ui::update();
    }
}
pub struct Importer {
    process: Process,
    input: BufWriter<ChildStdin>,
    output: mpsc::Receiver<Result<CompiledScene, String>>,
}
impl Importer {
    pub fn start(threads: usize) -> Result<Self> {
        fs::create_dir_all(common::data_dir())?;
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(common::data_dir().join("import.log"))?;
        let child = Command::new(std::env::current_exe()?)
            .args(["--import-worker", &threads.to_string()])
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .spawn()?;
        let mut process = Process {
            child,
            job: std::ptr::null_mut(),
            registered: false,
        };
        // Closing the parent must never leave an importer using CPU behind it.
        unsafe {
            process.job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            ensure!(
                !process.job.is_null(),
                "Cannot create importer lifetime guard"
            );
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            ensure!(
                SetInformationJobObject(
                    process.job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as _,
                    std::mem::size_of_val(&info) as u32
                ) != 0,
                "Cannot protect importer lifetime"
            );
            ensure!(
                AssignProcessToJobObject(process.job, process.child.as_raw_handle() as _) != 0,
                "Cannot attach importer lifetime guard"
            );
        }
        let input = BufWriter::new(
            process
                .child
                .stdin
                .take()
                .context("Importer input missing")?,
        );
        let mut stdout = BufReader::new(
            process
                .child
                .stdout
                .take()
                .context("Importer output missing")?,
        );
        let (tx, output) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let result = (|| -> Result<Result<CompiledScene, String>> {
                    let mut size = [0; 8];
                    stdout.read_exact(&mut size)?;
                    let size = u64::from_le_bytes(size);
                    ensure!(size > 0 && size <= MAX_FRAME, "Invalid importer reply size");
                    let mut frame = (&mut stdout).take(size);
                    let reply = bincode::deserialize_from(&mut frame)?;
                    ensure!(frame.limit() == 0, "Incomplete importer reply");
                    Ok(reply)
                })();
                match result {
                    Ok(reply) => {
                        if tx.send(reply).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("Importer stopped: {e:#}")));
                        break;
                    }
                }
            }
        });
        PROCESSES
            .lock()
            .unwrap()
            .push(process.child.as_raw_handle() as usize);
        process.registered = true;
        crate::ui::update();
        Ok(Self {
            process,
            input,
            output,
        })
    }
    pub fn import(&mut self, job: &Arc<Job>, state: &State) -> Result<CompiledScene> {
        let handle = self.process.child.as_raw_handle() as usize;
        // Hold the same mutex used for foreground promotion until the initial
        // priority is set, so promotion cannot be accidentally overwritten.
        {
            let mut child = job.child.lock().unwrap();
            child_priority(handle, job.background.load(Ordering::Acquire));
            *child = Some(handle);
        }
        let result = (|| -> Result<CompiledScene> {
            let request = Request {
                path: job.path.clone(),
                colours: job.config.source_colours,
                seconds: job.config.import_timeout_seconds,
            };
            serde_json::to_writer(&mut self.input, &request)?;
            self.input.write_all(b"\n")?;
            self.input.flush()?;
            let deadline = Instant::now() + Duration::from_secs(request.seconds);
            let mut last_update = Instant::now();
            loop {
                if state.quiescing.load(Ordering::Acquire) {
                    bail!("Import cancelled for shutdown")
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    bail!("{TIMED_OUT} after {} seconds", request.seconds)
                }
                if last_update.elapsed() >= Duration::from_secs(2) {
                    crate::ui::update();
                    last_update = Instant::now();
                }
                match self
                    .output
                    .recv_timeout(remaining.min(Duration::from_millis(100)))
                {
                    Ok(reply) => return reply.map_err(anyhow::Error::msg),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        })();
        *job.child.lock().unwrap() = None;
        if result.is_err() {
            let _ = self.process.child.kill();
            let _ = self.process.child.wait();
        }
        result
    }
}
pub fn run(threads: usize) -> Result<()> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.clamp(1, 32))
        .build_global()?;
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut output = BufWriter::new(std::io::stdout().lock());
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        ensure!(line.len() <= 65536, "Importer request too large");
        let request: Request = serde_json::from_str(&line)?;
        let mut result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let control = look::import_control::ImportControl::with_timeout(Duration::from_secs(
                request.seconds,
            ));
            compile_scene_for_render_with_control(
                &request.path,
                UpAxis::Z,
                if request.colours {
                    MaterialMode::Source
                } else {
                    MaterialMode::Technical
                },
                &mut Timings::default(),
                &control,
            )
        }))
        .map_err(|_| "Model import panicked".to_owned())
        .and_then(|r| r.map_err(|e| common::brief_error(&e)));
        let mut size = bincode::serialized_size(&result)?;
        if size > MAX_FRAME {
            result = Err("Compiled model is too large for a thumbnail".into());
            size = bincode::serialized_size(&result)?;
        }
        output.write_all(&size.to_le_bytes())?;
        bincode::serialize_into(&mut output, &result)?;
        output.flush()?;
    }
}

//! A settings preview never saves settings or populates the thumbnail cache.
//! Only the latest request is rendered; the imported scene lives while its tab is open.
use crate::{Broker, Config, Job, common, import_worker};
use anyhow::{Context, Result};
use look::{renderer::WgpuRenderer, scene::CompiledScene};
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};

pub struct Reply {
    pub revision: u64,
    pub result: Result<image::RgbaImage, String>,
}
struct Request {
    revision: u64,
    path: PathBuf,
    config: Config,
}
pub struct Preview {
    tx: mpsc::Sender<Option<Request>>,
    revision: Arc<AtomicU64>,
    active: Arc<Mutex<Option<Arc<Job>>>>,
    reply: Arc<Mutex<Option<Reply>>>,
}
impl Preview {
    pub fn new(broker: Arc<Broker>) -> Self {
        let (tx, rx) = mpsc::channel::<Option<Request>>();
        let revision = Arc::new(AtomicU64::new(0));
        let active = Arc::new(Mutex::new(None::<Arc<Job>>));
        let reply = Arc::new(Mutex::new(None));
        let (rev, running, out) = (revision.clone(), active.clone(), reply.clone());
        std::thread::spawn(move || {
            let mut scene: Option<(String, CompiledScene)> = None;
            let mut default_ready = false;
            while let Ok(mut request) = rx.recv() {
                for next in rx.try_iter() {
                    request = next;
                }
                let Some(request) = request else {
                    scene = None;
                    continue;
                };
                if rev.load(Ordering::Acquire) != request.revision {
                    continue;
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> Result<image::RgbaImage> {
                        if request.path == default_path() && !default_ready {
                            default_file()?;
                            default_ready = true;
                        }
                        crate::check_model_size(&request.path, &request.config)
                            .map_err(anyhow::Error::msg)?;
                        let job = Arc::new(Job {
                            path: request.path.clone(),
                            key: format!("preview-{}", request.revision),
                            config: request.config.clone(),
                            result: Mutex::new(None),
                            ready: Condvar::new(),
                            started: true.into(),
                            background: false.into(),
                            child: Mutex::new(None),
                            cancelled: false.into(),
                        });
                        *running.lock().unwrap() = Some(job.clone());
                        if request.config.renderer == "f3d" {
                            scene = None;
                            let image = image::load_from_memory(&crate::render_f3d_bytes(&job)?)?
                                .to_rgba8();
                            anyhow::ensure!(
                                image.dimensions() == (512, 512),
                                "Unexpected preview dimensions"
                            );
                            return Ok(image);
                        }
                        // Lighting and effects do not require another CPU import.
                        let mut import_config = request.config.clone();
                        import_config.renderer = "look".into();
                        let meta = std::fs::metadata(&request.path)?;
                        let identity = format!(
                            "{}:{:?}:{:?}:{}",
                            request.path.display(),
                            meta.len(),
                            meta.modified()?,
                            import_config.source_colours
                        );
                        if scene.as_ref().is_none_or(|s| s.0 != identity) {
                            scene = None;
                            let mut importer = import_worker::Importer::start(2)?;
                            let imported = importer.import(&job, &broker.state)?;
                            scene = Some((identity, imported));
                        }
                        if rev.load(Ordering::Acquire) != request.revision {
                            anyhow::bail!("Superseded preview");
                        }
                        let mut gpu = WgpuRenderer::new()?;
                        crate::render_image(
                            &mut gpu,
                            &request.config,
                            &scene.as_ref().context("No preview scene")?.1,
                        )
                    },
                ))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Preview rendering failed")))
                .map_err(|e| common::brief_error(&e));
                *running.lock().unwrap() = None;
                if rev.load(Ordering::Acquire) == request.revision {
                    *out.lock().unwrap() = Some(Reply {
                        revision: request.revision,
                        result,
                    });
                    crate::ui::preview_ready();
                }
            }
        });
        Self {
            tx,
            revision,
            active,
            reply,
        }
    }
    pub fn request(&self, path: PathBuf, config: Config) -> u64 {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(job) = self.active.lock().unwrap().as_ref() {
            if job.path != path || job.config.renderer != config.renderer {
                job.cancelled.store(true, Ordering::Release);
            }
        }
        let _ = self.tx.send(Some(Request {
            revision,
            path,
            config,
        }));
        revision
    }
    pub fn release(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        if let Some(job) = self.active.lock().unwrap().as_ref() {
            job.cancelled.store(true, Ordering::Release);
        }
        let _ = self.tx.send(None);
        self.reply.lock().unwrap().take();
    }
    pub fn invalidate(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.reply.lock().unwrap().take();
    }
    pub fn take(&self) -> Option<Reply> {
        self.reply.lock().unwrap().take()
    }
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.release();
    }
}
pub fn default_file() -> Result<PathBuf> {
    let path = default_path();
    let bytes = include_bytes!("../assets/preview.step");
    if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::create_dir_all(path.parent().unwrap())?;
        common::cache::atomic_write(&path, bytes)?;
    }
    Ok(path)
}
pub fn default_path() -> PathBuf {
    common::data_dir().join("preview").join("preview.step")
}

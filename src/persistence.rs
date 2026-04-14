use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use tracing::warn;

#[derive(Clone)]
pub struct AsyncJsonWriter<T> {
    path: Arc<PathBuf>,
    mode: Arc<WriterMode<T>>,
}

enum WriterMode<T> {
    Sync,
    Async {
        tx: mpsc::Sender<T>,
    },
}

impl<T> AsyncJsonWriter<T>
where
    T: Serialize + Send + 'static,
{
    pub fn new(path: PathBuf, debounce: Duration) -> Self {
        let path = Arc::new(path);
        if debounce.is_zero() {
            return Self {
                path,
                mode: Arc::new(WriterMode::Sync),
            };
        }

        let (tx, rx) = mpsc::channel::<T>();
        let worker_path = path.clone();
        let worker_name = worker_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| format!("json-writer-{}", stem))
            .unwrap_or_else(|| "json-writer".to_string());

        let mode = match thread::Builder::new()
            .name(worker_name)
            .spawn(move || Self::run_worker(rx, worker_path.as_ref().clone(), debounce))
        {
            Ok(_) => WriterMode::Async { tx },
            Err(e) => {
                warn!(
                    "[JsonWriter] Failed to start background writer for {}: {} — falling back to sync writes",
                    path.display(),
                    e
                );
                WriterMode::Sync
            }
        };

        Self {
            path,
            mode: Arc::new(mode),
        }
    }

    pub fn schedule(&self, value: T) {
        match &*self.mode {
            WriterMode::Sync => Self::write_json(self.path.as_ref(), &value),
            WriterMode::Async { tx } => {
                if let Err(mpsc::SendError(value)) = tx.send(value) {
                    warn!(
                        "[JsonWriter] Background writer for {} is unavailable — writing inline",
                        self.path.display()
                    );
                    Self::write_json(self.path.as_ref(), &value);
                }
            }
        }
    }

    pub fn write_now(&self, value: &T) {
        Self::write_json(self.path.as_ref(), value);
    }

    fn run_worker(rx: mpsc::Receiver<T>, path: PathBuf, debounce: Duration) {
        loop {
            let mut pending = match rx.recv() {
                Ok(value) => value,
                Err(_) => break,
            };

            loop {
                match rx.recv_timeout(debounce) {
                    Ok(value) => pending = value,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        Self::write_json(&path, &pending);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        Self::write_json(&path, &pending);
                        return;
                    }
                }
            }
        }
    }

    fn write_json(path: &Path, value: &T) {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(
                    "[JsonWriter] Failed to create parent dir for {}: {}",
                    path.display(),
                    e
                );
                return;
            }
        }

        match serde_json::to_vec(value) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    warn!("[JsonWriter] Failed to write {}: {}", path.display(), e);
                }
            }
            Err(e) => warn!(
                "[JsonWriter] Failed to serialize {}: {}",
                path.display(),
                e
            ),
        }
    }
}

use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct DecisionLog {
    file: Option<Arc<Mutex<File>>>,
}

impl DecisionLog {
    pub fn disabled() -> Self {
        Self { file: None }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Some(Arc::new(Mutex::new(file))),
        })
    }

    pub fn line(&self, text: impl AsRef<str>) {
        let Some(file) = &self.file else {
            return;
        };

        let Ok(mut file) = file.lock() else {
            return;
        };

        let _ = writeln!(file, "{} {}", local_timestamp(), text.as_ref());
        let _ = file.flush();
    }
}

fn local_timestamp() -> String {
    let now = SystemTime::now();
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as libc::time_t;

    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let tm_ptr = unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) };
    if tm_ptr.is_null() {
        return format!("unix_ms={}", dur.as_millis());
    }

    let tm = unsafe { tm.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        dur.subsec_millis()
    )
}

//! One tracing setup for every binary.
//!
//! Filter with `VICTORY_LOG` (falls back to `RUST_LOG`), e.g.
//! `VICTORY_LOG=victory_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for.
//!
//! Set `VICTORY_LOG_FILE=/path/to/run.log` to mirror all log lines to a file
//! in addition to stderr (ANSI escapes disabled so the saved log is plain text).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

#[derive(Clone)]
struct TeeMakeWriter {
    file: Arc<Mutex<File>>,
}

struct TeeWriter {
    stderr: io::Stderr,
    file: Arc<Mutex<File>>,
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            stderr: io::stderr(),
            file: Arc::clone(&self.file),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.stderr.write_all(buf);
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = self.stderr.flush();
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
        Ok(())
    }
}

fn open_log_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

pub fn init(default_level: &str) {
    let filter = std::env::var("VICTORY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default_level));

    let log_file = std::env::var_os("VICTORY_LOG_FILE")
        .filter(|v| !v.is_empty())
        .and_then(|p| open_log_file(Path::new(&p)).ok());

    if let Some(file) = log_file {
        let tee = TeeMakeWriter {
            file: Arc::new(Mutex::new(file)),
        };
        let _ = tracing_subscriber::registry()
            .with(CAP)
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_ansi(false)
                    .with_writer(tee),
            )
            .try_init();
    } else {
        let _ = tracing_subscriber::registry()
            .with(CAP)
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_writer(io::stderr),
            )
            .try_init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_log_file_creates_parent_dirs_and_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("logs").join("run.log");

        {
            let mut f = open_log_file(&path).unwrap();
            f.write_all(b"first run\n").unwrap();
        }
        {
            let mut f = open_log_file(&path).unwrap();
            f.write_all(b"second run\n").unwrap();
        }

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "second run\n");
    }

    #[test]
    fn tee_writer_mirrors_output_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tee.log");
        let file = open_log_file(&path).unwrap();
        let make = TeeMakeWriter {
            file: Arc::new(Mutex::new(file)),
        };

        let mut writer = make.make_writer();
        writer.write_all(b"hello log\n").unwrap();
        writer.flush().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello log\n");
    }
}

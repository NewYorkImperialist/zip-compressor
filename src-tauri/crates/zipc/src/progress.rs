//! Shared terminal plumbing for the subcommands: the progress line and the
//! Ctrl-C handler.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use zipcore::Progress;

use crate::units::{format_bytes, percent_done, truncate_start};
use crate::EXIT_CANCELLED;

/// Minimum time between progress-line redraws.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// A cancel flag set by Ctrl-C. A second Ctrl-C while the engine is winding
/// down exits at once.
pub fn install_cancel_handler() -> Result<Arc<AtomicBool>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::SeqCst) {
            std::process::exit(EXIT_CANCELLED.into());
        }
    })
    .context("installing Ctrl-C handler")?;
    Ok(cancel)
}

/// A single, self-overwriting status line on stderr.
pub struct ProgressLine {
    enabled: bool,
    /// What `files_done`/`files_total` count, e.g. "files".
    noun: &'static str,
    width: usize,
    last_draw: Option<Instant>,
    drawn: bool,
}

impl ProgressLine {
    pub fn new(enabled: bool, noun: &'static str) -> Self {
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse::<usize>().ok())
            .filter(|&w| w >= 20)
            .unwrap_or(80);
        Self {
            enabled,
            noun,
            width,
            last_draw: None,
            drawn: false,
        }
    }

    pub fn update(&mut self, p: &Progress) {
        if !self.enabled {
            return;
        }
        let complete = p.files_done >= p.files_total && p.bytes_done >= p.bytes_total;
        let due = self
            .last_draw
            .is_none_or(|t| t.elapsed() >= REDRAW_INTERVAL);
        if !due && !complete {
            return;
        }
        self.last_draw = Some(Instant::now());

        let head = format!(
            "{:5.1}%  {} / {}  {}/{} {}  ",
            percent_done(p.bytes_done, p.bytes_total),
            format_bytes(p.bytes_done),
            format_bytes(p.bytes_total),
            p.files_done,
            p.files_total,
            self.noun,
        );
        // Leave the last column free so the terminal never wraps.
        let room = self.width.saturating_sub(head.chars().count() + 1);
        let line = format!("{head}{}", truncate_start(&p.current, room));
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{line}\x1b[K");
        let _ = err.flush();
        self.drawn = true;
    }

    pub fn finish(&mut self) {
        if self.drawn {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[K");
            let _ = err.flush();
            self.drawn = false;
        }
    }
}

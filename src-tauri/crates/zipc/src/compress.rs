//! `zipc compress`.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use zipcore::{CompressOptions, Error, Method, Progress, Summary, SymlinkPolicy};

use crate::units::{
    format_bytes, format_duration, percent_done, ratio_percent, throughput_mb_s, truncate_start,
};
use crate::{Outcome, EXIT_CANCELLED};

/// Minimum time between progress-line redraws.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Args)]
pub struct CompressArgs {
    /// Files and directories to add. Each appears in the archive under its own name.
    #[arg(required = true, value_name = "INPUTS")]
    inputs: Vec<PathBuf>,

    /// Path of the ZIP archive to create.
    #[arg(short, long, value_name = "OUT.zip")]
    output: PathBuf,

    /// Deflate level, 0 (fastest) to 9 (smallest).
    #[arg(short, long, default_value_t = 6, value_parser = clap::value_parser!(u8).range(0..=9))]
    level: u8,

    /// Store entries without compression.
    #[arg(long, conflicts_with = "level")]
    store: bool,

    /// How to treat symbolic links in the inputs.
    #[arg(long, value_enum, default_value_t = Symlinks::Store)]
    symlinks: Symlinks,

    /// Keep OS clutter (.DS_Store, __MACOSX/, ._*, Thumbs.db, desktop.ini).
    #[arg(long)]
    keep_junk: bool,

    /// Do not show progress.
    #[arg(short, long)]
    quiet: bool,

    /// Overwrite the output archive if it already exists.
    #[arg(short, long)]
    force: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Symlinks {
    /// Store the link itself.
    Store,
    /// Archive what the link points to.
    Follow,
    /// Leave links out.
    Skip,
}

impl From<Symlinks> for SymlinkPolicy {
    fn from(s: Symlinks) -> Self {
        match s {
            Symlinks::Store => SymlinkPolicy::Store,
            Symlinks::Follow => SymlinkPolicy::Follow,
            Symlinks::Skip => SymlinkPolicy::Skip,
        }
    }
}

impl CompressArgs {
    fn options(&self) -> CompressOptions {
        CompressOptions {
            method: if self.store {
                Method::Store
            } else {
                Method::Deflate { level: self.level }
            },
            symlinks: self.symlinks.into(),
            exclude_junk: !self.keep_junk,
            ..Default::default()
        }
    }
}

pub fn run(args: CompressArgs) -> Result<Outcome> {
    for input in &args.inputs {
        std::fs::symlink_metadata(input)
            .with_context(|| format!("input not found: {}", input.display()))?;
    }
    if !args.force && std::fs::symlink_metadata(&args.output).is_ok() {
        bail!(
            "{} already exists (use --force to overwrite)",
            args.output.display()
        );
    }

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = Arc::clone(&cancel);
        ctrlc::set_handler(move || {
            // A second Ctrl-C while the engine is winding down exits at once.
            if cancel.swap(true, Ordering::SeqCst) {
                std::process::exit(EXIT_CANCELLED.into());
            }
        })
        .context("installing Ctrl-C handler")?;
    }

    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    let mut bar = ProgressLine::new(show_progress);
    let result = zipcore::compress_to_path(
        &args.inputs,
        &args.output,
        &args.options(),
        &mut |p| bar.update(p),
        &cancel,
    );
    bar.finish();

    match result {
        Ok(summary) => {
            print_summary(&summary);
            Ok(Outcome::Ok)
        }
        Err(Error::Cancelled) => Ok(Outcome::Cancelled),
        Err(e) => Err(e).with_context(|| format!("creating {}", args.output.display())),
    }
}

/// A single, self-overwriting status line on stderr.
struct ProgressLine {
    enabled: bool,
    width: usize,
    last_draw: Option<Instant>,
    drawn: bool,
}

impl ProgressLine {
    fn new(enabled: bool) -> Self {
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse::<usize>().ok())
            .filter(|&w| w >= 20)
            .unwrap_or(80);
        Self {
            enabled,
            width,
            last_draw: None,
            drawn: false,
        }
    }

    fn update(&mut self, p: &Progress) {
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
            "{:5.1}%  {} / {}  {}/{} files  ",
            percent_done(p.bytes_done, p.bytes_total),
            format_bytes(p.bytes_done),
            format_bytes(p.bytes_total),
            p.files_done,
            p.files_total,
        );
        // Leave the last column free so the terminal never wraps.
        let room = self.width.saturating_sub(head.chars().count() + 1);
        let line = format!("{head}{}", truncate_start(&p.current, room));
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{line}\x1b[K");
        let _ = err.flush();
        self.drawn = true;
    }

    fn finish(&mut self) {
        if self.drawn {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[K");
            let _ = err.flush();
            self.drawn = false;
        }
    }
}

fn print_summary(s: &Summary) {
    let ratio = ratio_percent(s.bytes_in, s.bytes_out)
        .map(|r| format!("{r:.1}%"))
        .unwrap_or_else(|| "n/a".into());
    let speed = throughput_mb_s(s.bytes_in, s.elapsed)
        .map(|t| format!("{t:.1} MB/s"))
        .unwrap_or_else(|| "n/a".into());
    println!(
        "{} files, {} dirs, {} symlinks",
        s.files, s.dirs, s.symlinks
    );
    println!(
        "{} -> {} ({ratio} of original)",
        format_bytes(s.bytes_in),
        format_bytes(s.bytes_out)
    );
    println!("{} at {speed}", format_duration(s.elapsed));
}

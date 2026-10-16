//! `zipc compress`.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use zipcore::{CompressOptions, Error, Method, Preset, Summary, SymlinkPolicy};

use crate::progress::{install_cancel_handler, ProgressLine};
use crate::units::{format_bytes, format_duration, ratio_percent, throughput_mb_s};
use crate::Outcome;

#[derive(Debug, Args)]
pub struct CompressArgs {
    /// Files and directories to add. Each appears in the archive under its own name.
    #[arg(required = true, value_name = "INPUTS")]
    inputs: Vec<PathBuf>,

    /// Path of the ZIP archive to create.
    #[arg(short, long, value_name = "OUT.zip")]
    output: PathBuf,

    // Every preset makes a standard ZIP that opens with the tools built into
    // Windows, macOS and Linux. The default is applied in `choice()` so that
    // clap can still reject `--preset` combined with an explicit override.
    /// Speed/size trade-off [default: balanced]
    #[arg(
        short,
        long,
        value_enum,
        value_name = "PRESET",
        conflicts_with_all = ["level", "store", "zopfli_iterations"]
    )]
    preset: Option<PresetArg>,

    /// Advanced: plain Deflate at this level, 0 (fastest) to 9 (smallest).
    #[arg(
        short,
        long,
        value_parser = clap::value_parser!(u8).range(0..=9),
        conflicts_with_all = ["store", "zopfli_iterations"]
    )]
    level: Option<u8>,

    /// Advanced: store entries without compression.
    #[arg(long, conflicts_with = "zopfli_iterations")]
    store: bool,

    /// Advanced: Zopfli with N iterations (preset max uses 15). Much slower.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=1000))]
    zopfli_iterations: Option<u16>,

    /// Worker threads; 0 uses one per CPU core. Output is identical either way.
    #[arg(short = 't', long, default_value_t = 0, value_name = "N")]
    threads: usize,

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

/// The presets as CLI values; doc comments are the `--help` text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PresetArg {
    /// Quickest; somewhat larger files (Deflate level 1).
    Fast,
    /// Good size at high speed; the default (Deflate level 6).
    Balanced,
    /// Smaller Deflate; tries several encoder settings per file. Slower than balanced.
    Best,
    /// Smallest (Zopfli, typically 3-8% below best). Much slower: often 50-100x slower than best.
    Max,
}

impl From<PresetArg> for Preset {
    fn from(p: PresetArg) -> Self {
        match p {
            PresetArg::Fast => Preset::Fast,
            PresetArg::Balanced => Preset::Balanced,
            PresetArg::Best => Preset::Best,
            PresetArg::Max => Preset::Max,
        }
    }
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

/// What the user chose: a named preset, or an explicit method override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Preset(Preset),
    Custom(Method),
}

impl Choice {
    fn method(self) -> Method {
        match self {
            Choice::Preset(p) => p.method(),
            Choice::Custom(m) => m,
        }
    }

    /// e.g. `preset balanced (Deflate level 6)` or `Deflate level 3`.
    fn describe(self) -> String {
        match self {
            Choice::Preset(p) => format!("preset {} ({})", p.name(), describe_method(p.method())),
            Choice::Custom(m) => describe_method(m),
        }
    }
}

fn describe_method(m: Method) -> String {
    match m {
        Method::Store => "Store, no compression".into(),
        Method::Deflate { level } => format!("Deflate level {level}"),
        Method::DeflateBest => "Deflate, best of several settings per small file".into(),
        Method::Zopfli { iterations } => format!("Zopfli, {iterations} iterations"),
    }
}

impl CompressArgs {
    fn choice(&self) -> Choice {
        if self.store {
            Choice::Custom(Method::Store)
        } else if let Some(level) = self.level {
            Choice::Custom(Method::Deflate { level })
        } else if let Some(iterations) = self.zopfli_iterations {
            Choice::Custom(Method::Zopfli { iterations })
        } else {
            Choice::Preset(self.preset.unwrap_or(PresetArg::Balanced).into())
        }
    }

    fn options(&self) -> CompressOptions {
        CompressOptions {
            method: self.choice().method(),
            symlinks: self.symlinks.into(),
            exclude_junk: !self.keep_junk,
            threads: self.threads,
            ..CompressOptions::default()
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

    let cancel = install_cancel_handler()?;

    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    let mut bar = ProgressLine::new(show_progress, "files");
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
            print_summary(&summary, effective_threads(args.threads), args.choice());
            Ok(Outcome::Ok)
        }
        Err(Error::Cancelled) => Ok(Outcome::Cancelled),
        Err(e) => Err(e).with_context(|| format!("creating {}", args.output.display())),
    }
}

/// The thread count a `--threads` value resolves to (0 means every core).
fn effective_threads(requested: usize) -> usize {
    if requested == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        requested
    }
}

fn print_summary(s: &Summary, threads: usize, choice: Choice) {
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
    let unit = if threads == 1 { "thread" } else { "threads" };
    println!(
        "{} at {speed} ({threads} {unit}), {}",
        format_duration(s.elapsed),
        choice.describe()
    );
}

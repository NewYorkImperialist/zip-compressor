//! Tauri shell: exposes the `zipcore` engine to the web front end.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::State;
use zipcore::{CompressOptions, Method, Progress, Summary, SymlinkPolicy};

/// Minimum spacing between progress events sent to the webview (~20/s).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Shared app state: one compression job at a time.
#[derive(Default)]
struct AppState {
    cancel: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MethodDto {
    Store,
    Deflate,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SymlinksDto {
    #[default]
    Store,
    Follow,
    Skip,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompressOptionsDto {
    method: MethodDto,
    /// Deflate level 0-9; defaults to 6. Ignored for `store`.
    level: Option<u8>,
    #[serde(default)]
    symlinks: SymlinksDto,
    #[serde(default = "default_true")]
    exclude_junk: bool,
}

fn default_true() -> bool {
    true
}

impl CompressOptionsDto {
    fn into_options(self) -> Result<CompressOptions, String> {
        let method = match self.method {
            MethodDto::Store => Method::Store,
            MethodDto::Deflate => {
                let level = self.level.unwrap_or(6);
                if level > 9 {
                    return Err(format!("invalid deflate level {level} (expected 0-9)"));
                }
                Method::Deflate { level }
            }
        };
        let symlinks = match self.symlinks {
            SymlinksDto::Store => SymlinkPolicy::Store,
            SymlinksDto::Follow => SymlinkPolicy::Follow,
            SymlinksDto::Skip => SymlinkPolicy::Skip,
        };
        Ok(CompressOptions {
            method,
            symlinks,
            exclude_junk: self.exclude_junk,
            ..Default::default()
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressDto {
    current: String,
    files_done: u64,
    files_total: u64,
    bytes_done: u64,
    bytes_total: u64,
}

impl From<&Progress> for ProgressDto {
    fn from(p: &Progress) -> Self {
        Self {
            current: p.current.clone(),
            files_done: p.files_done,
            files_total: p.files_total,
            bytes_done: p.bytes_done,
            bytes_total: p.bytes_total,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SummaryDto {
    output: String,
    files: u64,
    dirs: u64,
    symlinks: u64,
    bytes_in: u64,
    bytes_out: u64,
    elapsed_ms: u64,
}

impl SummaryDto {
    fn new(s: &Summary, output: String) -> Self {
        Self {
            output,
            files: s.files,
            dirs: s.dirs,
            symlinks: s.symlinks,
            bytes_in: s.bytes_in,
            bytes_out: s.bytes_out,
            elapsed_ms: u64::try_from(s.elapsed.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// Clears the busy flag when the job ends, however it ends.
struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[tauri::command]
async fn compress(
    inputs: Vec<String>,
    output: String,
    options: CompressOptionsDto,
    on_progress: Channel<ProgressDto>,
    state: State<'_, AppState>,
) -> Result<SummaryDto, String> {
    if inputs.is_empty() {
        return Err("nothing to compress".into());
    }
    let opts = options.into_options()?;
    if state
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("a compression job is already running".into());
    }
    let guard = BusyGuard(Arc::clone(&state.busy));
    state.cancel.store(false, Ordering::SeqCst);
    let cancel = Arc::clone(&state.cancel);

    let job = tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        let inputs: Vec<PathBuf> = inputs.into_iter().map(PathBuf::from).collect();
        let out_path = PathBuf::from(&output);

        let mut last_sent: Option<Instant> = None;
        let mut pending: Option<ProgressDto> = None;
        let mut on_engine_progress = |p: &Progress| {
            let dto = ProgressDto::from(p);
            if last_sent.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL) {
                last_sent = Some(Instant::now());
                pending = None;
                let _ = on_progress.send(dto);
            } else {
                pending = Some(dto);
            }
        };
        let result =
            zipcore::compress_to_path(&inputs, &out_path, &opts, &mut on_engine_progress, &cancel);
        // Flush the latest throttled update so the UI ends on the true final state.
        if let Some(dto) = pending {
            let _ = on_progress.send(dto);
        }
        result
            .map(|s| SummaryDto::new(&s, output))
            .map_err(|e| e.to_string())
    });

    job.await
        .map_err(|e| format!("compression task failed: {e}"))?
}

#[tauri::command]
fn cancel(state: State<'_, AppState>) {
    state.cancel.store(true, Ordering::SeqCst);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![compress, cancel])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

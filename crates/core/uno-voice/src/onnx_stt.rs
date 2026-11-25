//! Fast, offline, privacy-first Speech-to-Text engine powered by ONNX Runtime and GigaAM v3 / transcribe-rs.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use async_trait::async_trait;
use futures_util::StreamExt as _;
use ort::environment::GlobalThreadPoolOptions;
use tracing::{info, warn};
use transcribe_rs::onnx::Quantization;
use transcribe_rs::onnx::gigaam::GigaAMModel;
use transcribe_rs::{SpeechModel, TranscribeOptions};

use crate::stt::{DownloadProgressFn, SttEngine, VoiceError};

/// Hugging Face repository for GigaAM v3 ONNX weights (integrity enforced by pinned SHA-256).
const GIGAAM_HF_BASE: &str = "https://huggingface.co/istupakov/gigaam-v3-onnx/resolve/main";
const GIGAAM_MODEL_SHA256: &str =
    "ceb61454e2e1a2dec5872cbac1de0fe0a4271d1148f6b26b5bda53ff30a12acd";
const GIGAAM_VOCAB_SHA256: &str =
    "a9143c30844d3c0bee3e9e927e4084774eb1b9eeaafc473b2c4521e4911a7c07";
static ORT_INIT: OnceLock<()> = OnceLock::new();

fn sha256_hex(digest: &ring::digest::Digest) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for b in digest.as_ref() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn verify_file_sha256(path: &std::path::Path, expected_hex: &str) -> Result<bool, VoiceError> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(sha256_hex(&ctx.finish()) == expected_hex)
}

fn compute_ort_intra_threads(parallelism: usize) -> usize {
    (parallelism / 2).clamp(2, 4)
}

fn init_ort_thread_pool() {
    ORT_INIT.get_or_init(|| {
        let default_threads = compute_ort_intra_threads(
            std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(4),
        );
        let intra_threads = std::env::var("UNATROP_STT_THREADS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .map_or(default_threads, |n| n.clamp(1, 16));

        match GlobalThreadPoolOptions::default()
            .with_intra_threads(intra_threads)
            .and_then(|opts| opts.with_inter_threads(1))
            .and_then(|opts| opts.with_spin_control(false))
        {
            Ok(pool) => {
                let committed = ort::init()
                    .with_name("unatrop-stt")
                    .with_global_thread_pool(pool)
                    .commit();
                info!(
                    intra_threads,
                    committed, "Configured bounded ONNX Runtime global thread pool"
                );
            }
            Err(err) => {
                warn!("Failed to configure ONNX Runtime thread pool options: {err}");
            }
        }
    });
}

#[cfg(windows)]
struct LowPriorityGuard {
    previous: i32,
}

#[cfg(windows)]
impl LowPriorityGuard {
    const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
    const THREAD_PRIORITY_ERROR_RETURN: i32 = i32::MAX;

    fn enter() -> Self {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut core::ffi::c_void;
            fn GetThreadPriority(thread: *mut core::ffi::c_void) -> i32;
            fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
        }
        unsafe {
            let thread = GetCurrentThread();
            let previous = GetThreadPriority(thread);
            let _ = SetThreadPriority(thread, Self::THREAD_PRIORITY_BELOW_NORMAL);
            Self { previous }
        }
    }
}

#[cfg(windows)]
impl Drop for LowPriorityGuard {
    fn drop(&mut self) {
        if self.previous == Self::THREAD_PRIORITY_ERROR_RETURN {
            return;
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut core::ffi::c_void;
            fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
        }
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), self.previous);
        }
    }
}

#[cfg(not(windows))]
struct LowPriorityGuard;

#[cfg(not(windows))]
impl LowPriorityGuard {
    fn enter() -> Self {
        Self
    }
}

/// Fast local ONNX-based STT engine using GigaAM v3 (optimized for Russian, <50ms CPU latency).
pub struct OnnxStt {
    models_root: PathBuf,
    loaded: Arc<Mutex<Option<GigaAMModel>>>,
}

impl OnnxStt {
    /// Creates a new ONNX STT engine storing weights in `models_root/gigaam-v3`.
    pub fn new(models_root: PathBuf) -> Self {
        Self {
            models_root,
            loaded: Arc::new(Mutex::new(None)),
        }
    }

    /// Directory where the GigaAM model weights and vocabulary live.
    pub fn model_dir(&self) -> PathBuf {
        self.models_root.join("gigaam-v3")
    }

    /// Drops the loaded ONNX model weights to reclaim runtime memory when idle.
    pub async fn unload(&self) {
        let mut guard = self.loaded.lock().await;
        *guard = None;
    }

    /// Whether the model weights and vocabulary files are already downloaded.
    pub fn is_downloaded(&self) -> bool {
        let dir = self.model_dir();
        let model_path = dir.join("model.int8.onnx");
        let vocab_path = dir.join("vocab.txt");
        model_path.exists() && vocab_path.exists()
    }

    async fn ensure_files_downloaded(
        &self,
        on_progress: Option<&DownloadProgressFn>,
    ) -> Result<(PathBuf, PathBuf), VoiceError> {
        let dir = self.model_dir();
        std::fs::create_dir_all(&dir)?;

        let model_path = dir.join("model.int8.onnx");
        let vocab_path = dir.join("vocab.txt");

        let files = [
            ("v3_ctc.int8.onnx", &model_path, GIGAAM_MODEL_SHA256),
            ("v3_vocab.txt", &vocab_path, GIGAAM_VOCAB_SHA256),
        ];

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| VoiceError::Download(format!("HTTP client build error: {e}")))?;

        for (filename, dest, expected_sha256) in files {
            if dest.exists() {
                if verify_file_sha256(dest, expected_sha256)? {
                    continue;
                }
                warn!("Existing STT file {filename} failed SHA-256 verification; re-downloading");
            }

            let url = format!("{GIGAAM_HF_BASE}/{filename}");
            let temp_dest = dest.with_extension("tmp");

            info!("Downloading STT weight {filename} from {url}...");

            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| VoiceError::Download(format!("Failed GET {url}: {e}")))?;

            if !resp.status().is_success() {
                return Err(VoiceError::Download(format!(
                    "HTTP {} fetching {url}",
                    resp.status()
                )));
            }

            let total_size = resp.content_length().unwrap_or(0);
            let mut stream = resp.bytes_stream();
            let mut downloaded = 0u64;
            let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);

            let mut out_file = tokio::fs::File::create(&temp_dest).await?;

            while let Some(chunk_res) = stream.next().await {
                let chunk = chunk_res.map_err(|e| {
                    VoiceError::Download(format!("Stream error on {filename}: {e}"))
                })?;
                hasher.update(&chunk);
                tokio::io::AsyncWriteExt::write_all(&mut out_file, &chunk).await?;
                downloaded += chunk.len() as u64;

                if total_size > 0 {
                    let progress = (downloaded as f32) / (total_size as f32);
                    if let Some(cb) = on_progress {
                        cb(progress, filename);
                    }
                }
            }

            tokio::io::AsyncWriteExt::flush(&mut out_file).await?;
            drop(out_file);

            let actual_sha256 = sha256_hex(&hasher.finish());
            if actual_sha256 != expected_sha256 {
                let _ = std::fs::remove_file(&temp_dest);
                return Err(VoiceError::Download(format!(
                    "checksum mismatch for {filename}: expected {expected_sha256}, got {actual_sha256}"
                )));
            }

            std::fs::rename(&temp_dest, dest)?;
            info!("Saved {filename} to {dest:?}");
        }

        Ok((model_path, vocab_path))
    }
}

#[async_trait]
impl SttEngine for OnnxStt {
    async fn ensure_ready(
        &self,
        on_progress: Option<&DownloadProgressFn>,
    ) -> Result<(), VoiceError> {
        let mut guard = self.loaded.lock().await;
        if guard.is_some() {
            return Ok(());
        }

        self.ensure_files_downloaded(on_progress).await?;

        let dir = self.model_dir();
        info!("Loading GigaAM v3 Int8 model from {dir:?}...");

        let model = tokio::task::spawn_blocking(move || {
            let _prio = LowPriorityGuard::enter();
            init_ort_thread_pool();
            GigaAMModel::load(&dir, &Quantization::Int8).map_err(|e| {
                VoiceError::ModelLoad(format!("Failed to load GigaAM ONNX model: {e}"))
            })
        })
        .await
        .map_err(|e| VoiceError::ModelLoad(format!("Join error during model load: {e}")))?;

        *guard = Some(model?);
        info!("GigaAM v3 STT model is ready for real-time inference.");
        Ok(())
    }

    async fn transcribe(
        &self,
        pcm_16khz: &[f32],
        _language: Option<&str>,
    ) -> Result<String, VoiceError> {
        self.ensure_ready(None).await?;

        if pcm_16khz.is_empty() {
            return Ok(String::new());
        }

        let loaded = Arc::clone(&self.loaded);
        let samples = pcm_16khz.to_vec();

        tokio::task::spawn_blocking(move || {
            let _prio = LowPriorityGuard::enter();
            let mut guard = loaded.blocking_lock();
            let model = guard
                .as_mut()
                .ok_or_else(|| VoiceError::Inference("GigaAM model not loaded".to_string()))?;

            let options = TranscribeOptions::default();
            let result = model
                .transcribe(&samples, &options)
                .map_err(|e| VoiceError::Inference(format!("GigaAM transcription failed: {e}")))?;

            Ok(result.text)
        })
        .await
        .map_err(|e| VoiceError::Inference(format!("Join error during transcription: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::compute_ort_intra_threads;

    #[test]
    fn ort_intra_threads_are_bounded() {
        assert_eq!(compute_ort_intra_threads(1), 2);
        assert_eq!(compute_ort_intra_threads(2), 2);
        assert_eq!(compute_ort_intra_threads(6), 3);
        assert_eq!(compute_ort_intra_threads(8), 4);
        assert_eq!(compute_ort_intra_threads(16), 4);
    }
}

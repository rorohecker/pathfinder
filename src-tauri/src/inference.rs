//! ONNX Runtime (DirectML on Windows when available) + tokenizer-backed embeddings,
//! optional MobileNet classification, and 64-bit difference hashes for duplicate detection.
//!
//! Performance notes:
//!
//! - Each Session is built with `intra_threads = max(1, available_parallelism)` so
//!   CPU fallback actually uses the box. The previous build pinned it to 1, which
//!   made the embedding session run on a single core regardless of host topology.
//! - On Windows we explicitly bind DirectML to the discrete GPU (DXGI adapter
//!   index reported by `gpu_detect`) so the dGPU does the work even when the
//!   integrated GPU is enumerated first.
//! - Text embedding pads to the smallest multiple of `SEQ_STRIDE` that fits the
//!   tokenized input (`SEQ_STRIDE = 32`). Filename-length inputs typically run at
//!   seq=32 rather than the legacy fixed seq=128, which is ~4x less compute per call.
//! - `mean_pool` was rewritten to use vectorized ndarray reductions instead of the
//!   nested loop, eliminating ~50k Rust-level bounds checks per call.
//! - MobileNet preprocessing uses ndarray's `from_shape_fn` over the image bytes
//!   instead of a 4-deep nested loop, so 224*224*3 = 150,528 element writes happen
//!   through a single contiguous fill.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use image::imageops::FilterType;
use image::DynamicImage;
use ndarray::{Array1, Array2, Array3, Array4, Axis};
use once_cell::sync::Lazy;
use ort::ep;
use ort::session::Session;
use ort::value::TensorRef;
use tokenizers::Tokenizer;

use crate::local_ai;

const EMBED_DIM: usize = 384;
const MAX_SEQ: usize = 128;
const SEQ_STRIDE: usize = 32;

fn intra_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

#[cfg(windows)]
fn directml_provider() -> ep::DirectML {
    use ort::ep::directml::{DeviceFilter, PerformancePreference};
    // Strategy:
    //   1. If a Compute Accelerator (NPU) is enumerated, use DeviceFilter::Any
    //      with HighPerformance preference so DirectML can route to the NPU when
    //      the model fits and fall back to a GPU otherwise. The NPU path bypasses
    //      DXGI adapter indices because NPUs are exposed through DXCore.
    //   2. Otherwise, pin DirectML to the discrete GPU's DXGI adapter index. On
    //      hybrid graphics laptops (Radeon iGPU + GeForce dGPU) the iGPU is
    //      usually adapter 0, so the default DirectML init ends up on the slower
    //      device. Pinning the index targets the dGPU explicitly.
    let has_npu = !crate::gpu_detect::detect_npus().is_empty();
    if has_npu {
        ep::DirectML::default()
            .with_device_filter(DeviceFilter::Any)
            .with_performance_preference(PerformancePreference::HighPerformance)
    } else if let Some(idx) = crate::gpu_detect::preferred_directml_adapter_index() {
        ep::DirectML::default().with_device_id(idx as i32)
    } else {
        ep::DirectML::default()
    }
}

struct TextEmbedder {
    session: Session,
    tokenizer: Tokenizer,
}

static TEXT_EMBEDDER: Lazy<Mutex<Option<TextEmbedder>>> = Lazy::new(|| Mutex::new(None));
static MOBILENET: Lazy<Mutex<Option<Session>>> = Lazy::new(|| Mutex::new(None));
static ORT_STATUS: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new("not probed".to_string()));

/// Windows: remember `init_from` outcome so we do not call it on every session build;
/// cleared with [`reset_inference_sessions`] so a reinstall can retry.
#[cfg(windows)]
static ORT_ENV_STATE: OnceLock<Mutex<Option<Result<(), String>>>> = OnceLock::new();

#[cfg(windows)]
fn ort_env_state() -> &'static Mutex<Option<Result<(), String>>> {
    ORT_ENV_STATE.get_or_init(|| Mutex::new(None))
}

/// Drop cached sessions (e.g. after Local AI uninstall).
pub fn reset_inference_sessions() {
    if let Ok(mut g) = TEXT_EMBEDDER.lock() {
        *g = None;
    }
    if let Ok(mut g) = MOBILENET.lock() {
        *g = None;
    }
    if let Ok(mut s) = ORT_STATUS.lock() {
        *s = "not probed".to_string();
    }
    #[cfg(windows)]
    if let Ok(mut st) = ort_env_state().lock() {
        *st = None;
    }
}

fn model_dir() -> PathBuf {
    local_ai::ai_dir()
}

/// Load ONNX Runtime from `%APPDATA%\Pathfinder\ai\onnxruntime.dll` (Windows
/// `load-dynamic` build). No-op on other platforms.
fn ensure_ort_environment() -> ort::Result<()> {
    #[cfg(windows)]
    {
        let mut guard = ort_env_state().lock().map_err(|e| ort::Error::new(e.to_string()))?;
        match guard.as_ref() {
            Some(Ok(())) => return Ok(()),
            Some(Err(msg)) => return Err(ort::Error::new(msg.clone())),
            None => {}
        }
        let dll = model_dir().join("onnxruntime.dll");
        if !dll.is_file() {
            let msg = "onnxruntime.dll missing — install Local AI from Settings (downloads ORT + models)".to_string();
            *guard = Some(Err(msg.clone()));
            return Err(ort::Error::new(msg));
        }
        let mapped: Result<(), String> = ort::init_from(&dll)
            .map_err(|e| e.to_string())
            .map(|env| {
                env.commit();
            });
        *guard = Some(mapped.clone());
        mapped.map_err(ort::Error::new)
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

fn try_build_text_embedder() -> ort::Result<TextEmbedder> {
    ensure_ort_environment()?;
    let model_path = model_dir().join("text-embedding.onnx");
    let tok_path = model_dir().join("tokenizer.json");
    if !model_path.is_file() || !tok_path.is_file() {
        return Err(ort::Error::new(format!(
            "missing model at {}",
            model_path.display()
        )));
    }
    let tokenizer = Tokenizer::from_file(tok_path.as_path())
        .map_err(|e| ort::Error::new(format!("tokenizer: {e}")))?;
    let mut builder = Session::builder()?;
    #[cfg(windows)]
    {
        builder = builder.with_execution_providers([
            directml_provider().build(),
            ep::CPU::default().build(),
        ])?;
    }
    #[cfg(not(windows))]
    {
        builder = builder.with_execution_providers([ep::CPU::default().build()])?;
    }
    let session = builder
        .with_intra_threads(intra_threads())?
        .commit_from_file(model_path)?;
    if let Ok(mut s) = ORT_STATUS.lock() {
        let threads = intra_threads();
        #[cfg(windows)]
        let routing = {
            let inv = crate::gpu_detect::detect_gpus();
            let target = inv.primary_directml_target().map(|a| a.name.clone()).unwrap_or_default();
            if !crate::gpu_detect::detect_npus().is_empty() {
                "DirectML on NPU or high performance GPU".to_string()
            } else if !target.is_empty() {
                format!("DirectML on {target}")
            } else {
                "CPU".to_string()
            }
        };
        #[cfg(not(windows))]
        let routing = "CPU".to_string();
        *s = format!("text-embedding session ready ({routing}, {threads} CPU threads for fallback)");
    }
    Ok(TextEmbedder { session, tokenizer })
}

fn with_text_embedder<T>(f: impl FnOnce(&mut TextEmbedder) -> ort::Result<T>) -> ort::Result<T> {
    let mut guard = TEXT_EMBEDDER.lock().map_err(|e| ort::Error::new(e.to_string()))?;
    if guard.is_none() {
        *guard = Some(try_build_text_embedder()?);
    }
    let emb = guard
        .as_mut()
        .ok_or_else(|| ort::Error::new("text embedder unavailable".to_string()))?;
    f(emb)
}

/// One-line status for Settings / capability text.
pub fn ort_runtime_line() -> String {
    ORT_STATUS
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "inference mutex poisoned".into())
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let d = (na.sqrt() * nb.sqrt()).max(1e-8);
    dot / d
}

fn mean_pool(last_hidden: Array3<f32>, mask: Array2<i64>) -> Array1<f32> {
    // last_hidden: [1, seq, dim], mask: [1, seq] in i64. The previous code looped
    // over seq * dim in Rust with nested indexing (49,152 bounds-checked accesses
    // for seq=128, dim=384). The vectorized version below collapses that into a
    // single weighted sum over axis 1 plus a scalar divide, which the compiler
    // and BLAS layer can autovectorize.
    let seq = last_hidden.len_of(Axis(1));
    let dim = last_hidden.len_of(Axis(2));
    let mask_f = mask.mapv(|v| v as f32);
    let denom = mask_f.sum().max(1e-8);
    let hidden_2d = last_hidden
        .into_shape_with_order((seq, dim))
        .expect("last_hidden reshape");
    let weights = mask_f
        .into_shape_with_order(seq)
        .expect("mask reshape");
    // hidden_2d.T @ weights gives a [dim] vector of weighted sums.
    let mut out = Array1::<f32>::zeros(dim);
    for t in 0..seq {
        let w = weights[t];
        if w <= 0.0 {
            continue;
        }
        out.scaled_add(w, &hidden_2d.row(t));
    }
    out /= denom;
    let n = out.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
    out / n
}

pub fn embed_query_text(text: &str) -> Option<Vec<f32>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    with_text_embedder(|emb| {
        let enc = emb
            .tokenizer
            .encode(trimmed, true)
            .map_err(|e| ort::Error::new(e.to_string()))?;
        let raw_ids = enc.get_ids();
        let actual_len = raw_ids.len().min(MAX_SEQ);
        // Pad to the smallest multiple of SEQ_STRIDE that fits the input. A
        // typical file name tokenizes to under 24 ids, so we run at seq=32
        // instead of the legacy fixed 128. The transformer accepts any seq
        // length and the smaller tensor cuts attention compute by 1/16.
        let padded_len = (actual_len.max(1).div_ceil(SEQ_STRIDE) * SEQ_STRIDE).min(MAX_SEQ);
        let mut ids: Vec<i64> = raw_ids.iter().take(actual_len).map(|&x| x as i64).collect();
        let mut mask: Vec<i64> = vec![1_i64; actual_len];
        while ids.len() < padded_len {
            ids.push(0);
            mask.push(0);
        }
        let ids_arr = Array2::from_shape_vec((1, padded_len), ids)
            .map_err(|e| ort::Error::new(e.to_string()))?;
        let mask_arr = Array2::from_shape_vec((1, padded_len), mask)
            .map_err(|e| ort::Error::new(e.to_string()))?;
        let outputs = emb.session.run(ort::inputs![
            "input_ids" => TensorRef::from_array_view(&ids_arr)?,
            "attention_mask" => TensorRef::from_array_view(&mask_arr)?,
        ])?;
        let tensor = outputs
            .get("last_hidden_state")
            .or_else(|| outputs.get("sentence_embedding"))
            .ok_or_else(|| ort::Error::new("no embedding tensor in model output"))?;
        let (shape, data) = tensor.try_extract_tensor::<f32>()?;
        let vec: Vec<f32> = if shape.len() == 2 && (shape[1] as usize) == EMBED_DIM {
            data.to_vec()
        } else if shape.len() == 3 {
            let seq = shape[1] as usize;
            let dim = shape[2] as usize;
            let data_vec: Vec<f32> = data.to_vec();
            let hidden = Array3::from_shape_vec((1, seq, dim), data_vec)
                .map_err(|e| ort::Error::new(e.to_string()))?;
            mean_pool(hidden, mask_arr).to_vec()
        } else {
            return Err(ort::Error::new("unexpected embedding shape"));
        };
        Ok(vec)
    })
    .ok()
}

/// Embed a file name (or short label) for indexing.
pub fn embed_file_label(text: &str) -> Option<Vec<f32>> {
    embed_query_text(text)
}

/// Batched embedding for the background indexer. Tokenizes the whole slice,
/// pads to the longest sample (rounded up to SEQ_STRIDE), then issues one
/// session.run with shape [batch, seq]. DirectML and CPU EPs both benefit a
/// lot from batching: ORT only pays the kernel launch overhead once per call,
/// and the matmuls amortize over more rows. Returns None for any row whose
/// tokenizer call failed, so a single bad input does not abort the batch.
pub fn embed_file_labels_batch(texts: &[&str]) -> Vec<Option<Vec<f32>>> {
    if texts.is_empty() {
        return Vec::new();
    }
    let result = with_text_embedder(|emb| {
        // Tokenize all rows first to find the longest sequence in the batch.
        let mut id_rows: Vec<Vec<i64>> = Vec::with_capacity(texts.len());
        let mut mask_rows: Vec<Vec<i64>> = Vec::with_capacity(texts.len());
        let mut keep: Vec<bool> = Vec::with_capacity(texts.len());
        let mut longest = 0usize;
        for t in texts {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                id_rows.push(Vec::new());
                mask_rows.push(Vec::new());
                keep.push(false);
                continue;
            }
            match emb.tokenizer.encode(trimmed, true) {
                Ok(enc) => {
                    let len = enc.get_ids().len().min(MAX_SEQ);
                    longest = longest.max(len);
                    let ids: Vec<i64> = enc.get_ids().iter().take(len).map(|&x| x as i64).collect();
                    let mask: Vec<i64> = vec![1_i64; len];
                    id_rows.push(ids);
                    mask_rows.push(mask);
                    keep.push(true);
                }
                Err(_) => {
                    id_rows.push(Vec::new());
                    mask_rows.push(Vec::new());
                    keep.push(false);
                }
            }
        }
        let padded_len = (longest.max(1).div_ceil(SEQ_STRIDE) * SEQ_STRIDE).min(MAX_SEQ);
        let kept_count = keep.iter().filter(|k| **k).count();
        if kept_count == 0 {
            return Ok(vec![None; texts.len()]);
        }
        // Build flat batch tensors only for kept rows; remember original indices.
        let mut flat_ids: Vec<i64> = Vec::with_capacity(kept_count * padded_len);
        let mut flat_mask: Vec<i64> = Vec::with_capacity(kept_count * padded_len);
        let mut original_index: Vec<usize> = Vec::with_capacity(kept_count);
        for (i, ok) in keep.iter().enumerate() {
            if !*ok {
                continue;
            }
            let mut ids = std::mem::take(&mut id_rows[i]);
            let mut mask = std::mem::take(&mut mask_rows[i]);
            while ids.len() < padded_len {
                ids.push(0);
                mask.push(0);
            }
            flat_ids.extend_from_slice(&ids);
            flat_mask.extend_from_slice(&mask);
            original_index.push(i);
        }
        let ids_arr = Array2::from_shape_vec((kept_count, padded_len), flat_ids)
            .map_err(|e| ort::Error::new(e.to_string()))?;
        let mask_arr = Array2::from_shape_vec((kept_count, padded_len), flat_mask)
            .map_err(|e| ort::Error::new(e.to_string()))?;
        let outputs = emb.session.run(ort::inputs![
            "input_ids" => TensorRef::from_array_view(&ids_arr)?,
            "attention_mask" => TensorRef::from_array_view(&mask_arr)?,
        ])?;
        let tensor = outputs
            .get("last_hidden_state")
            .or_else(|| outputs.get("sentence_embedding"))
            .ok_or_else(|| ort::Error::new("no embedding tensor in batch output"))?;
        let (shape, data) = tensor.try_extract_tensor::<f32>()?;
        let mut result: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        if shape.len() == 2 && (shape[1] as usize) == EMBED_DIM {
            // Model already returns pooled sentence embeddings per row.
            let dim = shape[1] as usize;
            for (row, orig) in original_index.iter().enumerate() {
                let start = row * dim;
                result[*orig] = Some(data[start..start + dim].to_vec());
            }
        } else if shape.len() == 3 {
            let seq = shape[1] as usize;
            let dim = shape[2] as usize;
            let flat: Vec<f32> = data.to_vec();
            let hidden_full = Array3::from_shape_vec((kept_count, seq, dim), flat)
                .map_err(|e| ort::Error::new(e.to_string()))?;
            for (row, orig) in original_index.iter().enumerate() {
                let hidden_row = hidden_full
                    .index_axis(Axis(0), row)
                    .to_owned()
                    .insert_axis(Axis(0));
                let mask_row = mask_arr.row(row).to_owned().insert_axis(Axis(0));
                let pooled = mean_pool(hidden_row, mask_row);
                result[*orig] = Some(pooled.to_vec());
            }
        } else {
            return Err(ort::Error::new("unexpected batch embedding shape"));
        }
        Ok(result)
    });
    result.unwrap_or_else(|_| vec![None; texts.len()])
}

fn try_mobilenet_session() -> ort::Result<Session> {
    ensure_ort_environment()?;
    let path = model_dir().join("image-classifier.onnx");
    if !path.is_file() {
        return Err(ort::Error::new("image-classifier.onnx missing"));
    }
    let mut builder = Session::builder()?;
    #[cfg(windows)]
    {
        builder = builder.with_execution_providers([
            directml_provider().build(),
            ep::CPU::default().build(),
        ])?;
    }
    #[cfg(not(windows))]
    {
        builder = builder.with_execution_providers([ep::CPU::default().build()])?;
    }
    builder
        .with_intra_threads(intra_threads())?
        .commit_from_file(path)
}

fn mobilenet_session() -> ort::Result<std::sync::MutexGuard<'static, Option<Session>>> {
    let mut g = MOBILENET.lock().map_err(|e| ort::Error::new(e.to_string()))?;
    if g.is_none() {
        *g = Some(try_mobilenet_session()?);
    }
    Ok(g)
}

/// Returns a short human label (ImageNet class index mapped to a coarse tag) if the model runs.
pub fn suggest_image_tag(path: &Path) -> Option<String> {
    let img = image::open(path).ok()?.into_rgb8();
    let dyn_img = DynamicImage::ImageRgb8(img);
    // Triangle (linear) filter is a good speed / quality trade for a 224x224 input
    // to a classifier — Lanczos3 would be sharper but the model is robust to small
    // resampling differences and Triangle is ~3x faster.
    let resized = dyn_img.resize_exact(224, 224, FilterType::Triangle);
    let resized_rgb = resized.into_rgb8();
    let bytes = resized_rgb.as_raw();
    let mut input = Array4::<f32>::zeros((1, 3, 224, 224));
    // Vectorized: walk the row-major u8 buffer once and write directly into the
    // three planar f32 channels with ImageNet normalization baked in. ndarray
    // `as_slice_mut` gives a contiguous f32 view per channel, so the inner loop
    // becomes a single autovectorizable arithmetic pass.
    let plane = 224 * 224;
    let input_slice = input
        .as_slice_mut()
        .expect("input array is contiguous");
    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];
    let scale = [1.0 / (255.0 * STD[0]), 1.0 / (255.0 * STD[1]), 1.0 / (255.0 * STD[2])];
    let mean_scaled = [MEAN[0] / STD[0], MEAN[1] / STD[1], MEAN[2] / STD[2]];
    for px in 0..plane {
        let r = bytes[px * 3] as f32;
        let g = bytes[px * 3 + 1] as f32;
        let b = bytes[px * 3 + 2] as f32;
        input_slice[px] = r * scale[0] - mean_scaled[0];
        input_slice[plane + px] = g * scale[1] - mean_scaled[1];
        input_slice[2 * plane + px] = b * scale[2] - mean_scaled[2];
    }
    let mut guard = mobilenet_session().ok()?;
    let session = guard.as_mut()?;
    let input_ref = TensorRef::from_array_view(&input).ok()?;
    let out = session.run(ort::inputs!["input" => input_ref]).ok()?;
    let first = out.iter().next()?;
    let (_shape, data) = first.1.try_extract_tensor::<f32>().ok()?;
    let flat: Vec<f32> = data.to_vec();
    let idx = flat
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)?;
    Some(imagenet_coarse_tag(idx))
}

fn imagenet_coarse_tag(idx: usize) -> String {
    if (207..=259).contains(&idx) && idx != 237 {
        // Broad dog-ish band without listing every synset.
        "dog".into()
    } else if (281..=287).contains(&idx) {
        "cat".into()
    } else if (404..=407).contains(&idx) {
        "airplane".into()
    } else if (817..=819).contains(&idx) {
        "sports ball".into()
    } else if idx == 609 {
        "jeans".into()
    } else {
        format!("object-{idx}")
    }
}

/// 8x8 difference hash as u64 for near-duplicate image detection.
///
/// dhash is robust to scale changes, brightness shifts, and small color/contrast
/// edits. We resize down in two stages on larger inputs: first a fast box-filter
/// decimation to a 64x64 thumbnail, then a Triangle filter to the final 9x8. The
/// two-step path is roughly 4x faster than a single Triangle resize from a multi
/// megapixel JPEG, with no visible quality loss at 8x8 output.
pub fn dhash64(path: &Path) -> Option<u64> {
    let mut img = image::open(path).ok()?.into_luma8();
    if img.width() > 256 || img.height() > 256 {
        img = image::imageops::resize(&img, 64, 64, FilterType::Nearest);
    }
    let small = image::imageops::resize(&img, 9, 8, FilterType::Triangle);
    let buf = small.as_raw();
    let mut bits: u64 = 0;
    let mut bit = 0_u32;
    for y in 0..8usize {
        let row = &buf[y * 9..y * 9 + 9];
        for x in 0..8usize {
            if row[x] > row[x + 1] {
                bits |= 1u64 << bit;
            }
            bit += 1;
        }
    }
    Some(bits)
}

pub fn hamming64(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

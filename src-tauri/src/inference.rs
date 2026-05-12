//! ONNX Runtime (DirectML on Windows when available) + tokenizer-backed embeddings,
//! optional MobileNet classification, and 64-bit difference hashes for duplicate detection.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use ndarray::{Array1, Array2, Array3, Array4, Axis};
use once_cell::sync::Lazy;
use ort::ep;
use ort::session::Session;
use ort::value::TensorRef;
use tokenizers::Tokenizer;

use crate::local_ai;

const EMBED_DIM: usize = 384;
const MAX_SEQ: usize = 128;

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
            ep::DirectML::default().build(),
            ep::CPU::default().build(),
        ])?;
    }
    #[cfg(not(windows))]
    {
        builder = builder.with_execution_providers([ep::CPU::default().build()])?;
    }
    let session = builder
        .with_intra_threads(1)?
        .commit_from_file(model_path)?;
    if let Ok(mut s) = ORT_STATUS.lock() {
        *s = "text-embedding session ready (DirectML preferred on Windows)".to_string();
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
    // last_hidden: [1, seq, dim], mask: [1, seq]
    let dim = last_hidden.len_of(Axis(2));
    let mut out = Array1::<f32>::zeros(dim);
    let mut denom = 0.0_f32;
    let seq = last_hidden.len_of(Axis(1));
    for t in 0..seq {
        let m = mask[[0, t]] as f32;
        if m <= 0.0 {
            continue;
        }
        denom += m;
        for d in 0..dim {
            out[d] += last_hidden[[0, t, d]] * m;
        }
    }
    if denom > 0.0 {
        out /= denom;
    }
    // L2 normalize
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
        let ids_i = enc.get_ids();
        let mut ids: Vec<i64> = ids_i.iter().take(MAX_SEQ).map(|&x| x as i64).collect();
        let mut mask: Vec<i64> = vec![1_i64; ids.len()];
        while ids.len() < MAX_SEQ {
            ids.push(0);
            mask.push(0);
        }
        let ids_arr = Array2::from_shape_vec((1, MAX_SEQ), ids).map_err(|e| ort::Error::new(e.to_string()))?;
        let mask_arr = Array2::from_shape_vec((1, MAX_SEQ), mask).map_err(|e| ort::Error::new(e.to_string()))?;
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
            let hidden =
                Array3::from_shape_vec((1, seq, dim), data_vec).map_err(|e| ort::Error::new(e.to_string()))?;
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
            ep::DirectML::default().build(),
            ep::CPU::default().build(),
        ])?;
    }
    #[cfg(not(windows))]
    {
        builder = builder.with_execution_providers([ep::CPU::default().build()])?;
    }
    builder.with_intra_threads(1)?.commit_from_file(path)
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
    let resized = dyn_img.resize_exact(224, 224, FilterType::Triangle);
    let mut input = Array4::<f32>::zeros((1, 3, 224, 224));
    for y in 0..224 {
        for x in 0..224 {
            let p = resized.get_pixel(x as u32, y as u32);
            let r = p[0] as f32 / 255.0;
            let gch = p[1] as f32 / 255.0;
            let b = p[2] as f32 / 255.0;
            input[[0, 0, y, x]] = (r - 0.485) / 0.229;
            input[[0, 1, y, x]] = (gch - 0.456) / 0.224;
            input[[0, 2, y, x]] = (b - 0.406) / 0.225;
        }
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
pub fn dhash64(path: &Path) -> Option<u64> {
    let img = image::open(path).ok()?.into_luma8();
    let small = image::imageops::resize(&img, 9, 8, FilterType::Triangle);
    let mut bits: u64 = 0;
    let mut bit = 0_u32;
    for y in 0..8 {
        for x in 0..8 {
            let left = small.get_pixel(x, y).0[0] as i16;
            let right = small.get_pixel(x + 1, y).0[0] as i16;
            if left > right {
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

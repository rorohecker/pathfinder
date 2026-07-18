//! ONNX Runtime inference with NPU → GPU → CPU provider smoke-tests,
//! tokenizer-backed embeddings, image classification, and dHash duplicates.

use std::collections::HashSet;
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

use crate::imagenet_labels;
use crate::local_ai;
use crate::winml_bridge::{self, AcceleratorKind, ProviderCandidate};

const DEFAULT_EMBED_DIM: usize = 384;
const DEFAULT_MAX_SEQ: usize = 128;
const SEQ_STRIDE: usize = 32;
/// Near-duplicate Hamming distance for 64-bit dHash (0 = identical hash).
pub const DHASH_NEAR_DUP_THRESHOLD: u32 = 10;
const CLASSIFY_MIN_CONFIDENCE: f32 = 0.08;

fn intra_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

#[derive(Debug, Clone)]
pub struct SelectedProvider {
    pub label: String,
    pub kind: AcceleratorKind,
    pub tag: String,
}

static SELECTED_PROVIDER: Lazy<Mutex<Option<SelectedProvider>>> = Lazy::new(|| Mutex::new(None));
static TEXT_EMBEDDER: Lazy<Mutex<Option<TextEmbedder>>> = Lazy::new(|| Mutex::new(None));
static CLASSIFIER: Lazy<Mutex<Option<ClassifierSession>>> = Lazy::new(|| Mutex::new(None));
static ORT_STATUS: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new("not probed".to_string()));

#[cfg(windows)]
static ORT_ENV_STATE: OnceLock<Mutex<Option<Result<(), String>>>> = OnceLock::new();

#[cfg(windows)]
fn ort_env_state() -> &'static Mutex<Option<Result<(), String>>> {
    ORT_ENV_STATE.get_or_init(|| Mutex::new(None))
}

struct TextEmbedder {
    session: Session,
    tokenizer: Tokenizer,
    embed_dim: usize,
    max_seq: usize,
    query_prefix: String,
}

struct ClassifierSession {
    session: Session,
    input_name: String,
    input_size: u32,
}

/// Drop cached sessions (e.g. after Local AI uninstall / model update).
pub fn reset_inference_sessions() {
    if let Ok(mut g) = TEXT_EMBEDDER.lock() {
        *g = None;
    }
    if let Ok(mut g) = CLASSIFIER.lock() {
        *g = None;
    }
    if let Ok(mut s) = ORT_STATUS.lock() {
        *s = "not probed".to_string();
    }
    if let Ok(mut p) = SELECTED_PROVIDER.lock() {
        *p = None;
    }
    #[cfg(windows)]
    if let Ok(mut st) = ort_env_state().lock() {
        *st = None;
    }
}

pub fn selected_provider() -> Option<SelectedProvider> {
    SELECTED_PROVIDER.lock().ok().and_then(|g| g.clone())
}

pub fn selected_accelerator_kind() -> Option<&'static str> {
    selected_provider().map(|p| p.kind.as_str())
}

fn model_dir() -> PathBuf {
    local_ai::ai_dir()
}

fn ensure_ort_environment() -> ort::Result<()> {
    #[cfg(windows)]
    {
        let mut guard = ort_env_state()
            .lock()
            .map_err(|e| ort::Error::new(e.to_string()))?;
        match guard.as_ref() {
            Some(Ok(())) => return Ok(()),
            Some(Err(msg)) => return Err(ort::Error::new(msg.clone())),
            None => {}
        }
        let dll = model_dir().join("onnxruntime.dll");
        if !dll.is_file() {
            let msg = "onnxruntime.dll missing - install Local AI from Settings".to_string();
            *guard = Some(Err(msg.clone()));
            return Err(ort::Error::new(msg));
        }
        // Best-effort: register any discovered Windows ML EP libraries.
        for (name, path) in winml_bridge::discover_winml_ep_libraries() {
            let _ = (name, path);
            // ort 2.0-rc load-dynamic does not expose a stable public
            // RegisterExecutionProviderLibrary wrapper in all builds; discovery
            // still informs candidate ordering and status text.
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

#[cfg(windows)]
fn eps_for_candidate(c: &ProviderCandidate) -> Vec<ort::ep::ExecutionProviderDispatch> {
    use ort::ep::directml::{DeviceFilter, PerformancePreference};
    let mut eps = Vec::new();
    match c.tag.as_str() {
        "dml:npu" => {
            eps.push(
                ep::DirectML::default()
                    .with_device_filter(DeviceFilter::Npu)
                    .with_performance_preference(PerformancePreference::HighPerformance)
                    .build(),
            );
        }
        "dml:any_hp" => {
            eps.push(
                ep::DirectML::default()
                    .with_device_filter(DeviceFilter::Any)
                    .with_performance_preference(PerformancePreference::HighPerformance)
                    .build(),
            );
        }
        "dml:default" => {
            eps.push(ep::DirectML::default().build());
        }
        "cpu" => {
            eps.push(ep::CPU::default().build());
        }
        tag if tag.starts_with("dml:idx:") => {
            if let Ok(idx) = tag.trim_start_matches("dml:idx:").parse::<i32>() {
                eps.push(ep::DirectML::default().with_device_id(idx).build());
            } else {
                eps.push(ep::DirectML::default().build());
            }
        }
        tag if tag.starts_with("winml:") => {
            // Vendor EP registration is environment-dependent. Prefer DirectML
            // NPU/GPU path when the named EP cannot be appended via ort API.
            if c.kind == AcceleratorKind::Npu {
                eps.push(
                    ep::DirectML::default()
                        .with_device_filter(DeviceFilter::Npu)
                        .with_performance_preference(PerformancePreference::HighPerformance)
                        .build(),
                );
            }
            eps.push(ep::DirectML::default().build());
            eps.push(ep::CPU::default().build());
        }
        _ => {
            eps.push(ep::CPU::default().build());
        }
    }
    // Always allow CPU fallback inside the session if the preferred EP rejects nodes.
    if c.tag != "cpu" {
        eps.push(ep::CPU::default().build());
    }
    eps
}

#[cfg(not(windows))]
fn eps_for_candidate(_c: &ProviderCandidate) -> Vec<ort::ep::ExecutionProviderDispatch> {
    vec![ep::CPU::default().build()]
}

fn try_session_with_candidate(
    model_path: &Path,
    candidate: &ProviderCandidate,
) -> ort::Result<Session> {
    Session::builder()?
        .with_execution_providers(eps_for_candidate(candidate))?
        .with_intra_threads(intra_threads())?
        .commit_from_file(model_path)
}

/// Probe candidates in NPU → GPU → CPU order; keep the first session that builds.
fn open_session_preferring_hardware(model_path: &Path) -> ort::Result<(Session, SelectedProvider)> {
    ensure_ort_environment()?;
    let candidates = winml_bridge::provider_candidates();
    let mut last_err: Option<ort::Error> = None;
    for c in candidates {
        match try_session_with_candidate(model_path, &c) {
            Ok(session) => {
                let selected = SelectedProvider {
                    label: c.label.clone(),
                    kind: c.kind,
                    tag: c.tag.clone(),
                };
                if let Ok(mut g) = SELECTED_PROVIDER.lock() {
                    *g = Some(selected.clone());
                }
                return Ok((session, selected));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| ort::Error::new("no execution provider available")))
}

fn try_build_text_embedder() -> ort::Result<TextEmbedder> {
    let model_path = model_dir().join("text-embedding.onnx");
    let tok_path = model_dir().join("tokenizer.json");
    if !model_path.is_file() || !tok_path.is_file() {
        return Err(ort::Error::new(format!(
            "missing model at {}",
            model_path.display()
        )));
    }
    let info = local_ai::active_model_info();
    let embed_dim = info
        .as_ref()
        .map(|i| i.embedding_dim as usize)
        .unwrap_or(DEFAULT_EMBED_DIM);
    let max_seq = info
        .as_ref()
        .map(|i| i.max_seq)
        .unwrap_or(DEFAULT_MAX_SEQ)
        .clamp(32, 512);
    let query_prefix = info
        .as_ref()
        .map(|i| i.query_prefix.clone())
        .unwrap_or_default();

    let tokenizer = Tokenizer::from_file(tok_path.as_path())
        .map_err(|e| ort::Error::new(format!("tokenizer: {e}")))?;
    let (session, selected) = open_session_preferring_hardware(&model_path)?;
    if let Ok(mut s) = ORT_STATUS.lock() {
        *s = format!(
            "text-embedding ready on {} [{}] ({} threads)",
            selected.label,
            selected.kind.as_str(),
            intra_threads()
        );
    }
    Ok(TextEmbedder {
        session,
        tokenizer,
        embed_dim,
        max_seq,
        query_prefix,
    })
}

fn with_text_embedder<T>(f: impl FnOnce(&mut TextEmbedder) -> ort::Result<T>) -> ort::Result<T> {
    let mut guard = TEXT_EMBEDDER
        .lock()
        .map_err(|e| ort::Error::new(e.to_string()))?;
    if guard.is_none() {
        *guard = Some(try_build_text_embedder()?);
    }
    let emb = guard
        .as_mut()
        .ok_or_else(|| ort::Error::new("text embedder unavailable".to_string()))?;
    f(emb)
}

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
    let seq = last_hidden.len_of(Axis(1));
    let dim = last_hidden.len_of(Axis(2));
    let mask_f = mask.mapv(|v| v as f32);
    let denom = mask_f.sum().max(1e-8);
    let hidden_2d = last_hidden
        .into_shape_with_order((seq, dim))
        .expect("last_hidden reshape");
    let weights = mask_f.into_shape_with_order(seq).expect("mask reshape");
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

fn prepare_text(emb: &TextEmbedder, text: &str) -> String {
    let trimmed = text.trim();
    if emb.query_prefix.is_empty() {
        trimmed.to_string()
    } else {
        format!("{}{trimmed}", emb.query_prefix)
    }
}

pub fn embed_query_text(text: &str) -> Option<Vec<f32>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    with_text_embedder(|emb| {
        let input = prepare_text(emb, trimmed);
        let enc = emb
            .tokenizer
            .encode(input.as_str(), true)
            .map_err(|e| ort::Error::new(e.to_string()))?;
        let raw_ids = enc.get_ids();
        let actual_len = raw_ids.len().min(emb.max_seq);
        let padded_len = (actual_len.max(1).div_ceil(SEQ_STRIDE) * SEQ_STRIDE).min(emb.max_seq);
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
        let vec: Vec<f32> = if shape.len() == 2 && (shape[1] as usize) == emb.embed_dim {
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

pub fn embed_file_label(text: &str) -> Option<Vec<f32>> {
    // File labels should not use retrieval query prefixes.
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    with_text_embedder(|emb| {
        let saved = emb.query_prefix.clone();
        emb.query_prefix.clear();
        let enc = emb
            .tokenizer
            .encode(trimmed, true)
            .map_err(|e| ort::Error::new(e.to_string()));
        emb.query_prefix = saved;
        let enc = enc?;
        let raw_ids = enc.get_ids();
        let actual_len = raw_ids.len().min(emb.max_seq);
        let padded_len = (actual_len.max(1).div_ceil(SEQ_STRIDE) * SEQ_STRIDE).min(emb.max_seq);
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
            .ok_or_else(|| ort::Error::new("no embedding tensor"))?;
        let (shape, data) = tensor.try_extract_tensor::<f32>()?;
        if shape.len() == 2 {
            Ok(data.to_vec())
        } else if shape.len() == 3 {
            let seq = shape[1] as usize;
            let dim = shape[2] as usize;
            let hidden = Array3::from_shape_vec((1, seq, dim), data.to_vec())
                .map_err(|e| ort::Error::new(e.to_string()))?;
            Ok(mean_pool(hidden, mask_arr).to_vec())
        } else {
            Err(ort::Error::new("unexpected shape"))
        }
    })
    .ok()
}

pub fn embed_file_labels_batch(texts: &[&str]) -> Vec<Option<Vec<f32>>> {
    if texts.is_empty() {
        return Vec::new();
    }
    let result = with_text_embedder(|emb| {
        let saved_prefix = emb.query_prefix.clone();
        emb.query_prefix.clear();
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
                    let len = enc.get_ids().len().min(emb.max_seq);
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
        emb.query_prefix = saved_prefix;
        let padded_len = (longest.max(1).div_ceil(SEQ_STRIDE) * SEQ_STRIDE).min(emb.max_seq);
        let kept_count = keep.iter().filter(|k| **k).count();
        if kept_count == 0 {
            return Ok(vec![None; texts.len()]);
        }
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
        if shape.len() == 2 {
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
                result[*orig] = Some(mean_pool(hidden_row, mask_row).to_vec());
            }
        } else {
            return Err(ort::Error::new("unexpected batch embedding shape"));
        }
        Ok(result)
    });
    result.unwrap_or_else(|_| vec![None; texts.len()])
}

fn try_classifier_session() -> ort::Result<ClassifierSession> {
    let path = model_dir().join("image-classifier.onnx");
    if !path.is_file() {
        return Err(ort::Error::new("image-classifier.onnx missing"));
    }
    let info = local_ai::active_model_info();
    let input_name = info
        .as_ref()
        .map(|i| i.classifier_input_name.clone())
        .unwrap_or_else(|| "data".into());
    let input_size = info
        .as_ref()
        .map(|i| i.classifier_input_size)
        .unwrap_or(224);
    let (session, _) = open_session_preferring_hardware(&path)?;
    Ok(ClassifierSession {
        session,
        input_name,
        input_size,
    })
}

fn classifier_session() -> ort::Result<std::sync::MutexGuard<'static, Option<ClassifierSession>>> {
    let mut g = CLASSIFIER
        .lock()
        .map_err(|e| ort::Error::new(e.to_string()))?;
    if g.is_none() {
        *g = Some(try_classifier_session()?);
    }
    Ok(g)
}

pub fn image_classifier_available() -> bool {
    model_dir().join("image-classifier.onnx").is_file()
}

fn classifier_logits(path: &Path) -> Option<Vec<f32>> {
    let img = image::open(path).ok()?.into_rgb8();
    let dyn_img = DynamicImage::ImageRgb8(img);
    let mut guard = classifier_session().ok()?;
    let clf = guard.as_mut()?;
    let size = clf.input_size;
    let resized = dyn_img.resize_exact(size, size, FilterType::Triangle);
    let resized_rgb = resized.into_rgb8();
    let bytes = resized_rgb.as_raw();
    let plane = (size * size) as usize;
    let mut input = Array4::<f32>::zeros((1, 3, size as usize, size as usize));
    let input_slice = input.as_slice_mut().expect("contiguous");
    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];
    let scale = [
        1.0 / (255.0 * STD[0]),
        1.0 / (255.0 * STD[1]),
        1.0 / (255.0 * STD[2]),
    ];
    let mean_scaled = [MEAN[0] / STD[0], MEAN[1] / STD[1], MEAN[2] / STD[2]];
    for px in 0..plane {
        let r = bytes[px * 3] as f32;
        let g = bytes[px * 3 + 1] as f32;
        let b = bytes[px * 3 + 2] as f32;
        input_slice[px] = r * scale[0] - mean_scaled[0];
        input_slice[plane + px] = g * scale[1] - mean_scaled[1];
        input_slice[2 * plane + px] = b * scale[2] - mean_scaled[2];
    }
    let names = [
        clf.input_name.clone(),
        "data".into(),
        "input".into(),
        "images:0".into(),
        "images".into(),
    ];
    for name in names {
        let Ok(input_ref) = TensorRef::from_array_view(&input) else {
            break;
        };
        if let Ok(out) = clf.session.run(ort::inputs![name.as_str() => input_ref]) {
            if let Some((_, tensor)) = out.iter().next() {
                if let Ok((_shape, data)) = tensor.try_extract_tensor::<f32>() {
                    return Some(data.to_vec());
                }
            }
        }
    }
    None
}

fn softmax_inplace(logits: &mut [f32]) {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut sum = 0.0f32;
    for v in logits.iter_mut() {
        *v = (*v - max).exp();
        sum += *v;
    }
    if sum > 0.0 {
        for v in logits.iter_mut() {
            *v /= sum;
        }
    }
}

pub fn image_search_label_text(path: &Path) -> Option<String> {
    let mut flat = classifier_logits(path)?;
    if flat.is_empty() {
        return None;
    }
    softmax_inplace(&mut flat);
    let mut ranked: Vec<(usize, f32)> = flat.iter().copied().enumerate().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut tags: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for (idx, conf) in ranked.iter().take(32) {
        if *conf < CLASSIFY_MIN_CONFIDENCE {
            break;
        }
        let label = if let Some(coarse) = imagenet_labels::coarse_category(*idx) {
            coarse.to_string()
        } else {
            imagenet_labels::imagenet_label(*idx).to_string()
        };
        if seen.insert(label.clone()) {
            tags.push(label);
        }
        if tags.len() >= 5 {
            break;
        }
    }
    if tags.is_empty() {
        return None;
    }
    Some(tags.join(", "))
}

pub fn suggest_image_tag(path: &Path) -> Option<String> {
    let mut flat = classifier_logits(path)?;
    softmax_inplace(&mut flat);
    let (idx, conf) = flat
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, c)| (i, *c))?;
    if conf < CLASSIFY_MIN_CONFIDENCE {
        return None;
    }
    if let Some(coarse) = imagenet_labels::coarse_category(idx) {
        return Some(coarse.to_string());
    }
    Some(imagenet_labels::imagenet_label(idx).to_string())
}

/// 8x8 difference hash as u64 for near-duplicate image detection.
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

pub fn is_near_duplicate_dhash(a: u64, b: u64) -> bool {
    hamming64(a, b) <= DHASH_NEAR_DUP_THRESHOLD
}

/// Smoke-test that models load on the selected provider. Used for readiness.
pub fn smoke_test_ready() -> bool {
    if !local_ai::core_models_installed() {
        return false;
    }
    #[cfg(windows)]
    if !local_ai::onnx_runtime_installed() {
        return false;
    }
    with_text_embedder(|emb| {
        let _ = emb;
        Ok(())
    })
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hamming_near_dup_threshold() {
        assert!(is_near_duplicate_dhash(0, 0));
        assert!(is_near_duplicate_dhash(0, 0b11111)); // dist 5
        assert!(!is_near_duplicate_dhash(0, u64::MAX));
    }

    #[test]
    fn cosine_identical() {
        let v = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-5);
    }
}

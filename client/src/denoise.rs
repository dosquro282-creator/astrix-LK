use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_ATTEN_LIM_DB: f32 = 42.0;
const MIN_ATTEN_LIM_DB: f32 = 0.0;
const MAX_ATTEN_LIM_DB: f32 = 80.0;
const DEFAULT_POST_FILTER_BETA: f32 = 0.010;
const MIN_POST_FILTER_BETA: f32 = 0.0;
const MAX_POST_FILTER_BETA: f32 = 0.050;
const DEFAULT_MIN_DB_THRESH: f32 = -15.0;
const MIN_MIN_DB_THRESH: f32 = -40.0;
const MAX_MIN_DB_THRESH: f32 = 5.0;
const DEFAULT_MAX_DB_ERB_THRESH: f32 = 35.0;
const DEFAULT_MAX_DB_DF_THRESH: f32 = 35.0;
const MIN_MAX_DB_THRESH: f32 = 0.0;
const MAX_MAX_DB_THRESH: f32 = 60.0;
const DEFAULT_REDUCE_MASK_ID: &str = "max";
const CLIP_GUARD_HEADROOM: f32 = 0.98;
const CLIP_GUARD_RELEASE_STEP: f32 = 0.02;
const DEFAULT_LOG_LEVEL: &str = "warn";
const DEFAULT_MODEL_ID: &str = "DeepFilterNet3_onnx.tar.gz";
const NO_DENOISE_MODEL_ID: &str = "off";
const DEEPFILTERNET_DLL_ENV: &str = "ASTRIX_DF_DLL_PATH";
const DEEPFILTERNET_MODEL_ENV: &str = "ASTRIX_DF_MODEL_PATH";
const MIC_DENOISE_ENV: &str = "ASTRIX_MIC_DENOISE";

static DENOISE_UNAVAILABLE_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
pub struct KnownDenoiseModel {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct KnownReduceMask {
    pub id: &'static str,
    pub label: &'static str,
}

const KNOWN_MODELS: &[KnownDenoiseModel] = &[
    KnownDenoiseModel {
        id: NO_DENOISE_MODEL_ID,
        label: "Без шумодава",
    },
    KnownDenoiseModel {
        id: "DeepFilterNet3_ll_onnx.tar.gz",
        label: "DeepFilterNet3 ONNX Low Latency",
    },
    KnownDenoiseModel {
        id: "DeepFilterNet3_onnx.tar.gz",
        label: "DeepFilterNet3 ONNX",
    },
];

const KNOWN_REDUCE_MASKS: &[KnownReduceMask] = &[
    KnownReduceMask {
        id: "none",
        label: "None",
    },
    KnownReduceMask {
        id: "mean",
        label: "Mean",
    },
    KnownReduceMask {
        id: "max",
        label: "Max",
    },
];

#[derive(Clone, Debug, PartialEq)]
struct RuntimeDenoiseConfig {
    model_id: String,
    atten_lim_db: f32,
    post_filter_beta: f32,
    min_db_thresh: f32,
    max_db_erb_thresh: f32,
    max_db_df_thresh: f32,
    reduce_mask: ReduceMaskKind,
}

type SelectedDenoiseConfig = RuntimeDenoiseConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReduceMaskKind {
    None,
    Mean,
    Max,
}

impl ReduceMaskKind {
    fn from_id(mask_id: &str) -> Self {
        match mask_id {
            "none" => Self::None,
            "mean" => Self::Mean,
            _ => Self::Max,
        }
    }

    fn as_id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Mean => "mean",
            Self::Max => "max",
        }
    }

    fn as_c_int(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Max => 1,
            Self::Mean => 2,
        }
    }
}

struct SelectedDenoiseState {
    config: Mutex<SelectedDenoiseConfig>,
    generation: AtomicU64,
}

pub fn microphone_denoise_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_bool(MIC_DENOISE_ENV).unwrap_or(true))
}

pub fn known_models() -> &'static [KnownDenoiseModel] {
    KNOWN_MODELS
}

pub fn default_model_id() -> &'static str {
    DEFAULT_MODEL_ID
}

pub fn known_reduce_masks() -> &'static [KnownReduceMask] {
    KNOWN_REDUCE_MASKS
}

pub fn default_atten_lim_db() -> f32 {
    DEFAULT_ATTEN_LIM_DB
}

pub fn default_post_filter_beta() -> f32 {
    DEFAULT_POST_FILTER_BETA
}

pub fn default_min_db_thresh() -> f32 {
    DEFAULT_MIN_DB_THRESH
}

pub fn default_max_db_erb_thresh() -> f32 {
    DEFAULT_MAX_DB_ERB_THRESH
}

pub fn default_max_db_df_thresh() -> f32 {
    DEFAULT_MAX_DB_DF_THRESH
}

pub fn default_reduce_mask_id() -> &'static str {
    DEFAULT_REDUCE_MASK_ID
}

pub fn denoise_models_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vendor")
        .join("deepfilternet")
        .join("models")
}

pub fn is_model_downloaded(model_id: &str) -> bool {
    if is_denoise_disabled_model(model_id) {
        return true;
    }
    denoise_models_dir().join(model_id).is_file()
}

pub fn is_known_model(model_id: &str) -> bool {
    KNOWN_MODELS.iter().any(|model| model.id == model_id)
}

pub fn is_known_reduce_mask(mask_id: &str) -> bool {
    KNOWN_REDUCE_MASKS.iter().any(|mask| mask.id == mask_id)
}

pub fn selected_model_id() -> String {
    selected_denoise_state()
        .config
        .lock()
        .expect("selected denoise mutex poisoned")
        .model_id
        .clone()
}

pub fn selected_model_generation() -> u64 {
    selected_denoise_state().generation.load(Ordering::Acquire)
}

pub fn set_selected_model(model_id: &str) {
    let resolved = normalize_model_id(model_id).to_string();
    update_selected_denoise_config(|config| {
        if config.model_id != resolved {
            config.model_id = resolved;
            true
        } else {
            false
        }
    });
}

pub fn set_denoise_atten_lim_db(atten_lim_db: f32) {
    let resolved = normalize_atten_lim_db(atten_lim_db);
    update_selected_denoise_config(|config| {
        if (config.atten_lim_db - resolved).abs() > f32::EPSILON {
            config.atten_lim_db = resolved;
            true
        } else {
            false
        }
    });
}

pub fn set_denoise_post_filter_beta(post_filter_beta: f32) {
    let resolved = normalize_post_filter_beta(post_filter_beta);
    update_selected_denoise_config(|config| {
        if (config.post_filter_beta - resolved).abs() > f32::EPSILON {
            config.post_filter_beta = resolved;
            true
        } else {
            false
        }
    });
}

pub fn set_denoise_thresholds(min_db_thresh: f32, max_db_erb_thresh: f32, max_db_df_thresh: f32) {
    let resolved_min = normalize_min_db_thresh(min_db_thresh);
    let resolved_erb = normalize_max_db_erb_thresh(max_db_erb_thresh);
    let resolved_df = normalize_max_db_df_thresh(max_db_df_thresh);
    update_selected_denoise_config(|config| {
        let changed = (config.min_db_thresh - resolved_min).abs() > f32::EPSILON
            || (config.max_db_erb_thresh - resolved_erb).abs() > f32::EPSILON
            || (config.max_db_df_thresh - resolved_df).abs() > f32::EPSILON;
        if changed {
            config.min_db_thresh = resolved_min;
            config.max_db_erb_thresh = resolved_erb;
            config.max_db_df_thresh = resolved_df;
        }
        changed
    });
}

pub fn set_denoise_reduce_mask(mask_id: &str) {
    let resolved = ReduceMaskKind::from_id(normalize_reduce_mask_id(mask_id));
    update_selected_denoise_config(|config| {
        if config.reduce_mask != resolved {
            config.reduce_mask = resolved;
            true
        } else {
            false
        }
    });
}

pub fn model_label(model_id: &str) -> &'static str {
    KNOWN_MODELS
        .iter()
        .find(|model| model.id == model_id)
        .map(|model| model.label)
        .unwrap_or("DeepFilterNet model")
}

pub fn reduce_mask_label(mask_id: &str) -> &'static str {
    KNOWN_REDUCE_MASKS
        .iter()
        .find(|mask| mask.id == mask_id)
        .map(|mask| mask.label)
        .unwrap_or("Max")
}

pub fn normalize_atten_lim_db(atten_lim_db: f32) -> f32 {
    if atten_lim_db.is_finite() {
        atten_lim_db.clamp(MIN_ATTEN_LIM_DB, MAX_ATTEN_LIM_DB)
    } else {
        DEFAULT_ATTEN_LIM_DB
    }
}

pub fn normalize_post_filter_beta(post_filter_beta: f32) -> f32 {
    if post_filter_beta.is_finite() {
        post_filter_beta.clamp(MIN_POST_FILTER_BETA, MAX_POST_FILTER_BETA)
    } else {
        DEFAULT_POST_FILTER_BETA
    }
}

pub fn normalize_min_db_thresh(min_db_thresh: f32) -> f32 {
    if min_db_thresh.is_finite() {
        min_db_thresh.clamp(MIN_MIN_DB_THRESH, MAX_MIN_DB_THRESH)
    } else {
        DEFAULT_MIN_DB_THRESH
    }
}

pub fn normalize_max_db_erb_thresh(max_db_erb_thresh: f32) -> f32 {
    if max_db_erb_thresh.is_finite() {
        max_db_erb_thresh.clamp(MIN_MAX_DB_THRESH, MAX_MAX_DB_THRESH)
    } else {
        DEFAULT_MAX_DB_ERB_THRESH
    }
}

pub fn normalize_max_db_df_thresh(max_db_df_thresh: f32) -> f32 {
    if max_db_df_thresh.is_finite() {
        max_db_df_thresh.clamp(MIN_MAX_DB_THRESH, MAX_MAX_DB_THRESH)
    } else {
        DEFAULT_MAX_DB_DF_THRESH
    }
}

fn normalize_model_id(model_id: &str) -> &str {
    if is_known_model(model_id) {
        model_id
    } else {
        DEFAULT_MODEL_ID
    }
}

fn normalize_reduce_mask_id(mask_id: &str) -> &str {
    if is_known_reduce_mask(mask_id) {
        mask_id
    } else {
        DEFAULT_REDUCE_MASK_ID
    }
}

fn is_denoise_disabled_model(model_id: &str) -> bool {
    model_id == NO_DENOISE_MODEL_ID
}

fn current_runtime_config() -> RuntimeDenoiseConfig {
    selected_denoise_state()
        .config
        .lock()
        .expect("selected denoise mutex poisoned")
        .clone()
}

fn update_selected_denoise_config(update: impl FnOnce(&mut SelectedDenoiseConfig) -> bool) {
    let state = selected_denoise_state();
    let mut guard = state
        .config
        .lock()
        .expect("selected denoise mutex poisoned");
    if update(&mut guard) {
        state.generation.fetch_add(1, Ordering::AcqRel);
    }
}

fn selected_denoise_state() -> &'static SelectedDenoiseState {
    static STATE: OnceLock<SelectedDenoiseState> = OnceLock::new();
    STATE.get_or_init(|| SelectedDenoiseState {
        config: Mutex::new(SelectedDenoiseConfig {
            model_id: DEFAULT_MODEL_ID.to_string(),
            atten_lim_db: DEFAULT_ATTEN_LIM_DB,
            post_filter_beta: DEFAULT_POST_FILTER_BETA,
            min_db_thresh: DEFAULT_MIN_DB_THRESH,
            max_db_erb_thresh: DEFAULT_MAX_DB_ERB_THRESH,
            max_db_df_thresh: DEFAULT_MAX_DB_DF_THRESH,
            reduce_mask: ReduceMaskKind::from_id(DEFAULT_REDUCE_MASK_ID),
        }),
        generation: AtomicU64::new(0),
    })
}

pub struct AudioDenoiser {
    label: String,
    config_generation: u64,
    runtime_config: RuntimeDenoiseConfig,
    backend: DenoiserBackend,
}

enum DenoiserBackend {
    DeepFilterNet(DeepFilterNetAdapter),
    Disabled,
}

impl AudioDenoiser {
    pub fn new_sender_microphone() -> Self {
        Self::new("sender-microphone".to_string())
    }

    pub fn new_receiver_voice(user_id: i64) -> Self {
        Self::new(format!("receiver-user-{user_id}"))
    }

    fn new(label: String) -> Self {
        let runtime_config = current_runtime_config();
        let mut denoiser = Self {
            label,
            config_generation: selected_model_generation(),
            runtime_config,
            backend: DenoiserBackend::Disabled,
        };
        denoiser.rebuild_backend();
        denoiser
    }

    pub fn process_i16(&mut self, samples: &mut [i16]) {
        self.ensure_config_is_current();
        let err = match &mut self.backend {
            DenoiserBackend::DeepFilterNet(adapter) => adapter.process_i16(samples).err(),
            DenoiserBackend::Disabled => None,
        };
        if let Some(err) = err {
            eprintln!(
                "[denoise] disabling DeepFilterNet for {} after processing error: {}",
                self.label, err
            );
            self.backend = DenoiserBackend::Disabled;
        }
    }

    fn ensure_config_is_current(&mut self) {
        let generation = selected_model_generation();
        if generation == self.config_generation {
            return;
        }
        self.config_generation = generation;
        let next_config = current_runtime_config();
        let requires_rebuild = next_config.model_id != self.runtime_config.model_id
            || next_config.reduce_mask != self.runtime_config.reduce_mask;
        let tuning_changed = (next_config.atten_lim_db - self.runtime_config.atten_lim_db).abs()
            > f32::EPSILON
            || (next_config.post_filter_beta - self.runtime_config.post_filter_beta).abs()
                > f32::EPSILON
            || (next_config.min_db_thresh - self.runtime_config.min_db_thresh).abs() > f32::EPSILON
            || (next_config.max_db_erb_thresh - self.runtime_config.max_db_erb_thresh).abs()
                > f32::EPSILON
            || (next_config.max_db_df_thresh - self.runtime_config.max_db_df_thresh).abs()
                > f32::EPSILON;
        self.runtime_config = next_config;

        if requires_rebuild {
            self.rebuild_backend();
            return;
        }

        if tuning_changed {
            match &mut self.backend {
                DenoiserBackend::DeepFilterNet(adapter) => {
                    if let Err(err) = adapter.apply_runtime_config(&self.runtime_config) {
                        eprintln!(
                            "[denoise] disabling DeepFilterNet for {} after config update error: {}",
                            self.label, err
                        );
                        self.backend = DenoiserBackend::Disabled;
                    }
                }
                DenoiserBackend::Disabled => {
                    if !is_denoise_disabled_model(&self.runtime_config.model_id) {
                        self.rebuild_backend();
                    }
                }
            }
        }
    }

    fn rebuild_backend(&mut self) {
        if is_denoise_disabled_model(&self.runtime_config.model_id) {
            self.backend = DenoiserBackend::Disabled;
            return;
        }

        match DeepFilterNetAdapter::new(&self.label, &self.runtime_config) {
            Ok(adapter) => {
                self.backend = DenoiserBackend::DeepFilterNet(adapter);
            }
            Err(err) => {
                if !DENOISE_UNAVAILABLE_LOGGED.swap(true, Ordering::Relaxed) {
                    eprintln!("[denoise] DeepFilterNet unavailable, using bypass: {err}");
                }
                self.backend = DenoiserBackend::Disabled;
            }
        }
    }
}

struct DeepFilterNetAdapter {
    state: DeepFilterNetState,
    frame_len: usize,
    input_ring: VecDeque<f32>,
    output_ring: VecDeque<i16>,
    scratch_in: Vec<f32>,
    scratch_out: Vec<f32>,
    output_gain: f32,
}

impl DeepFilterNetAdapter {
    fn new(label: &str, runtime_config: &RuntimeDenoiseConfig) -> Result<Self, String> {
        let state = DeepFilterNetState::new(runtime_config)?;
        let frame_len = state.frame_len();
        if frame_len == 0 {
            return Err("DeepFilterNet reported frame length 0".to_string());
        }
        eprintln!(
            "[denoise] DeepFilterNet active for {} using {} (frame_len={} @ {} Hz, atten_lim={:.1} dB, post_filter_beta={:.3}, thresholds=[{:.1}, {:.1}, {:.1}], reduce_mask={})",
            label,
            runtime_config.model_id,
            frame_len,
            DEFAULT_SAMPLE_RATE,
            runtime_config.atten_lim_db,
            runtime_config.post_filter_beta,
            runtime_config.min_db_thresh,
            runtime_config.max_db_erb_thresh,
            runtime_config.max_db_df_thresh,
            runtime_config.reduce_mask.as_id(),
        );
        Ok(Self {
            state,
            frame_len,
            input_ring: VecDeque::with_capacity(frame_len * 4),
            output_ring: VecDeque::with_capacity(frame_len * 4),
            scratch_in: Vec::with_capacity(frame_len),
            scratch_out: vec![0.0; frame_len],
            output_gain: 1.0,
        })
    }

    fn apply_runtime_config(
        &mut self,
        runtime_config: &RuntimeDenoiseConfig,
    ) -> Result<(), String> {
        self.state.set_runtime_config(runtime_config)
    }

    fn process_i16(&mut self, samples: &mut [i16]) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }

        let original = samples.to_vec();
        self.input_ring
            .extend(samples.iter().map(|&sample| sample as f32 / 32768.0));

        while self.input_ring.len() >= self.frame_len {
            self.scratch_in.clear();
            self.scratch_in
                .extend(self.input_ring.drain(..self.frame_len));
            if self.scratch_out.len() != self.frame_len {
                self.scratch_out.resize(self.frame_len, 0.0);
            }
            self.state
                .process_frame(&mut self.scratch_in, &mut self.scratch_out)?;
            let peak = self
                .scratch_out
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            let target_gain = if peak > CLIP_GUARD_HEADROOM {
                CLIP_GUARD_HEADROOM / peak
            } else {
                1.0
            };
            if target_gain < self.output_gain {
                self.output_gain = target_gain;
            } else {
                self.output_gain = (self.output_gain + CLIP_GUARD_RELEASE_STEP).min(target_gain);
            }
            self.output_ring.extend(
                self.scratch_out
                    .iter()
                    .map(|sample| f32_to_i16(*sample * self.output_gain)),
            );
        }

        for (idx, sample) in samples.iter_mut().enumerate() {
            if let Some(processed) = self.output_ring.pop_front() {
                *sample = processed;
            } else {
                *sample = original[idx];
            }
        }

        Ok(())
    }
}

fn f32_to_i16(sample: f32) -> i16 {
    (sample * 32768.0).clamp(-32768.0, 32767.0) as i16
}

fn env_bool(key: &str) -> Option<bool> {
    let value = std::env::var(key).ok()?;
    if value == "1" || value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value == "0" || value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

fn env_f32(key: &str, default: f32) -> f32 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(default)
}

#[derive(Clone)]
struct DeepFilterNetPaths {
    dll_path: PathBuf,
    model_path: PathBuf,
}

fn deepfilternet_paths() -> Result<DeepFilterNetPaths, String> {
    resolve_deepfilternet_paths(&selected_model_id())
}

fn resolve_deepfilternet_paths(model_id: &str) -> Result<DeepFilterNetPaths, String> {
    let vendor_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vendor")
        .join("deepfilternet");

    let dll_path = deepfilternet_dll_path(&vendor_dir)?;
    let model_path = deepfilternet_model_path(&vendor_dir, model_id)?;

    Ok(DeepFilterNetPaths {
        dll_path,
        model_path,
    })
}

fn deepfilternet_dll_path(vendor_dir: &Path) -> Result<PathBuf, String> {
    static DLL_PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    match DLL_PATH.get_or_init(|| {
        if let Ok(path) = std::env::var(DEEPFILTERNET_DLL_ENV) {
            let path = PathBuf::from(path);
            if path.is_file() {
                Ok(path)
            } else {
                Err(format!(
                    "{} points to a missing DLL: {}",
                    DEEPFILTERNET_DLL_ENV,
                    path.display()
                ))
            }
        } else {
            find_dll_in_vendor(vendor_dir).ok_or_else(|| {
                format!(
                    "DeepFilterNet DLL not found. Set {} or place libdf.dll/df.dll in {}",
                    DEEPFILTERNET_DLL_ENV,
                    vendor_dir.display()
                )
            })
        }
    }) {
        Ok(path) => Ok(path.clone()),
        Err(err) => Err(err.clone()),
    }
}

fn deepfilternet_model_path(vendor_dir: &Path, model_id: &str) -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var(DEEPFILTERNET_MODEL_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "{} points to a missing model: {}",
            DEEPFILTERNET_MODEL_ENV,
            path.display()
        ));
    }

    let models_dir = vendor_dir.join("models");
    let model_path = models_dir.join(model_id);
    if model_path.is_file() {
        return Ok(model_path);
    }

    find_model_in_vendor(vendor_dir).ok_or_else(|| {
        format!(
            "DeepFilterNet model '{}' not found in {}. Download models or set {}.",
            model_id,
            models_dir.display(),
            DEEPFILTERNET_MODEL_ENV
        )
    })
}

fn find_dll_in_vendor(vendor_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        vendor_dir.join("libdf.dll"),
        vendor_dir.join("df.dll"),
        vendor_dir.join("deepfilternet.dll"),
        vendor_dir.join("DeepFilterNet.dll"),
        vendor_dir.join("libdf").join("libdf.dll"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

fn find_model_in_vendor(vendor_dir: &Path) -> Option<PathBuf> {
    for base_dir in [vendor_dir.join("models"), vendor_dir.to_path_buf()] {
        for model in KNOWN_MODELS {
            let path = base_dir.join(model.id);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    for dir in [vendor_dir.to_path_buf(), vendor_dir.join("models")] {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|value| value.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let is_model = KNOWN_MODELS
                .iter()
                .any(|model| name == model.id.to_ascii_lowercase());
            if path.is_file() && is_model {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    pub type DfCreateFn = unsafe extern "C" fn(*const c_char, f32, *const c_char) -> *mut c_void;
    pub type DfCreateExtFn = unsafe extern "C" fn(
        *const c_char,
        f32,
        f32,
        f32,
        f32,
        f32,
        i32,
        *const c_char,
    ) -> *mut c_void;
    pub type DfGetFrameLengthFn = unsafe extern "C" fn(*mut c_void) -> usize;
    pub type DfSetAttenLimFn = unsafe extern "C" fn(*mut c_void, f32);
    pub type DfSetPostFilterBetaFn = unsafe extern "C" fn(*mut c_void, f32);
    pub type DfSetThresholdsFn = unsafe extern "C" fn(*mut c_void, f32, f32, f32);
    pub type DfProcessFrameFn = unsafe extern "C" fn(*mut c_void, *mut f32, *mut f32) -> f32;
    pub type DfFreeFn = unsafe extern "C" fn(*mut c_void);

    pub struct DeepFilterNetApi {
        _module: HMODULE,
        pub df_create: DfCreateFn,
        pub df_create_ext: Option<DfCreateExtFn>,
        pub df_get_frame_length: DfGetFrameLengthFn,
        pub df_set_atten_lim: DfSetAttenLimFn,
        pub df_set_post_filter_beta: DfSetPostFilterBetaFn,
        pub df_set_thresholds: Option<DfSetThresholdsFn>,
        pub df_process_frame: DfProcessFrameFn,
        pub df_free: DfFreeFn,
    }

    unsafe impl Send for DeepFilterNetApi {}
    unsafe impl Sync for DeepFilterNetApi {}

    impl DeepFilterNetApi {
        fn load() -> Result<Arc<Self>, String> {
            let paths = deepfilternet_paths()?;
            let wide_path: Vec<u16> = paths
                .dll_path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let module = unsafe { LoadLibraryW(PCWSTR(wide_path.as_ptr())) }.map_err(|err| {
                format!(
                    "LoadLibraryW failed for {}: {err:?}",
                    paths.dll_path.display()
                )
            })?;

            Ok(Arc::new(Self {
                _module: module,
                df_create: load_symbol(module, b"df_create\0")?,
                df_create_ext: load_optional_symbol(module, b"df_create_ext\0"),
                df_get_frame_length: load_symbol(module, b"df_get_frame_length\0")?,
                df_set_atten_lim: load_symbol(module, b"df_set_atten_lim\0")?,
                df_set_post_filter_beta: load_symbol(module, b"df_set_post_filter_beta\0")?,
                df_set_thresholds: load_optional_symbol(module, b"df_set_thresholds\0"),
                df_process_frame: load_symbol(module, b"df_process_frame\0")?,
                df_free: load_symbol(module, b"df_free\0")?,
            }))
        }
    }

    fn load_symbol<T>(module: HMODULE, name: &[u8]) -> Result<T, String>
    where
        T: Copy,
    {
        let symbol = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }
            .ok_or_else(|| format!("GetProcAddress failed for {}", symbol_name(name)))?;
        let ptr = symbol as *const ();
        Ok(unsafe { std::mem::transmute_copy::<*const (), T>(&ptr) })
    }

    fn load_optional_symbol<T>(module: HMODULE, name: &[u8]) -> Option<T>
    where
        T: Copy,
    {
        let symbol = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }?;
        let ptr = symbol as *const ();
        Some(unsafe { std::mem::transmute_copy::<*const (), T>(&ptr) })
    }

    fn symbol_name(name: &[u8]) -> String {
        let end = name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(name.len());
        String::from_utf8_lossy(&name[..end]).into_owned()
    }

    pub fn deepfilternet_api() -> Result<Arc<DeepFilterNetApi>, String> {
        static API: OnceLock<Result<Arc<DeepFilterNetApi>, String>> = OnceLock::new();
        match API.get_or_init(DeepFilterNetApi::load) {
            Ok(api) => Ok(Arc::clone(api)),
            Err(err) => Err(err.clone()),
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::*;

    pub struct DeepFilterNetApi;

    pub fn deepfilternet_api() -> Result<Arc<DeepFilterNetApi>, String> {
        Err("DeepFilterNet runtime loading is only implemented on Windows".to_string())
    }
}

struct DeepFilterNetState {
    api: Arc<platform::DeepFilterNetApi>,
    ptr: *mut c_void,
}

impl DeepFilterNetState {
    fn new(runtime_config: &RuntimeDenoiseConfig) -> Result<Self, String> {
        let api = platform::deepfilternet_api()?;
        let paths = resolve_deepfilternet_paths(&runtime_config.model_id)?;
        let model_path =
            CString::new(paths.model_path.to_string_lossy().as_bytes()).map_err(|_| {
                format!(
                    "model path contains interior NUL: {}",
                    paths.model_path.display()
                )
            })?;
        let log_level =
            CString::new(DEFAULT_LOG_LEVEL).map_err(|_| "invalid log level".to_string())?;
        let atten_lim = env_f32("ASTRIX_DF_ATTEN_LIM_DB", runtime_config.atten_lim_db);
        let ptr = if let Some(df_create_ext) = api.df_create_ext {
            unsafe {
                df_create_ext(
                    model_path.as_ptr(),
                    atten_lim,
                    runtime_config.post_filter_beta,
                    runtime_config.min_db_thresh,
                    runtime_config.max_db_erb_thresh,
                    runtime_config.max_db_df_thresh,
                    runtime_config.reduce_mask.as_c_int(),
                    log_level.as_ptr(),
                )
            }
        } else {
            unsafe { (api.df_create)(model_path.as_ptr(), atten_lim, log_level.as_ptr()) }
        };
        if ptr.is_null() {
            return Err(format!(
                "df_create returned null for model {}",
                paths.model_path.display()
            ));
        }

        let state = Self { api, ptr };
        state.set_runtime_config(runtime_config)?;
        Ok(state)
    }

    fn frame_len(&self) -> usize {
        #[cfg(target_os = "windows")]
        {
            unsafe { (self.api.df_get_frame_length)(self.ptr) }
        }
        #[cfg(not(target_os = "windows"))]
        {
            0
        }
    }

    fn process_frame(&mut self, input: &mut [f32], output: &mut [f32]) -> Result<(), String> {
        if input.len() != output.len() {
            return Err(format!(
                "DeepFilterNet frame buffer size mismatch: in={} out={}",
                input.len(),
                output.len()
            ));
        }
        #[cfg(target_os = "windows")]
        {
            let _ = unsafe {
                (self.api.df_process_frame)(self.ptr, input.as_mut_ptr(), output.as_mut_ptr())
            };
            Ok(())
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = input;
            let _ = output;
            Err("DeepFilterNet runtime loading is only implemented on Windows".to_string())
        }
    }

    fn set_runtime_config(&self, runtime_config: &RuntimeDenoiseConfig) -> Result<(), String> {
        #[cfg(target_os = "windows")]
        {
            let atten_lim_db = env_f32("ASTRIX_DF_ATTEN_LIM_DB", runtime_config.atten_lim_db);
            let post_filter_beta = env_f32(
                "ASTRIX_DF_POST_FILTER_BETA",
                runtime_config.post_filter_beta,
            );
            unsafe {
                (self.api.df_set_atten_lim)(self.ptr, atten_lim_db);
                (self.api.df_set_post_filter_beta)(self.ptr, post_filter_beta);
                if let Some(df_set_thresholds) = self.api.df_set_thresholds {
                    df_set_thresholds(
                        self.ptr,
                        runtime_config.min_db_thresh,
                        runtime_config.max_db_erb_thresh,
                        runtime_config.max_db_df_thresh,
                    );
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = runtime_config;
            Err("DeepFilterNet runtime loading is only implemented on Windows".to_string())
        }
    }
}

impl Drop for DeepFilterNetState {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        unsafe {
            (self.api.df_free)(self.ptr);
        }
    }
}

unsafe impl Send for DeepFilterNetState {}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn deepfilternet_smoke_test_with_bundled_runtime() {
        let paths = deepfilternet_paths().expect("DeepFilterNet artifacts should resolve");
        assert!(
            paths.dll_path.is_file(),
            "missing DLL: {}",
            paths.dll_path.display()
        );
        assert!(
            paths.model_path.is_file(),
            "missing model: {}",
            paths.model_path.display()
        );

        let config = current_runtime_config();
        let mut state =
            DeepFilterNetState::new(&config).expect("DeepFilterNet state should initialize");
        let frame_len = state.frame_len();
        assert!(frame_len > 0, "frame length should be non-zero");

        let mut input = vec![0.0f32; frame_len];
        let mut output = vec![0.0f32; frame_len];
        state
            .process_frame(&mut input, &mut output)
            .expect("DeepFilterNet should process a silence frame");
        assert!(
            output.iter().all(|sample| sample.is_finite()),
            "processed frame contains non-finite samples"
        );
    }
}

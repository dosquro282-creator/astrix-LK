use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_ATTEN_LIM_DB: f32 = 12.0;
const DEFAULT_POST_FILTER_BETA: f32 = 0.0;
const DEFAULT_LOG_LEVEL: &str = "warn";
const DEFAULT_MODEL_ID: &str = "DeepFilterNet3_onnx.tar.gz";
const DEEPFILTERNET_DLL_ENV: &str = "ASTRIX_DF_DLL_PATH";
const DEEPFILTERNET_MODEL_ENV: &str = "ASTRIX_DF_MODEL_PATH";
const MIC_DENOISE_ENV: &str = "ASTRIX_MIC_DENOISE";

static DENOISE_UNAVAILABLE_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
pub struct KnownDenoiseModel {
    pub id: &'static str,
    pub label: &'static str,
}

const KNOWN_MODELS: &[KnownDenoiseModel] = &[
    KnownDenoiseModel {
        id: "DeepFilterNet3_ll_onnx.tar.gz",
        label: "DeepFilterNet3 ONNX Low Latency",
    },
    KnownDenoiseModel {
        id: "DeepFilterNet3_onnx.tar.gz",
        label: "DeepFilterNet3 ONNX",
    },
];

struct SelectedModelState {
    id: Mutex<String>,
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

pub fn denoise_models_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vendor")
        .join("deepfilternet")
        .join("models")
}

pub fn is_model_downloaded(model_id: &str) -> bool {
    denoise_models_dir().join(model_id).is_file()
}

pub fn is_known_model(model_id: &str) -> bool {
    KNOWN_MODELS.iter().any(|model| model.id == model_id)
}

pub fn selected_model_id() -> String {
    selected_model_state()
        .id
        .lock()
        .expect("selected model mutex poisoned")
        .clone()
}

pub fn selected_model_generation() -> u64 {
    selected_model_state().generation.load(Ordering::Acquire)
}

pub fn set_selected_model(model_id: &str) {
    let resolved = normalize_model_id(model_id).to_string();
    let state = selected_model_state();
    let mut guard = state.id.lock().expect("selected model mutex poisoned");
    if *guard != resolved {
        *guard = resolved;
        state.generation.fetch_add(1, Ordering::AcqRel);
    }
}

pub fn model_label(model_id: &str) -> &'static str {
    KNOWN_MODELS
        .iter()
        .find(|model| model.id == model_id)
        .map(|model| model.label)
        .unwrap_or("DeepFilterNet model")
}

fn normalize_model_id(model_id: &str) -> &str {
    if is_known_model(model_id) {
        model_id
    } else {
        DEFAULT_MODEL_ID
    }
}

fn selected_model_state() -> &'static SelectedModelState {
    static STATE: OnceLock<SelectedModelState> = OnceLock::new();
    STATE.get_or_init(|| SelectedModelState {
        id: Mutex::new(DEFAULT_MODEL_ID.to_string()),
        generation: AtomicU64::new(0),
    })
}

pub struct AudioDenoiser {
    label: String,
    model_generation: u64,
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
        let mut denoiser = Self {
            label,
            model_generation: selected_model_generation(),
            backend: DenoiserBackend::Disabled,
        };
        denoiser.rebuild_backend();
        denoiser
    }

    pub fn process_i16(&mut self, samples: &mut [i16]) {
        self.ensure_model_is_current();
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

    fn ensure_model_is_current(&mut self) {
        let generation = selected_model_generation();
        if generation != self.model_generation {
            self.model_generation = generation;
            self.rebuild_backend();
        }
    }

    fn rebuild_backend(&mut self) {
        match DeepFilterNetAdapter::new(&self.label) {
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
}

impl DeepFilterNetAdapter {
    fn new(label: &str) -> Result<Self, String> {
        let state = DeepFilterNetState::new()?;
        let frame_len = state.frame_len();
        let model_id = selected_model_id();
        if frame_len == 0 {
            return Err("DeepFilterNet reported frame length 0".to_string());
        }
        eprintln!(
            "[denoise] DeepFilterNet active for {} using {} (frame_len={} @ {} Hz)",
            label, model_id, frame_len, DEFAULT_SAMPLE_RATE
        );
        Ok(Self {
            state,
            frame_len,
            input_ring: VecDeque::with_capacity(frame_len * 4),
            output_ring: VecDeque::with_capacity(frame_len * 4),
            scratch_in: Vec::with_capacity(frame_len),
            scratch_out: vec![0.0; frame_len],
        })
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
            self.scratch_in.extend(self.input_ring.drain(..self.frame_len));
            if self.scratch_out.len() != self.frame_len {
                self.scratch_out.resize(self.frame_len, 0.0);
            }
            self.state
                .process_frame(&mut self.scratch_in, &mut self.scratch_out)?;
            self.output_ring
                .extend(self.scratch_out.iter().copied().map(f32_to_i16));
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
    resolve_deepfilternet_paths()
}

fn resolve_deepfilternet_paths() -> Result<DeepFilterNetPaths, String> {
    let vendor_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vendor")
        .join("deepfilternet");

    let dll_path = deepfilternet_dll_path(&vendor_dir)?;
    let model_path = deepfilternet_model_path(&vendor_dir)?;

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

fn deepfilternet_model_path(vendor_dir: &Path) -> Result<PathBuf, String> {
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

    let model_id = selected_model_id();
    let models_dir = vendor_dir.join("models");
    let model_path = models_dir.join(&model_id);
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
    pub type DfGetFrameLengthFn = unsafe extern "C" fn(*mut c_void) -> usize;
    pub type DfSetPostFilterBetaFn = unsafe extern "C" fn(*mut c_void, f32);
    pub type DfProcessFrameFn = unsafe extern "C" fn(*mut c_void, *mut f32, *mut f32) -> f32;
    pub type DfFreeFn = unsafe extern "C" fn(*mut c_void);

    pub struct DeepFilterNetApi {
        _module: HMODULE,
        pub df_create: DfCreateFn,
        pub df_get_frame_length: DfGetFrameLengthFn,
        pub df_set_post_filter_beta: DfSetPostFilterBetaFn,
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
                df_get_frame_length: load_symbol(module, b"df_get_frame_length\0")?,
                df_set_post_filter_beta: load_symbol(module, b"df_set_post_filter_beta\0")?,
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

    fn symbol_name(name: &[u8]) -> String {
        let end = name.iter().position(|&byte| byte == 0).unwrap_or(name.len());
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
    fn new() -> Result<Self, String> {
        let api = platform::deepfilternet_api()?;
        let paths = deepfilternet_paths()?;
        let model_path =
            CString::new(paths.model_path.to_string_lossy().as_bytes()).map_err(|_| {
                format!(
                    "model path contains interior NUL: {}",
                    paths.model_path.display()
                )
            })?;
        let log_level =
            CString::new(DEFAULT_LOG_LEVEL).map_err(|_| "invalid log level".to_string())?;
        let atten_lim = env_f32("ASTRIX_DF_ATTEN_LIM_DB", DEFAULT_ATTEN_LIM_DB);
        let ptr = unsafe { (api.df_create)(model_path.as_ptr(), atten_lim, log_level.as_ptr()) };
        if ptr.is_null() {
            return Err(format!(
                "df_create returned null for model {}",
                paths.model_path.display()
            ));
        }

        let state = Self { api, ptr };
        let post_filter_beta = env_f32("ASTRIX_DF_POST_FILTER_BETA", DEFAULT_POST_FILTER_BETA);
        unsafe {
            (state.api.df_set_post_filter_beta)(state.ptr, post_filter_beta);
        }
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
        assert!(paths.dll_path.is_file(), "missing DLL: {}", paths.dll_path.display());
        assert!(
            paths.model_path.is_file(),
            "missing model: {}",
            paths.model_path.display()
        );

        let mut state = DeepFilterNetState::new().expect("DeepFilterNet state should initialize");
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

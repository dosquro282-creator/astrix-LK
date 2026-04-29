#include "astrix/nvenc_d11_bridge.h"

#include <d3d11.h>
#include <dxgi.h>
#include <windows.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <cstring>
#include <cstdio>
#include <cstdint>
#include <deque>
#include <limits>
#include <memory>
#include <mutex>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <unordered_map>
#include <vector>

#include "nvEncodeAPI.h"

namespace astrix_nvenc {

namespace {

[[noreturn]] void ThrowNvenc(const std::string& message,
                             NVENCSTATUS status = NV_ENC_ERR_GENERIC) {
  std::ostringstream oss;
  oss << message << " (status=" << static_cast<int>(status) << ")";
  throw std::runtime_error(oss.str());
}

void CheckNvenc(NVENCSTATUS status, const char* what) {
  if (status != NV_ENC_SUCCESS) {
    ThrowNvenc(what, status);
  }
}

std::string WideToUtf8(const wchar_t* wide) {
  if (!wide || !wide[0]) {
    return "NVIDIA";
  }
  const int bytes =
      WideCharToMultiByte(CP_UTF8, 0, wide, -1, nullptr, 0, nullptr, nullptr);
  if (bytes <= 1) {
    return "NVIDIA";
  }
  std::string out(static_cast<std::size_t>(bytes) - 1, '\0');
  WideCharToMultiByte(
      CP_UTF8, 0, wide, -1, out.data(), bytes, nullptr, nullptr);
  return out;
}

std::string AdapterNameFromDevice(ID3D11Device* device) {
  IDXGIDevice* dxgi_device = nullptr;
  IDXGIAdapter* adapter = nullptr;
  DXGI_ADAPTER_DESC desc = {};
  std::string name = "NVIDIA";

  if (device && SUCCEEDED(device->QueryInterface(
                    __uuidof(IDXGIDevice), reinterpret_cast<void**>(&dxgi_device))) &&
      dxgi_device && SUCCEEDED(dxgi_device->GetAdapter(&adapter)) && adapter &&
      SUCCEEDED(adapter->GetDesc(&desc))) {
    name = WideToUtf8(desc.Description);
  }

  if (adapter) {
    adapter->Release();
  }
  if (dxgi_device) {
    dxgi_device->Release();
  }
  return name;
}

bool IsEnvDisabled(const char* value) {
  return value != nullptr &&
         (std::strcmp(value, "0") == 0 ||
          std::strcmp(value, "false") == 0 ||
          std::strcmp(value, "FALSE") == 0 ||
          std::strcmp(value, "normal") == 0 ||
          std::strcmp(value, "NORMAL") == 0);
}

void ApplyNvencWorkerPriority() {
  const char* priority_env = std::getenv("ASTRIX_NVENC_WORKER_PRIORITY");
  if (IsEnvDisabled(priority_env)) {
    return;
  }

  int priority = THREAD_PRIORITY_HIGHEST;
  if (priority_env != nullptr &&
      (std::strcmp(priority_env, "highest") == 0 ||
       std::strcmp(priority_env, "HIGHEST") == 0 ||
       std::strcmp(priority_env, "2") == 0)) {
    priority = THREAD_PRIORITY_HIGHEST;
  }
  SetThreadPriority(GetCurrentThread(), priority);
}

bool GuidEquals(const GUID& a, const GUID& b) {
  return std::memcmp(&a, &b, sizeof(GUID)) == 0;
}

void LogH264LowLatencyConfig(const char* label,
                             const NV_ENC_INITIALIZE_PARAMS& init,
                             const NV_ENC_CONFIG& config) {
  const auto& rc = config.rcParams;
  const auto& h264 = config.encodeCodecConfig.h264Config;
  std::fprintf(
      stderr,
      "[nvenc_d11] config: label=%s tuning=%u preset_p1=%u preset_p4=%u rc=%u multipass=%u "
      "lookahead=%u depth=%u aq=%u aq_strength=%u temporal_aq=%u zero_reorder=%u "
      "non_ref_p=%u strict_gop=%u frame_interval_p=%d gop=%u idr=%u vbv=%u/%u "
      "h264_fast profile_baseline=%u entropy=%u adaptive_transform=%u deblock=%u "
      "b_ref=%u bdirect=%u hier_p=%u hier_b=%u temporal_svc=%u intra_refresh=%u "
      "ltr=%u refs_l0=%u refs_l1=%u max_ref=%u async=%u\n",
      label,
      static_cast<unsigned>(init.tuningInfo),
      GuidEquals(init.presetGUID, NV_ENC_PRESET_P1_GUID) ? 1u : 0u,
      GuidEquals(init.presetGUID, NV_ENC_PRESET_P4_GUID) ? 1u : 0u,
      static_cast<unsigned>(rc.rateControlMode),
      static_cast<unsigned>(rc.multiPass),
      rc.enableLookahead,
      static_cast<unsigned>(rc.lookaheadDepth),
      rc.enableAQ,
      rc.aqStrength,
      rc.enableTemporalAQ,
      rc.zeroReorderDelay,
      rc.enableNonRefP,
      rc.strictGOPTarget,
      config.frameIntervalP,
      config.gopLength,
      h264.idrPeriod,
      rc.vbvBufferSize,
      rc.vbvInitialDelay,
      GuidEquals(config.profileGUID, NV_ENC_H264_PROFILE_BASELINE_GUID) ? 1u : 0u,
      static_cast<unsigned>(h264.entropyCodingMode),
      static_cast<unsigned>(h264.adaptiveTransformMode),
      h264.disableDeblockingFilterIDC,
      static_cast<unsigned>(h264.useBFramesAsRef),
      static_cast<unsigned>(h264.bdirectMode),
      h264.hierarchicalPFrames,
      h264.hierarchicalBFrames,
      h264.enableTemporalSVC,
      h264.enableIntraRefresh,
      h264.enableLTR,
      static_cast<unsigned>(h264.numRefL0),
      static_cast<unsigned>(h264.numRefL1),
      h264.maxNumRefFrames,
      init.enableEncodeAsync);
}

uint32_t MaxSupportedVersion(HMODULE module) {
  using GetMaxVersionFn = NVENCSTATUS(NVENCAPI*)(uint32_t*);
  auto* get_max_version = reinterpret_cast<GetMaxVersionFn>(
      GetProcAddress(module, "NvEncodeAPIGetMaxSupportedVersion"));
  if (!get_max_version) {
    ThrowNvenc("NvEncodeAPIGetMaxSupportedVersion not found",
               NV_ENC_ERR_NO_ENCODE_DEVICE);
  }
  uint32_t version = 0;
  CheckNvenc(get_max_version(&version), "NvEncodeAPIGetMaxSupportedVersion failed");
  return version;
}

NV_ENCODE_API_FUNCTION_LIST CreateApi(HMODULE module) {
  using CreateInstanceFn = NVENCSTATUS(NVENCAPI*)(NV_ENCODE_API_FUNCTION_LIST*);
  auto* create_instance = reinterpret_cast<CreateInstanceFn>(
      GetProcAddress(module, "NvEncodeAPICreateInstance"));
  if (!create_instance) {
    ThrowNvenc("NvEncodeAPICreateInstance not found",
               NV_ENC_ERR_NO_ENCODE_DEVICE);
  }

  NV_ENCODE_API_FUNCTION_LIST api = {NV_ENCODE_API_FUNCTION_LIST_VER};
  CheckNvenc(create_instance(&api), "NvEncodeAPICreateInstance failed");
  return api;
}

uint32_t ComputeVbvBufferBits(uint32_t bitrate, uint32_t fps) {
  const uint32_t safe_fps = fps > 0 ? fps : 1u;
  const uint64_t frame_bits =
      static_cast<uint64_t>(bitrate) / safe_fps > 0
          ? static_cast<uint64_t>(bitrate) / safe_fps
          : 1u;
  // Use a larger frame window to smooth bitrate bursts before VBV penalizes quality.
  // At high FPS (>=90): 16 frames gives ~177ms of buffer headroom.
  // At 60+ FPS: 12 frames gives ~200ms of buffer headroom.
  // At <60 FPS: 8 frames gives ~133ms of buffer headroom.
  const uint64_t frame_window_bits =
      frame_bits * (safe_fps >= 90 ? 16ull : safe_fps >= 60 ? 12ull : 8ull);
  const uint64_t duration_window_bits =
      static_cast<uint64_t>(bitrate) / 4u > 0
          ? static_cast<uint64_t>(bitrate) / 4u
          : 1u;
  const uint64_t vbv_bits =
      frame_window_bits > duration_window_bits ? frame_window_bits
                                               : duration_window_bits;
  const uint64_t max_u32 = static_cast<uint64_t>(UINT32_MAX);
  return static_cast<uint32_t>(
      vbv_bits > max_u32 ? max_u32 : vbv_bits);
}

struct DetectedInputFormat {
  NV_ENC_BUFFER_FORMAT nvenc_format;
  DXGI_FORMAT dxgi_format;
  const char* label;
};

DetectedInputFormat DetectInputFormat(
    const rust::Vec<std::uintptr_t>& texture_ptrs) {
  if (texture_ptrs.empty()) {
    throw std::runtime_error("NVENC D3D11 requires at least one input texture");
  }
  auto* texture = reinterpret_cast<ID3D11Texture2D*>(texture_ptrs[0]);
  if (!texture) {
    throw std::runtime_error("NVENC D3D11 texture pointer is null");
  }

  D3D11_TEXTURE2D_DESC desc = {};
  texture->GetDesc(&desc);
  if (desc.Format == DXGI_FORMAT_NV12) {
    return {NV_ENC_BUFFER_FORMAT_NV12, DXGI_FORMAT_NV12, "NV12"};
  }
  if (desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM) {
    return {NV_ENC_BUFFER_FORMAT_ARGB, DXGI_FORMAT_B8G8R8A8_UNORM,
            "ARGB/BGRA"};
  }
  if (desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM) {
    return {NV_ENC_BUFFER_FORMAT_ABGR, DXGI_FORMAT_R8G8B8A8_UNORM,
            "ABGR/RGBA"};
  }

  std::ostringstream oss;
  oss << "NVENC D3D11 unsupported input DXGI format: "
      << static_cast<int>(desc.Format);
  throw std::runtime_error(oss.str());
}

}  // namespace

struct PendingSubmission {
  uint32_t input_slot = 0;
  uint32_t output_slot = 0;
  bool force_idr = false;
  std::chrono::steady_clock::time_point enqueued_at;
  std::chrono::steady_clock::time_point submitted_at;
};

struct CompletedPacket {
  std::vector<std::uint8_t> data;
  std::uint64_t encode_time_us = 0;
};

struct OutputSlot {
  HANDLE completion_event = nullptr;
  NV_ENC_OUTPUT_PTR bitstream = nullptr;
  NV_ENC_INPUT_PTR mapped_input = nullptr;
  bool reserved = false;
};

struct NvencD3D11Session::Impl {
  Impl(std::uintptr_t d3d11_device,
       std::uint32_t width,
       std::uint32_t height,
       std::uint32_t fps,
       std::uint32_t bitrate,
       rust::Vec<std::uintptr_t> texture_ptrs,
       std::uint32_t gir_period_frames,
       std::uint32_t gir_duration_frames)
      : width_(width),
        height_(height),
        fps_(fps ? fps : 60),
        bitrate_(bitrate),
        ring_size_(static_cast<uint32_t>(texture_ptrs.size())),
        gir_period_frames_(gir_period_frames),
        gir_duration_frames_(gir_duration_frames) {
    if (ring_size_ == 0) {
      throw std::runtime_error("NVENC D3D11 requires at least one input texture");
    }

    device_ = reinterpret_cast<ID3D11Device*>(d3d11_device);
    if (!device_) {
      throw std::runtime_error("NVENC D3D11 device pointer is null");
    }
    device_->AddRef();
    encoder_name_ = AdapterNameFromDevice(device_);
    const auto detected_input_format = DetectInputFormat(texture_ptrs);
    input_buffer_format_ = detected_input_format.nvenc_format;
    input_dxgi_format_ = detected_input_format.dxgi_format;
    input_format_label_ = detected_input_format.label;

    module_ = LoadLibraryW(L"nvEncodeAPI64.dll");
    if (!module_) {
      throw std::runtime_error("nvEncodeAPI64.dll not found");
    }

    const uint32_t driver_version = MaxSupportedVersion(module_);
    const uint32_t api_version =
        (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
    if (api_version > driver_version) {
      throw std::runtime_error(
          "Current NVIDIA driver does not support the bundled NVENC API version");
    }

    api_ = CreateApi(module_);
    OpenSession();
    InitializeEncoder();
    CreateOutputRing();
    RegisterInputs(std::move(texture_ptrs));
    stop_event_ = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (!stop_event_) {
      throw std::runtime_error("CreateEventW failed for NVENC stop event");
    }
    submit_event_ = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!submit_event_) {
      throw std::runtime_error("CreateEventW failed for NVENC submit event");
    }
    StartOutputWorker();
  }

  ~Impl() { Cleanup(); }

  const std::string& encoder_name() const { return encoder_name_; }

  bool is_async() const { return async_encode_; }

  std::uint32_t ring_size() const { return ring_size_; }

  std::uint32_t in_flight_count() const {
    std::lock_guard<std::mutex> lock(queue_mutex_);
    ThrowWorkerErrorLocked();
    return static_cast<std::uint32_t>(submit_queue_.size() + in_flight_.size());
  }

  std::uint64_t last_encode_time_us() const {
    std::lock_guard<std::mutex> lock(queue_mutex_);
    return last_encode_time_us_;
  }

  std::uint64_t last_submit_map_us() const {
    return last_submit_map_us_.load(std::memory_order_relaxed);
  }

  std::uint64_t last_submit_encode_picture_us() const {
    return last_submit_encode_picture_us_.load(std::memory_order_relaxed);
  }

  std::uint64_t last_submit_total_us() const {
    return last_submit_total_us_.load(std::memory_order_relaxed);
  }

  void submit(std::uintptr_t texture_ptr, bool force_idr) {
    const auto submit_started_at = std::chrono::steady_clock::now();
    std::lock_guard<std::mutex> submit_lock(submit_mutex_);

    uint32_t input_slot = 0;
    uint32_t output_slot = 0;

    {
      std::lock_guard<std::mutex> lock(queue_mutex_);
      ThrowWorkerErrorLocked();
      if (submit_queue_.size() + in_flight_.size() >= outputs_.size()) {
        throw std::runtime_error("NVENC D3D11 queue is full");
      }

      const auto it = texture_to_slot_.find(texture_ptr);
      if (it == texture_to_slot_.end()) {
        throw std::runtime_error(
            "NVENC D3D11 received a texture outside the registered ring");
      }
      input_slot = it->second;
      bool found_free_slot = false;
      for (std::size_t i = 0; i < outputs_.size(); ++i) {
        const auto candidate =
            static_cast<uint32_t>((next_submit_index_ + i) % outputs_.size());
        auto& out = outputs_[candidate];
        if (!out.reserved && out.mapped_input == nullptr) {
          output_slot = candidate;
          found_free_slot = true;
          break;
        }
      }
      if (!found_free_slot) {
        throw std::runtime_error("NVENC D3D11 queue is full");
      }
      auto& out = outputs_[output_slot];
      out.reserved = true;
      submit_queue_.push_back(PendingSubmission{
          .input_slot = input_slot,
          .output_slot = output_slot,
          .force_idr = force_idr,
          .enqueued_at = std::chrono::steady_clock::now(),
      });
      next_submit_index_++;
    }
    last_submit_total_us_.store(
        static_cast<std::uint64_t>(
            std::chrono::duration_cast<std::chrono::microseconds>(
                std::chrono::steady_clock::now() - submit_started_at)
                .count()),
        std::memory_order_relaxed);
    if (submit_event_) {
      SetEvent(submit_event_);
    }
    pending_cv_.notify_one();
  }

  rust::Vec<std::uint8_t> collect(std::uint32_t timeout_ms) {
    CompletedPacket packet;
    {
      std::unique_lock<std::mutex> lock(queue_mutex_);
      ThrowWorkerErrorLocked();
      if (completed_.empty()) {
        if (timeout_ms == 0) {
          return rust::Vec<std::uint8_t>{};
        }
        completed_cv_.wait_for(
            lock,
            std::chrono::milliseconds(timeout_ms),
            [&] {
              return stop_requested_ || !completed_.empty() ||
                     !worker_error_.empty();
            });
      }

      if (completed_.empty()) {
        ThrowWorkerErrorLocked();
        return rust::Vec<std::uint8_t>{};
      }

      packet = std::move(completed_.front());
      completed_.pop_front();
      last_encode_time_us_ = packet.encode_time_us;
    }

    rust::Vec<std::uint8_t> result;
    result.reserve(packet.data.size());
    for (auto byte : packet.data) {
      result.push_back(byte);
    }
    return result;
  }

  void set_bitrate(std::uint32_t bitrate) {
    if (!encoder_ || bitrate == 0 || bitrate == bitrate_) {
      return;
    }
    {
      std::lock_guard<std::mutex> lock(queue_mutex_);
      ThrowWorkerErrorLocked();
    }

    NV_ENC_RECONFIGURE_PARAMS reconfig = {NV_ENC_RECONFIGURE_PARAMS_VER};
    NV_ENC_INITIALIZE_PARAMS init = init_params_;
    NV_ENC_CONFIG config = encode_config_;
    init.encodeConfig = &config;
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.maxBitRate = bitrate;
    const uint32_t vbv = ComputeVbvBufferBits(bitrate, fps_);
    config.rcParams.vbvBufferSize = vbv;
    // vbvInitialDelay at 50% allows temporary burst room before VBV constraints bite.
    config.rcParams.vbvInitialDelay = vbv / 2;
    reconfig.reInitEncodeParams = init;
    reconfig.reInitEncodeParams.encodeConfig = &config;
    // Bitrate-only reconfigurations should stay in-place; resetting the encoder
    // on every WebRTC BWE tick creates visible quality/FPS oscillation.
    reconfig.resetEncoder = 0;

    {
      std::lock_guard<std::mutex> api_lock(api_mutex_);
      CheckNvenc(api_.nvEncReconfigureEncoder(encoder_, &reconfig),
                 "nvEncReconfigureEncoder failed");
    }

    bitrate_ = bitrate;
    init_params_ = init;
    encode_config_ = config;
    init_params_.encodeConfig = &encode_config_;
  }

 private:
  void OpenSession() {
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS params = {
        NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER};
    params.device = device_;
    params.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    params.apiVersion = NVENCAPI_VERSION;
    CheckNvenc(api_.nvEncOpenEncodeSessionEx(&params, &encoder_),
               "nvEncOpenEncodeSessionEx failed");
  }

  int GetCapability(NV_ENC_CAPS cap) const {
    NV_ENC_CAPS_PARAM caps = {NV_ENC_CAPS_PARAM_VER};
    caps.capsToQuery = cap;
    int value = 0;
    CheckNvenc(api_.nvEncGetEncodeCaps(encoder_, NV_ENC_CODEC_H264_GUID, &caps, &value),
               "nvEncGetEncodeCaps failed");
    return value;
  }

  void InitializeEncoder() {
    struct InitAttempt {
      const char* label;
      bool enable_async;
      bool conservative;
      bool minimal_preset;
      bool null_config;
      GUID preset_guid;
    };

    const bool async_supported =
        GetCapability(NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT) != 0;
    const bool prefer_fast_preset = fps_ >= 90;
    const char* fast_h264_env = std::getenv("ASTRIX_DXGI_NVENC_FAST_H264_MODE");
    const bool prefer_fast_h264_mode =
        fast_h264_env == nullptr
            ? prefer_fast_preset
            : !(std::strcmp(fast_h264_env, "0") == 0 ||
                std::strcmp(fast_h264_env, "false") == 0 ||
                std::strcmp(fast_h264_env, "FALSE") == 0);
    std::vector<InitAttempt> attempts;
    attempts.reserve(prefer_fast_preset ? 8 : 5);
    if (prefer_fast_preset) {
      attempts.push_back(
          {"custom async p1", async_supported, false, false, false, NV_ENC_PRESET_P1_GUID});
      attempts.push_back(
          {"preset async p1", async_supported, true, false, false, NV_ENC_PRESET_P1_GUID});
      attempts.push_back(
          {"preset sync p1", false, true, false, false, NV_ENC_PRESET_P1_GUID});
      attempts.push_back(
          {"custom async p4 fallback", async_supported, false, false, false, NV_ENC_PRESET_P4_GUID});
      attempts.push_back(
          {"preset async p4 fallback", async_supported, true, false, false, NV_ENC_PRESET_P4_GUID});
      attempts.push_back(
          {"preset sync p4 fallback", false, true, false, false, NV_ENC_PRESET_P4_GUID});
    } else {
      attempts.push_back(
          {"custom async p4", async_supported, false, false, false, NV_ENC_PRESET_P4_GUID});
      attempts.push_back(
          {"preset async p4", async_supported, true, false, false, NV_ENC_PRESET_P4_GUID});
      attempts.push_back(
          {"preset sync p4", false, true, false, false, NV_ENC_PRESET_P4_GUID});
    }
    attempts.push_back(
        {"minimal sync p1", false, true, true, false, NV_ENC_PRESET_P1_GUID});
    attempts.push_back(
        {"null-config sync p1", false, true, true, true, NV_ENC_PRESET_P1_GUID});

    auto build_attempt = [&](const InitAttempt& attempt) {
      NV_ENC_PRESET_CONFIG preset = {NV_ENC_PRESET_CONFIG_VER,
                                     {NV_ENC_CONFIG_VER}};
      CheckNvenc(api_.nvEncGetEncodePresetConfigEx(
                     encoder_, NV_ENC_CODEC_H264_GUID, attempt.preset_guid,
                     NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &preset),
                 "nvEncGetEncodePresetConfigEx failed");

      encode_config_ = preset.presetCfg;
      encode_config_.version = NV_ENC_CONFIG_VER;

      if (!attempt.minimal_preset) {
        encode_config_.gopLength = NVENC_INFINITE_GOPLENGTH;
        encode_config_.frameIntervalP = 1;
        encode_config_.encodeCodecConfig.h264Config.idrPeriod =
            encode_config_.gopLength;
        encode_config_.rcParams.version = NV_ENC_RC_PARAMS_VER;
        encode_config_.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        encode_config_.rcParams.averageBitRate = bitrate_;
        encode_config_.rcParams.maxBitRate = bitrate_;
        encode_config_.rcParams.multiPass = NV_ENC_MULTI_PASS_DISABLED;
        encode_config_.rcParams.enableAQ = 0;
        encode_config_.rcParams.aqStrength = 0;
        encode_config_.rcParams.enableLookahead = 0;
        encode_config_.rcParams.lookaheadDepth = 0;
        encode_config_.rcParams.disableIadapt = 1;
        encode_config_.rcParams.disableBadapt = 1;
        encode_config_.rcParams.enableTemporalAQ = 0;
        encode_config_.rcParams.zeroReorderDelay = 1;
        encode_config_.rcParams.enableNonRefP = prefer_fast_preset ? 1 : 0;
        encode_config_.rcParams.strictGOPTarget = 1;
        encode_config_.frameFieldMode = NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
        encode_config_.encodeCodecConfig.h264Config.outputBufferingPeriodSEI = 0;
        encode_config_.encodeCodecConfig.h264Config.outputPictureTimingSEI = 0;
        encode_config_.encodeCodecConfig.h264Config.hierarchicalPFrames = 0;
        encode_config_.encodeCodecConfig.h264Config.hierarchicalBFrames = 0;
        encode_config_.encodeCodecConfig.h264Config.enableTemporalSVC = 0;
        // GIR (Gradual Intra Refresh) - OPTIONAL, disabled by default
        // Only enabled via env vars: ASTRIX_NVENC_GIR_PERIOD_FRAMES, ASTRIX_NVENC_GIR_DURATION_FRAMES
        if (gir_period_frames_ > 0 && gir_duration_frames_ > 0) {
          const bool gir_supported = GetCapability(NV_ENC_CAPS_SUPPORT_INTRA_REFRESH) != 0;
          if (gir_supported) {
            encode_config_.encodeCodecConfig.h264Config.enableIntraRefresh = 1;
            encode_config_.encodeCodecConfig.h264Config.intraRefreshPeriod = gir_period_frames_;
            encode_config_.encodeCodecConfig.h264Config.intraRefreshCnt = gir_duration_frames_;
            std::fprintf(stderr,
                "[nvenc_d11] GIR enabled: period=%u frames, duration=%u frames\n",
                gir_period_frames_, gir_duration_frames_);
          } else {
            std::fprintf(stderr,
                "[nvenc_d11] GIR requested but not supported by GPU, continuing without GIR\n");
            encode_config_.encodeCodecConfig.h264Config.enableIntraRefresh = 0;
            encode_config_.encodeCodecConfig.h264Config.intraRefreshPeriod = 0;
            encode_config_.encodeCodecConfig.h264Config.intraRefreshCnt = 0;
          }
        } else {
          encode_config_.encodeCodecConfig.h264Config.enableIntraRefresh = 0;
          encode_config_.encodeCodecConfig.h264Config.intraRefreshPeriod = 0;
          encode_config_.encodeCodecConfig.h264Config.intraRefreshCnt = 0;
        }
        encode_config_.encodeCodecConfig.h264Config.enableLTR = 0;
        encode_config_.encodeCodecConfig.h264Config.ltrNumFrames = 0;
        encode_config_.encodeCodecConfig.h264Config.ltrTrustMode = 0;
        encode_config_.encodeCodecConfig.h264Config.maxNumRefFrames =
            prefer_fast_preset ? 1u : 0u;
        encode_config_.encodeCodecConfig.h264Config.numRefL0 =
            prefer_fast_preset ? NV_ENC_NUM_REF_FRAMES_1
                               : NV_ENC_NUM_REF_FRAMES_AUTOSELECT;
        encode_config_.encodeCodecConfig.h264Config.numRefL1 =
            NV_ENC_NUM_REF_FRAMES_AUTOSELECT;
        encode_config_.encodeCodecConfig.h264Config.useBFramesAsRef =
            NV_ENC_BFRAME_REF_MODE_DISABLED;
        encode_config_.encodeCodecConfig.h264Config.bdirectMode =
            NV_ENC_H264_BDIRECT_MODE_DISABLE;
      }

      if (prefer_fast_h264_mode && !attempt.minimal_preset) {
        encode_config_.profileGUID = NV_ENC_H264_PROFILE_BASELINE_GUID;
        encode_config_.encodeCodecConfig.h264Config.entropyCodingMode =
            NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC;
        encode_config_.encodeCodecConfig.h264Config.adaptiveTransformMode =
            NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE;
        encode_config_.encodeCodecConfig.h264Config.disableDeblockingFilterIDC =
            2;
      }

      if (!attempt.conservative && !attempt.minimal_preset) {
        if (!prefer_fast_h264_mode) {
          encode_config_.profileGUID = NV_ENC_CODEC_PROFILE_AUTOSELECT_GUID;
        }
        encode_config_.encodeCodecConfig.h264Config.repeatSPSPPS = 1;
        encode_config_.encodeCodecConfig.h264Config.disableSPSPPS = 0;
        const uint32_t vbv = ComputeVbvBufferBits(bitrate_, fps_);
        encode_config_.rcParams.vbvBufferSize = vbv;
        // vbvInitialDelay at 50% allows temporary burst room before VBV constraints bite.
        // This helps smooth high-FPS frame size variations without triggering quality drops.
        encode_config_.rcParams.vbvInitialDelay = vbv / 2;
      }

      init_params_ = {};
      init_params_.version = NV_ENC_INITIALIZE_PARAMS_VER;
      init_params_.encodeConfig =
          attempt.null_config ? nullptr : &encode_config_;
      init_params_.encodeGUID = NV_ENC_CODEC_H264_GUID;
      init_params_.presetGUID = attempt.preset_guid;
      init_params_.encodeWidth = width_;
      init_params_.encodeHeight = height_;
      init_params_.darWidth = width_;
      init_params_.darHeight = height_;
      init_params_.frameRateNum = fps_;
      init_params_.frameRateDen = 1;
      init_params_.enablePTD = 1;
      init_params_.enableEncodeAsync = attempt.enable_async ? 1u : 0u;
      init_params_.maxEncodeWidth = attempt.minimal_preset ? 0u : width_;
      init_params_.maxEncodeHeight = attempt.minimal_preset ? 0u : height_;
      init_params_.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
      init_params_.bufferFormat = input_buffer_format_;
    };

    NVENCSTATUS last_status = NV_ENC_ERR_GENERIC;
    std::ostringstream failure_log;
    bool first_attempt = true;

    for (const auto& attempt : attempts) {
      if (attempt.enable_async && !async_supported) {
        continue;
      }
      if (!first_attempt) {
        if (encoder_) {
          api_.nvEncDestroyEncoder(encoder_);
          encoder_ = nullptr;
        }
        OpenSession();
      }
      first_attempt = false;

      build_attempt(attempt);
      const NVENCSTATUS status =
          api_.nvEncInitializeEncoder(encoder_, &init_params_);
      if (status == NV_ENC_SUCCESS) {
        async_encode_ = attempt.enable_async;
        std::fprintf(
            stderr,
            "[nvenc_d11] init: %s (fps=%u, async=%s, low_latency=%s, input=%s, h264_fast=%s)\n",
            attempt.label,
            fps_,
            attempt.enable_async ? "on" : "off",
            prefer_fast_preset ? "prefer-fast" : "balanced",
            input_format_label_,
            prefer_fast_h264_mode ? "on" : "off");
        if (!attempt.null_config) {
          LogH264LowLatencyConfig(attempt.label, init_params_, encode_config_);
        } else {
          std::fprintf(stderr,
                       "[nvenc_d11] config: label=%s encodeConfig=null "
                       "(driver preset only; low-latency fields not inspectable here)\n",
                       attempt.label);
        }
        return;
      }

      last_status = status;
      failure_log << attempt.label << "=" << static_cast<int>(status) << ' ';
    }

    ThrowNvenc("nvEncInitializeEncoder failed after attempts: " +
                   failure_log.str(),
               last_status);
  }

  void CreateOutputRing() {
    outputs_.resize(ring_size_);
    for (auto& out : outputs_) {
      out.completion_event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
      if (!out.completion_event) {
        throw std::runtime_error("CreateEventW failed for NVENC completion event");
      }

      NV_ENC_EVENT_PARAMS event_params = {NV_ENC_EVENT_PARAMS_VER};
      event_params.completionEvent = out.completion_event;
      CheckNvenc(api_.nvEncRegisterAsyncEvent(encoder_, &event_params),
                 "nvEncRegisterAsyncEvent failed");

      NV_ENC_CREATE_BITSTREAM_BUFFER bitstream = {
          NV_ENC_CREATE_BITSTREAM_BUFFER_VER};
      CheckNvenc(api_.nvEncCreateBitstreamBuffer(encoder_, &bitstream),
                 "nvEncCreateBitstreamBuffer failed");
      out.bitstream = bitstream.bitstreamBuffer;
    }
  }

  void RegisterInputs(rust::Vec<std::uintptr_t> texture_ptrs) {
    textures_.reserve(texture_ptrs.size());
    registered_inputs_.reserve(texture_ptrs.size());

    for (std::size_t i = 0; i < texture_ptrs.size(); ++i) {
      auto* texture = reinterpret_cast<ID3D11Texture2D*>(texture_ptrs[i]);
      if (!texture) {
        throw std::runtime_error("NVENC D3D11 texture pointer is null");
      }
      texture->AddRef();

      D3D11_TEXTURE2D_DESC desc = {};
      texture->GetDesc(&desc);
      if (desc.Width != width_ || desc.Height != height_) {
        throw std::runtime_error("NVENC D3D11 input ring texture size mismatch");
      }
      if (desc.Format != input_dxgi_format_) {
        throw std::runtime_error("NVENC D3D11 input ring texture format mismatch");
      }

      NV_ENC_REGISTER_RESOURCE resource = {NV_ENC_REGISTER_RESOURCE_VER};
      resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
      resource.width = width_;
      resource.height = height_;
      resource.pitch = 0;
      resource.subResourceIndex = 0;
      resource.resourceToRegister = texture;
      resource.bufferFormat = input_buffer_format_;
      resource.bufferUsage = NV_ENC_INPUT_IMAGE;
      CheckNvenc(api_.nvEncRegisterResource(encoder_, &resource),
                 "nvEncRegisterResource failed");

      texture_to_slot_.emplace(texture_ptrs[i], static_cast<uint32_t>(i));
      textures_.push_back(texture);
      registered_inputs_.push_back(resource.registeredResource);
    }
  }

  void Cleanup() {
    StopOutputWorker();

    if (encoder_) {
      for (auto& out : outputs_) {
        if (out.mapped_input) {
          api_.nvEncUnmapInputResource(encoder_, out.mapped_input);
          out.mapped_input = nullptr;
        }
      }

      for (auto resource : registered_inputs_) {
        if (resource) {
          api_.nvEncUnregisterResource(encoder_, resource);
        }
      }
      registered_inputs_.clear();

      for (auto& out : outputs_) {
        if (out.bitstream) {
          api_.nvEncDestroyBitstreamBuffer(encoder_, out.bitstream);
          out.bitstream = nullptr;
        }
        if (out.completion_event) {
          NV_ENC_EVENT_PARAMS event_params = {NV_ENC_EVENT_PARAMS_VER};
          event_params.completionEvent = out.completion_event;
          api_.nvEncUnregisterAsyncEvent(encoder_, &event_params);
          CloseHandle(out.completion_event);
          out.completion_event = nullptr;
        }
      }
      outputs_.clear();

      api_.nvEncDestroyEncoder(encoder_);
      encoder_ = nullptr;
    }

    {
      std::lock_guard<std::mutex> lock(queue_mutex_);
      submit_queue_.clear();
      in_flight_.clear();
      completed_.clear();
      worker_error_.clear();
    }

    if (stop_event_) {
      CloseHandle(stop_event_);
      stop_event_ = nullptr;
    }
    if (submit_event_) {
      CloseHandle(submit_event_);
      submit_event_ = nullptr;
    }

    for (auto* texture : textures_) {
      if (texture) {
        texture->Release();
      }
    }
    textures_.clear();

    if (module_) {
      FreeLibrary(module_);
      module_ = nullptr;
    }

    if (device_) {
      device_->Release();
      device_ = nullptr;
    }
  }

  void StartOutputWorker() {
    output_worker_ = std::thread([this]() { OutputWorkerLoop(); });
  }

  void StopOutputWorker() {
    {
      std::lock_guard<std::mutex> lock(queue_mutex_);
      stop_requested_ = true;
    }
    if (stop_event_) {
      SetEvent(stop_event_);
    }
    if (submit_event_) {
      SetEvent(submit_event_);
    }
    pending_cv_.notify_all();
    completed_cv_.notify_all();
    if (output_worker_.joinable()) {
      output_worker_.join();
    }
  }

  void OutputWorkerLoop() {
    try {
      ApplyNvencWorkerPriority();
      while (true) {
        PendingSubmission queued_submission;
        bool have_queued_submission = false;
        bool have_ready_completion = false;
        HANDLE completion_event = nullptr;
        {
          std::unique_lock<std::mutex> lock(queue_mutex_);
          pending_cv_.wait(lock, [&] {
            return stop_requested_ || !submit_queue_.empty() || !in_flight_.empty();
          });
          if (stop_requested_ && submit_queue_.empty() && in_flight_.empty()) {
            return;
          }

          if (!in_flight_.empty()) {
            completion_event = outputs_[in_flight_.front().output_slot].completion_event;
            const DWORD ready_result = WaitForSingleObject(completion_event, 0);
            if (ready_result == WAIT_OBJECT_0) {
              have_ready_completion = true;
            } else if (ready_result != WAIT_TIMEOUT) {
              throw std::runtime_error(
                  "WaitForSingleObject failed for NVENC completion event");
            }
          }

          // Prefer harvesting a ready bitstream before feeding more input. Otherwise
          // a 120fps submit stream can keep the worker enqueue-biased until the whole
          // output ring is full, causing transient QueueFull spikes and extra latency.
          if (!have_ready_completion && !submit_queue_.empty() &&
              in_flight_.size() < outputs_.size()) {
            queued_submission = submit_queue_.front();
            submit_queue_.pop_front();
            have_queued_submission = true;
          } else if (!have_ready_completion && !in_flight_.empty()) {
            completion_event = outputs_[in_flight_.front().output_slot].completion_event;
          } else if (!have_ready_completion) {
            continue;
          }
        }

        if (have_queued_submission) {
          NV_ENC_INPUT_PTR mapped_input = nullptr;
          std::uint64_t map_us = 0;
          std::uint64_t encode_picture_us = 0;
          const auto submit_started_at = std::chrono::steady_clock::now();
          {
            std::lock_guard<std::mutex> api_lock(api_mutex_);
            const auto map_started_at = std::chrono::steady_clock::now();
            NV_ENC_MAP_INPUT_RESOURCE map_input = {NV_ENC_MAP_INPUT_RESOURCE_VER};
            map_input.registeredResource =
                registered_inputs_[queued_submission.input_slot];
            CheckNvenc(api_.nvEncMapInputResource(encoder_, &map_input),
                       "nvEncMapInputResource failed");
            mapped_input = map_input.mappedResource;
            map_us = static_cast<std::uint64_t>(
                std::chrono::duration_cast<std::chrono::microseconds>(
                    std::chrono::steady_clock::now() - map_started_at)
                    .count());

            auto& out = outputs_[queued_submission.output_slot];
            NV_ENC_PIC_PARAMS pic_params = {NV_ENC_PIC_PARAMS_VER};
            pic_params.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
            pic_params.inputBuffer = mapped_input;
            pic_params.bufferFmt = input_buffer_format_;
            pic_params.inputWidth = width_;
            pic_params.inputHeight = height_;
            pic_params.outputBitstream = out.bitstream;
            pic_params.completionEvent = out.completion_event;
            if (queued_submission.force_idr) {
              pic_params.encodePicFlags = NV_ENC_PIC_FLAG_FORCEINTRA |
                                          NV_ENC_PIC_FLAG_FORCEIDR |
                                          NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
            }

            const auto encode_started_at = std::chrono::steady_clock::now();
            const NVENCSTATUS status = api_.nvEncEncodePicture(encoder_, &pic_params);
            encode_picture_us = static_cast<std::uint64_t>(
                std::chrono::duration_cast<std::chrono::microseconds>(
                    std::chrono::steady_clock::now() - encode_started_at)
                    .count());
            if (status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT) {
              if (mapped_input != nullptr) {
                api_.nvEncUnmapInputResource(encoder_, mapped_input);
                mapped_input = nullptr;
              }
              ThrowNvenc("nvEncEncodePicture failed", status);
            }
          }

          {
            std::lock_guard<std::mutex> lock(queue_mutex_);
            ThrowWorkerErrorLocked();
            auto& out = outputs_[queued_submission.output_slot];
            if (!out.reserved) {
              throw std::runtime_error(
                  "NVENC D3D11 output slot lost reservation before submit");
            }
            if (out.mapped_input != nullptr) {
              throw std::runtime_error(
                  "NVENC D3D11 output slot became busy in worker submit");
            }
            out.mapped_input = mapped_input;
            queued_submission.submitted_at = std::chrono::steady_clock::now();
            in_flight_.push_back(queued_submission);
          }

          last_submit_map_us_.store(map_us, std::memory_order_relaxed);
          last_submit_encode_picture_us_.store(
              encode_picture_us, std::memory_order_relaxed);
          last_submit_total_us_.store(
              static_cast<std::uint64_t>(
                  std::chrono::duration_cast<std::chrono::microseconds>(
                      std::chrono::steady_clock::now() - submit_started_at)
                      .count()),
              std::memory_order_relaxed);
          continue;
        }

        if (!have_ready_completion) {
          HANDLE wait_handles[3] = {stop_event_, submit_event_, completion_event};
          const DWORD wait_result =
              WaitForMultipleObjects(3, wait_handles, FALSE, INFINITE);
          if (wait_result == WAIT_OBJECT_0) {
            return;
          }
          if (wait_result == WAIT_OBJECT_0 + 1) {
            continue;
          }
          if (wait_result != WAIT_OBJECT_0 + 2) {
            throw std::runtime_error(
                "WaitForMultipleObjects failed for NVENC completion event");
          }
        }

        PendingSubmission submission;
        {
          std::lock_guard<std::mutex> lock(queue_mutex_);
          if (in_flight_.empty()) {
            throw std::runtime_error(
                "NVENC D3D11 output worker lost in-flight submission state");
          }
          submission = in_flight_.front();
        }

        CompletedPacket packet;
        {
          std::lock_guard<std::mutex> api_lock(api_mutex_);
          auto& out = outputs_[submission.output_slot];
          NV_ENC_LOCK_BITSTREAM lock = {NV_ENC_LOCK_BITSTREAM_VER};
          lock.outputBitstream = out.bitstream;
          lock.doNotWait = false;
          CheckNvenc(api_.nvEncLockBitstream(encoder_, &lock),
                     "nvEncLockBitstream failed");

          const auto* ptr =
              static_cast<const std::uint8_t*>(lock.bitstreamBufferPtr);
          packet.data.assign(ptr, ptr + lock.bitstreamSizeInBytes);
          packet.encode_time_us = static_cast<std::uint64_t>(
              std::chrono::duration_cast<std::chrono::microseconds>(
                  std::chrono::steady_clock::now() - submission.submitted_at)
                  .count());

          CheckNvenc(api_.nvEncUnlockBitstream(encoder_, out.bitstream),
                     "nvEncUnlockBitstream failed");
          if (out.mapped_input) {
            CheckNvenc(api_.nvEncUnmapInputResource(encoder_, out.mapped_input),
                       "nvEncUnmapInputResource failed");
          }
        }

        {
          std::lock_guard<std::mutex> lock(queue_mutex_);
          auto& out = outputs_[submission.output_slot];
          out.mapped_input = nullptr;
          out.reserved = false;
          if (in_flight_.empty()) {
            throw std::runtime_error(
                "NVENC D3D11 output worker lost in-flight submission state");
          }
          in_flight_.pop_front();
          completed_.push_back(std::move(packet));
        }
        completed_cv_.notify_one();
      }
    } catch (const std::exception& e) {
      SetWorkerError(std::string("NVENC D3D11 output worker failed: ") +
                     e.what());
    } catch (...) {
      SetWorkerError("NVENC D3D11 output worker failed: unknown exception");
    }
  }

  void SetWorkerError(const std::string& message) {
    {
      std::lock_guard<std::mutex> lock(queue_mutex_);
      if (worker_error_.empty()) {
        worker_error_ = message;
      }
      stop_requested_ = true;
    }
    if (stop_event_) {
      SetEvent(stop_event_);
    }
    if (submit_event_) {
      SetEvent(submit_event_);
    }
    pending_cv_.notify_all();
    completed_cv_.notify_all();
  }

  void ThrowWorkerErrorLocked() const {
    if (!worker_error_.empty()) {
      throw std::runtime_error(worker_error_);
    }
  }

  HMODULE module_ = nullptr;
  NV_ENCODE_API_FUNCTION_LIST api_ = {NV_ENCODE_API_FUNCTION_LIST_VER};
  ID3D11Device* device_ = nullptr;
  void* encoder_ = nullptr;
  std::string encoder_name_;
  uint32_t width_ = 0;
  uint32_t height_ = 0;
  uint32_t fps_ = 0;
  uint32_t bitrate_ = 0;
  uint32_t ring_size_ = 0;
  NV_ENC_BUFFER_FORMAT input_buffer_format_ = NV_ENC_BUFFER_FORMAT_NV12;
  DXGI_FORMAT input_dxgi_format_ = DXGI_FORMAT_NV12;
  const char* input_format_label_ = "NV12";
  bool async_encode_ = false;
  std::vector<ID3D11Texture2D*> textures_;
  std::vector<NV_ENC_REGISTERED_PTR> registered_inputs_;
  std::unordered_map<std::uintptr_t, uint32_t> texture_to_slot_;
  std::vector<OutputSlot> outputs_;
  HANDLE stop_event_ = nullptr;
  HANDLE submit_event_ = nullptr;
  std::thread output_worker_;
  mutable std::mutex queue_mutex_;
  std::mutex submit_mutex_;
  std::mutex api_mutex_;
  std::condition_variable pending_cv_;
  std::condition_variable completed_cv_;
  std::deque<PendingSubmission> submit_queue_;
  std::deque<PendingSubmission> in_flight_;
  std::deque<CompletedPacket> completed_;
  std::string worker_error_;
  std::size_t next_submit_index_ = 0;
  bool stop_requested_ = false;
  std::uint64_t last_encode_time_us_ = 0;
  std::atomic<std::uint64_t> last_submit_map_us_{0};
  std::atomic<std::uint64_t> last_submit_encode_picture_us_{0};
  std::atomic<std::uint64_t> last_submit_total_us_{0};
  uint32_t gir_period_frames_ = 0;
  uint32_t gir_duration_frames_ = 0;
  NV_ENC_INITIALIZE_PARAMS init_params_ = {};
  NV_ENC_CONFIG encode_config_ = {};
};

NvencD3D11Session::NvencD3D11Session(std::uintptr_t d3d11_device,
                                     std::uint32_t width,
                                     std::uint32_t height,
                                     std::uint32_t fps,
                                     std::uint32_t bitrate,
                                     rust::Vec<std::uintptr_t> texture_ptrs,
                                     std::uint32_t gir_period_frames,
                                     std::uint32_t gir_duration_frames)
    : impl_(std::make_unique<Impl>(d3d11_device,
                                   width,
                                   height,
                                   fps,
                                   bitrate,
                                   std::move(texture_ptrs),
                                   gir_period_frames,
                                   gir_duration_frames)) {}

NvencD3D11Session::~NvencD3D11Session() = default;

rust::String NvencD3D11Session::encoder_name() const {
  return rust::String(impl_->encoder_name());
}

bool NvencD3D11Session::is_async() const {
  return impl_->is_async();
}

std::uint32_t NvencD3D11Session::input_ring_size() const {
  return impl_->ring_size();
}

std::uint32_t NvencD3D11Session::in_flight_count() const {
  return impl_->in_flight_count();
}

rust::Vec<std::uint8_t> NvencD3D11Session::collect(std::uint32_t timeout_ms) {
  return impl_->collect(timeout_ms);
}

std::uint64_t NvencD3D11Session::last_encode_time_us() const {
  return impl_->last_encode_time_us();
}

std::uint64_t NvencD3D11Session::last_submit_map_us() const {
  return impl_->last_submit_map_us();
}

std::uint64_t NvencD3D11Session::last_submit_encode_picture_us() const {
  return impl_->last_submit_encode_picture_us();
}

std::uint64_t NvencD3D11Session::last_submit_total_us() const {
  return impl_->last_submit_total_us();
}

void NvencD3D11Session::submit(std::uintptr_t texture_ptr, bool force_idr) {
  impl_->submit(texture_ptr, force_idr);
}

void NvencD3D11Session::set_bitrate(std::uint32_t bitrate) {
  impl_->set_bitrate(bitrate);
}

std::unique_ptr<NvencD3D11Session> nvenc_d3d11_create(
    std::uintptr_t d3d11_device,
    std::uint32_t width,
    std::uint32_t height,
    std::uint32_t fps,
    std::uint32_t bitrate,
    rust::Vec<std::uintptr_t> texture_ptrs,
    std::uint32_t gir_period_frames,
    std::uint32_t gir_duration_frames) {
  return std::make_unique<NvencD3D11Session>(
      d3d11_device, width, height, fps, bitrate, std::move(texture_ptrs),
      gir_period_frames, gir_duration_frames);
}

}  // namespace astrix_nvenc

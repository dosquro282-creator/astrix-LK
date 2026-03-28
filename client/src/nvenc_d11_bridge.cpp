#include "astrix/nvenc_d11_bridge.h"

#include <d3d11.h>
#include <dxgi.h>
#include <windows.h>

#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <deque>
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

}  // namespace

struct PendingSubmission {
  uint32_t output_slot = 0;
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
};

struct NvencD3D11Session::Impl {
  Impl(std::uintptr_t d3d11_device,
       std::uint32_t width,
       std::uint32_t height,
       std::uint32_t fps,
       std::uint32_t bitrate,
       rust::Vec<std::uintptr_t> texture_ptrs)
      : width_(width),
        height_(height),
        fps_(fps ? fps : 60),
        bitrate_(bitrate),
        ring_size_(static_cast<uint32_t>(texture_ptrs.size())) {
    if (ring_size_ == 0) {
      throw std::runtime_error("NVENC D3D11 requires at least one NV12 texture");
    }

    device_ = reinterpret_cast<ID3D11Device*>(d3d11_device);
    if (!device_) {
      throw std::runtime_error("NVENC D3D11 device pointer is null");
    }
    device_->AddRef();
    encoder_name_ = AdapterNameFromDevice(device_);

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
    StartOutputWorker();
  }

  ~Impl() { Cleanup(); }

  const std::string& encoder_name() const { return encoder_name_; }

  bool is_async() const { return async_encode_; }

  std::uint32_t ring_size() const { return ring_size_; }

  std::uint32_t in_flight_count() const {
    std::lock_guard<std::mutex> lock(queue_mutex_);
    ThrowWorkerErrorLocked();
    return static_cast<std::uint32_t>(pending_.size());
  }

  std::uint64_t last_encode_time_us() const {
    std::lock_guard<std::mutex> lock(queue_mutex_);
    return last_encode_time_us_;
  }

  void submit(std::uintptr_t texture_ptr, bool force_idr) {
    std::lock_guard<std::mutex> submit_lock(submit_mutex_);

    uint32_t input_slot = 0;
    uint32_t output_slot = 0;
    HANDLE completion_event = nullptr;
    NV_ENC_OUTPUT_PTR bitstream = nullptr;

    {
      std::lock_guard<std::mutex> lock(queue_mutex_);
      ThrowWorkerErrorLocked();
      if (pending_.size() >= outputs_.size()) {
        throw std::runtime_error("NVENC D3D11 queue is full");
      }

      const auto it = texture_to_slot_.find(texture_ptr);
      if (it == texture_to_slot_.end()) {
        throw std::runtime_error(
            "NVENC D3D11 received a texture outside the registered ring");
      }
      input_slot = it->second;
      output_slot =
          static_cast<uint32_t>(next_submit_index_ % outputs_.size());
      auto& out = outputs_[output_slot];
      if (out.mapped_input != nullptr) {
        throw std::runtime_error(
            "NVENC D3D11 output slot still has a mapped input");
      }
      completion_event = out.completion_event;
      bitstream = out.bitstream;
    }

    NV_ENC_INPUT_PTR mapped_input = nullptr;
    {
      std::lock_guard<std::mutex> api_lock(api_mutex_);
      NV_ENC_MAP_INPUT_RESOURCE map_input = {NV_ENC_MAP_INPUT_RESOURCE_VER};
      map_input.registeredResource = registered_inputs_[input_slot];
      CheckNvenc(api_.nvEncMapInputResource(encoder_, &map_input),
                 "nvEncMapInputResource failed");
      mapped_input = map_input.mappedResource;

      NV_ENC_PIC_PARAMS pic_params = {NV_ENC_PIC_PARAMS_VER};
      pic_params.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
      pic_params.inputBuffer = mapped_input;
      pic_params.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
      pic_params.inputWidth = width_;
      pic_params.inputHeight = height_;
      pic_params.outputBitstream = bitstream;
      pic_params.completionEvent = completion_event;
      if (force_idr) {
        pic_params.encodePicFlags = NV_ENC_PIC_FLAG_FORCEINTRA |
                                    NV_ENC_PIC_FLAG_FORCEIDR |
                                    NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
      }

      const NVENCSTATUS status = api_.nvEncEncodePicture(encoder_, &pic_params);
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
      auto& out = outputs_[output_slot];
      if (out.mapped_input != nullptr) {
        std::lock_guard<std::mutex> api_lock(api_mutex_);
        if (mapped_input != nullptr) {
          api_.nvEncUnmapInputResource(encoder_, mapped_input);
        }
        throw std::runtime_error(
            "NVENC D3D11 output slot became busy during submit");
      }
      out.mapped_input = mapped_input;
      pending_.push_back(PendingSubmission{
          .output_slot = output_slot,
          .submitted_at = std::chrono::steady_clock::now(),
      });
      next_submit_index_++;
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
    const uint32_t vbv =
        std::max<std::uint32_t>(bitrate / std::max<std::uint32_t>(fps_, 1), 1u) * 2u;
    config.rcParams.vbvBufferSize = vbv;
    config.rcParams.vbvInitialDelay = vbv;
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
    std::vector<InitAttempt> attempts = {
        {"custom async p4", async_supported, false, false, false, NV_ENC_PRESET_P4_GUID},
        {"preset async p4", async_supported, true, false, false, NV_ENC_PRESET_P4_GUID},
        {"preset sync p4", false, true, false, false, NV_ENC_PRESET_P4_GUID},
        {"minimal sync p1", false, true, true, false, NV_ENC_PRESET_P1_GUID},
        {"null-config sync p1", false, true, true, true, NV_ENC_PRESET_P1_GUID},
    };

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
        encode_config_.rcParams.enableAQ = 0;
        encode_config_.rcParams.enableLookahead = 0;
        encode_config_.rcParams.zeroReorderDelay = 1;
      }

      if (!attempt.conservative && !attempt.minimal_preset) {
        encode_config_.profileGUID = NV_ENC_CODEC_PROFILE_AUTOSELECT_GUID;
        encode_config_.encodeCodecConfig.h264Config.repeatSPSPPS = 1;
        encode_config_.encodeCodecConfig.h264Config.disableSPSPPS = 0;
        const uint32_t vbv =
            std::max<std::uint32_t>(bitrate_ / std::max<std::uint32_t>(fps_, 1),
                                    1u) *
            2u;
        encode_config_.rcParams.vbvBufferSize = vbv;
        encode_config_.rcParams.vbvInitialDelay = vbv;
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
      init_params_.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
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
      if (desc.Format != DXGI_FORMAT_NV12) {
        throw std::runtime_error("NVENC D3D11 input ring must use DXGI_FORMAT_NV12");
      }

      NV_ENC_REGISTER_RESOURCE resource = {NV_ENC_REGISTER_RESOURCE_VER};
      resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
      resource.width = width_;
      resource.height = height_;
      resource.pitch = 0;
      resource.subResourceIndex = 0;
      resource.resourceToRegister = texture;
      resource.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
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
      pending_.clear();
      completed_.clear();
      worker_error_.clear();
    }

    if (stop_event_) {
      CloseHandle(stop_event_);
      stop_event_ = nullptr;
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
    pending_cv_.notify_all();
    completed_cv_.notify_all();
    if (output_worker_.joinable()) {
      output_worker_.join();
    }
  }

  void OutputWorkerLoop() {
    try {
      while (true) {
        PendingSubmission submission;
        HANDLE completion_event = nullptr;
        {
          std::unique_lock<std::mutex> lock(queue_mutex_);
          pending_cv_.wait(lock, [&] {
            return stop_requested_ || !pending_.empty();
          });
          if (stop_requested_) {
            return;
          }
          submission = pending_.front();
          completion_event = outputs_[submission.output_slot].completion_event;
        }

        HANDLE wait_handles[2] = {stop_event_, completion_event};
        const DWORD wait_result =
            WaitForMultipleObjects(2, wait_handles, FALSE, INFINITE);
        if (wait_result == WAIT_OBJECT_0) {
          return;
        }
        if (wait_result != WAIT_OBJECT_0 + 1) {
          throw std::runtime_error(
              "WaitForMultipleObjects failed for NVENC completion event");
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
          if (pending_.empty()) {
            throw std::runtime_error(
                "NVENC D3D11 output worker lost pending submission state");
          }
          pending_.pop_front();
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
  bool async_encode_ = false;
  std::vector<ID3D11Texture2D*> textures_;
  std::vector<NV_ENC_REGISTERED_PTR> registered_inputs_;
  std::unordered_map<std::uintptr_t, uint32_t> texture_to_slot_;
  std::vector<OutputSlot> outputs_;
  HANDLE stop_event_ = nullptr;
  std::thread output_worker_;
  mutable std::mutex queue_mutex_;
  std::mutex submit_mutex_;
  std::mutex api_mutex_;
  std::condition_variable pending_cv_;
  std::condition_variable completed_cv_;
  std::deque<PendingSubmission> pending_;
  std::deque<CompletedPacket> completed_;
  std::string worker_error_;
  std::size_t next_submit_index_ = 0;
  bool stop_requested_ = false;
  std::uint64_t last_encode_time_us_ = 0;
  NV_ENC_INITIALIZE_PARAMS init_params_ = {};
  NV_ENC_CONFIG encode_config_ = {};
};

NvencD3D11Session::NvencD3D11Session(std::uintptr_t d3d11_device,
                                     std::uint32_t width,
                                     std::uint32_t height,
                                     std::uint32_t fps,
                                     std::uint32_t bitrate,
                                     rust::Vec<std::uintptr_t> texture_ptrs)
    : impl_(std::make_unique<Impl>(d3d11_device,
                                   width,
                                   height,
                                   fps,
                                   bitrate,
                                   std::move(texture_ptrs))) {}

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
    rust::Vec<std::uintptr_t> texture_ptrs) {
  return std::make_unique<NvencD3D11Session>(
      d3d11_device, width, height, fps, bitrate, std::move(texture_ptrs));
}

}  // namespace astrix_nvenc

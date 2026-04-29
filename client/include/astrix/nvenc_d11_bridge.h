#pragma once

#include <cstdint>
#include <memory>

#include "rust/cxx.h"

namespace astrix_nvenc {
class NvencD3D11Session;
}
#include "astrix-client/src/nvenc_d11_bridge.rs.h"

namespace astrix_nvenc {

class NvencD3D11Session {
 public:
  NvencD3D11Session(std::uintptr_t d3d11_device,
                    std::uint32_t width,
                    std::uint32_t height,
                    std::uint32_t fps,
                    std::uint32_t bitrate,
                    rust::Vec<std::uintptr_t> texture_ptrs,
                    std::uint32_t gir_period_frames = 0,
                    std::uint32_t gir_duration_frames = 0);
  ~NvencD3D11Session();

  rust::String encoder_name() const;
  bool is_async() const;
  std::uint32_t input_ring_size() const;
  std::uint32_t in_flight_count() const;
  rust::Vec<std::uint8_t> collect(std::uint32_t timeout_ms);
  std::uint64_t last_encode_time_us() const;
  std::uint64_t last_submit_map_us() const;
  std::uint64_t last_submit_encode_picture_us() const;
  std::uint64_t last_submit_total_us() const;
  void submit(std::uintptr_t texture_ptr, bool force_idr);
  void set_bitrate(std::uint32_t bitrate);

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

std::unique_ptr<NvencD3D11Session> nvenc_d3d11_create(
    std::uintptr_t d3d11_device,
    std::uint32_t width,
    std::uint32_t height,
    std::uint32_t fps,
    std::uint32_t bitrate,
    rust::Vec<std::uintptr_t> texture_ptrs,
    std::uint32_t gir_period_frames = 0,
    std::uint32_t gir_duration_frames = 0);

}  // namespace astrix_nvenc

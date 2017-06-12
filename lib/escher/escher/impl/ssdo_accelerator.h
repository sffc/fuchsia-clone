// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#pragma once

#include "escher/forward_declarations.h"
#include "escher/impl/compute_shader.h"
#include "escher/renderer/timestamper.h"

namespace escher {
namespace impl {

// The purpose of this class is to generate a lookup table that can decide
// whether SSDO sampling/filtering can be skipped for a given pixel.
class SsdoAccelerator {
 public:
  SsdoAccelerator(GlslToSpirvCompiler* compiler,
                  ImageCache* image_cache,
                  ResourceLifePreserver* life_preserver);
  ~SsdoAccelerator();

  // Generates a packed lookup table to accelerate SSDO sampling/filtering.
  // 2 bits of information are computed for each pixel of the input depth image;
  // these indicate whether SSDO sampling and/or filtering is required for that
  // pixel.
  //
  // To reduce memory bandwidth requirements, the information for a 4x4
  // region of the depth image is packed into a single 32-bit RGBA pixel.
  // Each row of the input region is stored into a single 8-bit channel, e.g.
  // the red channel represents row0.  Within each channel, the leftmost result
  // is stored in the 2 least-significant bits, and the rightmost in the 2 most
  // significant bits.  Of each pair of bits, the less-significant one indicates
  // whether SSDO sampling is required, and the other indicates whether SSDO
  // filtering is required.
  TexturePtr GenerateLookupTable(CommandBuffer* command_buffer,
                                 const TexturePtr& depth_texture,
                                 vk::ImageUsageFlags image_flags,
                                 Timestamper* timestamper);

  // Unpack the image generated by GenerateLookupTable(), or another image with
  // the same packed format, into an image 4x larger in each dimension, suitable
  // for debug visualization.  For each ouput pixel, the corresponding pair of
  // packed bits determine the red and green values.  For example, if the less-
  // significant of the packed bits is 1, then the red channel is set to 1.0,
  // otherwise 0.0.
  TexturePtr UnpackLookupTable(CommandBuffer* command_buffer,
                               const TexturePtr& packed_lookup_table,
                               uint32_t output_width,
                               uint32_t output_height,
                               Timestamper* timestamper);

  // Cycle through the available SSDO acceleration modes.  This is a temporary
  // API: eventually there will only be one mode (the best one!), but this is
  // useful during development.
  void CycleMode();

 private:
  const VulkanContext& vulkan_context() const;

  TexturePtr GenerateHighLowLookupTable(CommandBuffer* command_buffer,
                                        const TexturePtr& depth_texture,
                                        vk::ImageUsageFlags image_flags,
                                        Timestamper* timestamper);
  TexturePtr GenerateSampleLookupTable(CommandBuffer* command_buffer,
                                       const TexturePtr& depth_texture,
                                       vk::ImageUsageFlags image_flags,
                                       Timestamper* timestamper);
  TexturePtr GenerateNullLookupTable(CommandBuffer* command_buffer,
                                     const TexturePtr& depth_texture,
                                     vk::ImageUsageFlags image_flags,
                                     Timestamper* timestamper);

  // Temporary, so that we can lazily initialize ComputeShaders as needed.
  // Eventually, we'll know exactly which we need, and eagerly initialize them.
  GlslToSpirvCompiler* compiler_;

  ImageCache* const image_cache_;
  ResourceLifePreserver* const life_preserver_;

  // Used by GenerateLookupTable().
  std::unique_ptr<ComputeShader> high_low_neighbors_packed_kernel_;
  std::unique_ptr<ComputeShader> high_low_neighbors_packed_parallel_kernel_;
  // Takes input generated by high_low_neighbors_packed_kernel_, and fills the
  // output image that is returned by GenerateLookupTable().
  std::unique_ptr<ComputeShader> sampling_filtering_packed_kernel_;
  // Generates an output image that is returned by GenerateLookupTable(), which
  // says that sampling/filtering is necessary for all pixels.
  std::unique_ptr<ComputeShader> null_packed_kernel_;
  // Used by UnpackLookupTable() to unpack an image in the same format as
  // generated by GenerateLookupTable().
  std::unique_ptr<ComputeShader> unpack_32_to_2_kernel_;

  uint32_t mode_ = 1;

  FRIEND_REF_COUNTED_THREAD_SAFE(SsdoAccelerator);
  FTL_DISALLOW_COPY_AND_ASSIGN(SsdoAccelerator);
};

}  // namespace impl
}  // namespace escher

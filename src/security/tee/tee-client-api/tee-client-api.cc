// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <arpa/inet.h>
#include <fcntl.h>
#include <fuchsia/hardware/tee/llcpp/fidl.h>
#include <fuchsia/tee/llcpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <lib/zx/channel.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>

#include <cstring>
#include <optional>
#include <string_view>
#include <utility>
#include <variant>
#include <vector>

#include <tee-client-api/tee_client_api.h>

namespace {

// LLCPP FIDL Type Construction Helpers

class Value {
 public:
  fuchsia_tee::wire::Value to_llcpp() {
    if (direction_.has_value()) {
      llcpp_builder_.set_direction(fidl::unowned_ptr(&direction_.value()));
    }
    if (a_.has_value()) {
      llcpp_builder_.set_a(fidl::unowned_ptr(&a_.value()));
    }
    if (b_.has_value()) {
      llcpp_builder_.set_b(fidl::unowned_ptr(&b_.value()));
    }
    if (c_.has_value()) {
      llcpp_builder_.set_c(fidl::unowned_ptr(&c_.value()));
    }

    return llcpp_builder_.build();
  }

  void set_direction(fuchsia_tee::wire::Direction direction) { direction_ = direction; }

  void set_a(uint64_t a) { a_ = a; }

  void set_b(uint64_t b) { b_ = b; }

  void set_c(uint64_t c) { c_ = c; }

 private:
  fuchsia_tee::wire::Value::UnownedBuilder llcpp_builder_;

  std::optional<fuchsia_tee::wire::Direction> direction_{};
  std::optional<uint64_t> a_{};
  std::optional<uint64_t> b_{};
  std::optional<uint64_t> c_{};
};

class Buffer {
 public:
  fuchsia_tee::wire::Buffer to_llcpp() {
    if (direction_.has_value()) {
      llcpp_builder_.set_direction(fidl::unowned_ptr(&direction_.value()));
    }
    if (vmo_.has_value()) {
      llcpp_builder_.set_vmo(fidl::unowned_ptr(&vmo_.value()));
    }
    if (offset_.has_value()) {
      llcpp_builder_.set_offset(fidl::unowned_ptr(&offset_.value()));
    }
    if (size_.has_value()) {
      llcpp_builder_.set_size(fidl::unowned_ptr(&size_.value()));
    }

    return llcpp_builder_.build();
  }

  void set_direction(fuchsia_tee::wire::Direction direction) { direction_ = direction; }

  void set_vmo(zx::vmo vmo) { vmo_ = std::move(vmo); }

  void set_offset(uint64_t offset) { offset_ = offset; }

  void set_size(uint64_t size) { size_ = size; }

 private:
  fuchsia_tee::wire::Buffer::UnownedBuilder llcpp_builder_;

  std::optional<fuchsia_tee::wire::Direction> direction_{};
  std::optional<zx::vmo> vmo_{};
  std::optional<uint64_t> offset_{};
  std::optional<uint64_t> size_{};
};

class Parameter {
 public:
  fuchsia_tee::wire::Parameter to_llcpp() {
    if (std::holds_alternative<fidl::aligned<fuchsia_tee::wire::None>>(data_)) {
      llcpp_data_ = std::get<fidl::aligned<fuchsia_tee::wire::None>>(data_);
      return fuchsia_tee::wire::Parameter::WithNone(
          fidl::unowned_ptr(&std::get<fidl::aligned<fuchsia_tee::wire::None>>(llcpp_data_)));
    }
    if (std::holds_alternative<Value>(data_)) {
      llcpp_data_ = std::get<Value>(data_).to_llcpp();
      return fuchsia_tee::wire::Parameter::WithValue(
          fidl::unowned_ptr(&std::get<fuchsia_tee::wire::Value>(llcpp_data_)));
    }
    if (std::holds_alternative<Buffer>(data_)) {
      llcpp_data_ = std::get<Buffer>(data_).to_llcpp();
      return fuchsia_tee::wire::Parameter::WithBuffer(
          fidl::unowned_ptr(&std::get<fuchsia_tee::wire::Buffer>(llcpp_data_)));
    }

    return fuchsia_tee::wire::Parameter();
  }

  void set_none() { data_ = fuchsia_tee::wire::None{}; }

  void set_value(Value value) { data_ = std::move(value); }

  void set_buffer(Buffer buffer) { data_ = std::move(buffer); }

 private:
  std::variant<std::monostate, fidl::aligned<fuchsia_tee::wire::None>, fuchsia_tee::wire::Value,
               fuchsia_tee::wire::Buffer>
      llcpp_data_{};

  std::variant<std::monostate, fidl::aligned<fuchsia_tee::wire::None>, Value, Buffer> data_{};
};

class ParameterSet {
 public:
  fidl::VectorView<fuchsia_tee::wire::Parameter> to_llcpp() {
    ZX_DEBUG_ASSERT(parameters_.has_value());

    llcpp_parameters_.clear();
    llcpp_parameters_.reserve(parameters_->size());
    for (auto& parameter : parameters_.value()) {
      llcpp_parameters_.push_back(parameter.to_llcpp());
    }

    return fidl::unowned_vec(llcpp_parameters_);
  }

  void set_parameters(std::vector<Parameter> parameters) { parameters_ = std::move(parameters); }

 private:
  std::vector<fuchsia_tee::wire::Parameter> llcpp_parameters_;

  std::optional<std::vector<Parameter>> parameters_;
};

struct UuidEqualityComparator {
  bool operator()(const fuchsia_tee::wire::Uuid& lhs, const fuchsia_tee::wire::Uuid& rhs) const {
    if (lhs.time_low != rhs.time_low) {
      return false;
    }
    if (lhs.time_mid != rhs.time_mid) {
      return false;
    }
    if (lhs.time_hi_and_version != rhs.time_hi_and_version) {
      return false;
    }

    if (!std::equal(lhs.clock_seq_and_node.begin(), lhs.clock_seq_and_node.end(),
                    rhs.clock_seq_and_node.begin(), rhs.clock_seq_and_node.end())) {
      return false;
    }

    return true;
  }
};

using UuidToChannelContainer = std::vector<std::pair<fuchsia_tee::wire::Uuid, zx::channel>>;

constexpr std::string_view kServiceDirectoryPath("/svc/");
constexpr std::string_view kDeviceInfoServicePath("/svc/fuchsia.tee.DeviceInfo");

// Presently only used by clients that need to connect before the service is available / don't need
// the TEE to be able to use file services.
constexpr std::string_view kTeeDevClass("/dev/class/tee/");

std::string GetApplicationServicePath(const fuchsia_tee::wire::Uuid& app_uuid) {
  constexpr std::string_view kApplicationServicePathPrefix = "/svc/fuchsia.tee.Application.";
  constexpr size_t kUuidNameLength = 36;
  constexpr const char* kPathFormat = "%s%08x-%04x-%04x-%02x%02x-%02x%02x%02x%02x%02x%02x";
  constexpr size_t kPathLength = kApplicationServicePathPrefix.size() + kUuidNameLength;

  // Reserve an extra spot for the null-terminating character.
  char path_buf[kPathLength + 1];
  snprintf(path_buf, kPathLength + 1, kPathFormat, kApplicationServicePathPrefix.data(),
           app_uuid.time_low, app_uuid.time_mid, app_uuid.time_hi_and_version,
           app_uuid.clock_seq_and_node[0], app_uuid.clock_seq_and_node[1],
           app_uuid.clock_seq_and_node[2], app_uuid.clock_seq_and_node[3],
           app_uuid.clock_seq_and_node[4], app_uuid.clock_seq_and_node[5],
           app_uuid.clock_seq_and_node[6], app_uuid.clock_seq_and_node[7]);

  return std::string(path_buf, kPathLength);
}

constexpr uint32_t GetParamTypeForIndex(uint32_t param_types, size_t index) {
  constexpr uint32_t kBitsPerParamType = 4;
  return ((param_types >> (index * kBitsPerParamType)) & 0xF);
}

constexpr bool IsSharedMemFlagInOut(uint32_t flags) {
  constexpr uint32_t kInOutFlags = TEEC_MEM_INPUT | TEEC_MEM_OUTPUT;
  return (flags & kInOutFlags) == kInOutFlags;
}

constexpr bool IsDirectionInput(fuchsia_tee::wire::Direction direction) {
  return ((direction == fuchsia_tee::wire::Direction::INPUT) ||
          (direction == fuchsia_tee::wire::Direction::INOUT));
}

constexpr bool IsDirectionOutput(fuchsia_tee::wire::Direction direction) {
  return ((direction == fuchsia_tee::wire::Direction::OUTPUT) ||
          (direction == fuchsia_tee::wire::Direction::INOUT));
}

TEEC_Result CheckGlobalPlatformCompliance(zx::unowned_channel device_connector_channel) {
  zx::channel device_info_client_end;
  zx::channel device_info_server_end;
  if (zx_status_t status =
          zx::channel::create(0 /* flags */, &device_info_client_end, &device_info_server_end);
      status != ZX_OK) {
    return TEEC_ERROR_COMMUNICATION;
  }

  if (device_connector_channel->is_valid()) {
    auto result = fuchsia_hardware_tee::DeviceConnector::Call::ConnectToDeviceInfo(
        std::move(device_connector_channel), std::move(device_info_server_end));
    if (!result.ok()) {
      return TEEC_ERROR_NOT_SUPPORTED;
    }
  } else {
    if (zx_status_t status =
            fdio_service_connect(kDeviceInfoServicePath.data(), device_info_server_end.release());
        status != ZX_OK) {
      return TEEC_ERROR_NOT_SUPPORTED;
    }
  }

  auto result =
      fuchsia_tee::DeviceInfo::Call::GetOsInfo(zx::unowned_channel(device_info_client_end));
  if (!result.ok() || !result->info.has_is_global_platform_compliant() ||
      !result->info.is_global_platform_compliant()) {
    return TEEC_ERROR_NOT_SUPPORTED;
  }

  return TEEC_SUCCESS;
}

void ConvertTeecUuidToZxUuid(const TEEC_UUID& teec_uuid, fuchsia_tee::wire::Uuid* out_uuid) {
  ZX_DEBUG_ASSERT(out_uuid);

  out_uuid->time_low = teec_uuid.timeLow;
  out_uuid->time_mid = teec_uuid.timeMid;
  out_uuid->time_hi_and_version = teec_uuid.timeHiAndVersion;

  std::memcpy(out_uuid->clock_seq_and_node.data(), teec_uuid.clockSeqAndNode,
              sizeof(out_uuid->clock_seq_and_node));
}

constexpr TEEC_Result ConvertStatusToResult(zx_status_t status) {
  switch (status) {
    case ZX_ERR_PEER_CLOSED:
      return TEEC_ERROR_COMMUNICATION;
    case ZX_ERR_INVALID_ARGS:
      return TEEC_ERROR_BAD_PARAMETERS;
    case ZX_ERR_NOT_SUPPORTED:
      return TEEC_ERROR_NOT_SUPPORTED;
    case ZX_ERR_NO_MEMORY:
      return TEEC_ERROR_OUT_OF_MEMORY;
    case ZX_OK:
      return TEEC_SUCCESS;
  }
  return TEEC_ERROR_GENERIC;
}

constexpr uint32_t ConvertZxToTeecReturnOrigin(fuchsia_tee::wire::ReturnOrigin return_origin) {
  switch (return_origin) {
    case fuchsia_tee::wire::ReturnOrigin::COMMUNICATION:
      return TEEC_ORIGIN_COMMS;
    case fuchsia_tee::wire::ReturnOrigin::TRUSTED_OS:
      return TEEC_ORIGIN_TEE;
    case fuchsia_tee::wire::ReturnOrigin::TRUSTED_APPLICATION:
      return TEEC_ORIGIN_TRUSTED_APP;
    default:
      return TEEC_ORIGIN_API;
  }
}

constexpr size_t CountOperationParameters(const TEEC_Operation& operation) {
  // Find the highest-indexed non-none parameter.
  for (size_t param_num = static_cast<size_t>(TEEC_NUM_PARAMS_MAX); param_num != 0; param_num--) {
    uint32_t param_type = GetParamTypeForIndex(operation.paramTypes, param_num - 1);
    if (param_type != TEEC_NONE) {
      return param_num;
    }
  }

  return 0;
}

zx_status_t CreateVmoWithName(uint64_t size, uint32_t options, std::string_view name,
                              zx::vmo* result) {
  ZX_DEBUG_ASSERT(result);

  zx::vmo vmo;
  zx_status_t s = zx::vmo::create(size, options, &vmo);
  if (s != ZX_OK) {
    return s;
  }

  s = vmo.set_property(ZX_PROP_NAME, name.data(), name.size());
  if (s != ZX_OK) {
    return s;
  }
  *result = std::move(vmo);
  return s;
}

void PreprocessValue(uint32_t param_type, const TEEC_Value& teec_value, Parameter* out_parameter) {
  ZX_DEBUG_ASSERT(out_parameter);

  fuchsia_tee::wire::Direction direction;
  switch (param_type) {
    case TEEC_VALUE_INPUT:
      direction = fuchsia_tee::wire::Direction::INPUT;
      break;
    case TEEC_VALUE_OUTPUT:
      direction = fuchsia_tee::wire::Direction::OUTPUT;
      break;
    case TEEC_VALUE_INOUT:
      direction = fuchsia_tee::wire::Direction::INOUT;
      break;
    default:
      ZX_PANIC("Unknown param type");
  }

  Value value;
  value.set_direction(direction);
  if (IsDirectionInput(direction)) {
    // The TEEC_Value type only includes two generic fields, whereas the Fuchsia TEE interface
    // supports three. The c field cannot be used by the TEE Client API.
    value.set_a(teec_value.a);
    value.set_b(teec_value.b);
  }

  out_parameter->set_value(std::move(value));
}

TEEC_Result PreprocessTemporaryMemref(uint32_t param_type,
                                      const TEEC_TempMemoryReference& temp_memory_ref,
                                      Parameter* out_parameter) {
  ZX_DEBUG_ASSERT(out_parameter);

  fuchsia_tee::wire::Direction direction;
  switch (param_type) {
    case TEEC_MEMREF_TEMP_INPUT:
      direction = fuchsia_tee::wire::Direction::INPUT;
      break;
    case TEEC_MEMREF_TEMP_OUTPUT:
      direction = fuchsia_tee::wire::Direction::OUTPUT;
      break;
    case TEEC_MEMREF_TEMP_INOUT:
      direction = fuchsia_tee::wire::Direction::INOUT;
      break;
    default:
      ZX_PANIC("TEE Client API Unknown parameter type\n");
  }

  zx::vmo vmo;

  if (temp_memory_ref.buffer) {
    // We either have data to input or have a buffer to output data to, so create a VMO for it.
    zx_status_t status = CreateVmoWithName(temp_memory_ref.size, 0, "teec_temp_memory", &vmo);
    if (status != ZX_OK) {
      return ConvertStatusToResult(status);
    }

    // If the memory reference is used as an input, then we must copy the data from the user
    // provided buffer into the VMO. There is no need to do this for parameters that are output
    // only.
    if (IsDirectionInput(direction)) {
      status = vmo.write(temp_memory_ref.buffer, 0, temp_memory_ref.size);
      if (status != ZX_OK) {
        return ConvertStatusToResult(status);
      }
    }
  }

  Buffer buffer;
  buffer.set_direction(direction);
  if (vmo.is_valid()) {
    buffer.set_vmo(std::move(vmo));
  }
  buffer.set_offset(0);
  buffer.set_size(temp_memory_ref.size);

  out_parameter->set_buffer(std::move(buffer));
  return TEEC_SUCCESS;
}

TEEC_Result PreprocessWholeMemref(const TEEC_RegisteredMemoryReference& memory_ref,
                                  Parameter* out_parameter) {
  ZX_DEBUG_ASSERT(out_parameter);

  if (!memory_ref.parent) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  TEEC_SharedMemory* shared_mem = memory_ref.parent;
  fuchsia_tee::wire::Direction direction;
  if (IsSharedMemFlagInOut(shared_mem->flags)) {
    direction = fuchsia_tee::wire::Direction::INOUT;
  } else if (shared_mem->flags & TEEC_MEM_INPUT) {
    direction = fuchsia_tee::wire::Direction::INPUT;
  } else if (shared_mem->flags & TEEC_MEM_OUTPUT) {
    direction = fuchsia_tee::wire::Direction::OUTPUT;
  } else {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  zx::vmo vmo;
  zx_status_t status = zx::unowned_vmo(shared_mem->imp.vmo)->duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo);
  if (status != ZX_OK) {
    return ConvertStatusToResult(status);
  }

  Buffer buffer;
  buffer.set_direction(direction);
  buffer.set_vmo(std::move(vmo));
  buffer.set_offset(0);
  buffer.set_size(shared_mem->size);

  out_parameter->set_buffer(std::move(buffer));
  return TEEC_SUCCESS;
}

TEEC_Result PreprocessPartialMemref(uint32_t param_type,
                                    const TEEC_RegisteredMemoryReference& memory_ref,
                                    Parameter* out_parameter) {
  ZX_DEBUG_ASSERT(out_parameter);

  if (!memory_ref.parent) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  uint32_t expected_shm_flags = 0;
  fuchsia_tee::wire::Direction direction;
  switch (param_type) {
    case TEEC_MEMREF_PARTIAL_INPUT:
      expected_shm_flags = TEEC_MEM_INPUT;
      direction = fuchsia_tee::wire::Direction::INPUT;
      break;
    case TEEC_MEMREF_PARTIAL_OUTPUT:
      expected_shm_flags = TEEC_MEM_OUTPUT;
      direction = fuchsia_tee::wire::Direction::OUTPUT;
      break;
    case TEEC_MEMREF_PARTIAL_INOUT:
      expected_shm_flags = TEEC_MEM_INPUT | TEEC_MEM_OUTPUT;
      direction = fuchsia_tee::wire::Direction::INOUT;
      break;
    default:
      ZX_DEBUG_ASSERT(param_type == TEEC_MEMREF_PARTIAL_INPUT ||
                      param_type == TEEC_MEMREF_PARTIAL_OUTPUT ||
                      param_type == TEEC_MEMREF_PARTIAL_INOUT);
  }

  TEEC_SharedMemory* shared_mem = memory_ref.parent;

  if ((shared_mem->flags & expected_shm_flags) != expected_shm_flags) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  zx::vmo vmo;
  zx_status_t status = zx::unowned_vmo(shared_mem->imp.vmo)->duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo);
  if (status != ZX_OK) {
    return ConvertStatusToResult(status);
  }

  Buffer buffer;
  buffer.set_direction(direction);
  buffer.set_vmo(std::move(vmo));
  buffer.set_offset(memory_ref.offset);
  buffer.set_size(memory_ref.size);

  out_parameter->set_buffer(std::move(buffer));
  return TEEC_SUCCESS;
}

TEEC_Result PreprocessOperation(const TEEC_Operation* operation, ParameterSet* out_parameter_set) {
  ZX_DEBUG_ASSERT(out_parameter_set);
  std::vector<Parameter> parameters;

  if (!operation) {
    out_parameter_set->set_parameters(std::move(parameters));
    return TEEC_SUCCESS;
  }

  size_t num_params = CountOperationParameters(*operation);
  parameters.reserve(num_params);

  TEEC_Result rc = TEEC_SUCCESS;
  for (size_t i = 0; i < num_params; i++) {
    uint32_t param_type = GetParamTypeForIndex(operation->paramTypes, i);
    Parameter parameter;

    switch (param_type) {
      case TEEC_NONE:
        parameter.set_none();
        break;
      case TEEC_VALUE_INPUT:
      case TEEC_VALUE_OUTPUT:
      case TEEC_VALUE_INOUT:
        PreprocessValue(param_type, operation->params[i].value, &parameter);
        break;
      case TEEC_MEMREF_TEMP_INPUT:
      case TEEC_MEMREF_TEMP_OUTPUT:
      case TEEC_MEMREF_TEMP_INOUT:
        rc = PreprocessTemporaryMemref(param_type, operation->params[i].tmpref, &parameter);
        break;
      case TEEC_MEMREF_WHOLE:
        rc = PreprocessWholeMemref(operation->params[i].memref, &parameter);
        break;
      case TEEC_MEMREF_PARTIAL_INPUT:
      case TEEC_MEMREF_PARTIAL_OUTPUT:
      case TEEC_MEMREF_PARTIAL_INOUT:
        rc = PreprocessPartialMemref(param_type, operation->params[i].memref, &parameter);
        break;
      default:
        rc = TEEC_ERROR_BAD_PARAMETERS;
        break;
    }

    if (rc != TEEC_SUCCESS) {
      return rc;
    }

    parameters.push_back(std::move(parameter));
  }

  out_parameter_set->set_parameters(std::move(parameters));
  return rc;
}

TEEC_Result PostprocessValue(uint32_t param_type, const fuchsia_tee::wire::Parameter& zx_param,
                             TEEC_Value* out_teec_value) {
  ZX_DEBUG_ASSERT(out_teec_value);
  ZX_DEBUG_ASSERT(param_type == TEEC_VALUE_INPUT || param_type == TEEC_VALUE_OUTPUT ||
                  param_type == TEEC_VALUE_INOUT);

  if (zx_param.which() != fuchsia_tee::wire::Parameter::Tag::kValue) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  const fuchsia_tee::wire::Value& zx_value = zx_param.value();
  if (!zx_value.has_direction()) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  // Validate that the direction of the returned parameter matches the expected.
  if ((param_type == TEEC_VALUE_INPUT) &&
      (zx_value.direction() != fuchsia_tee::wire::Direction::INPUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_VALUE_OUTPUT) &&
      (zx_value.direction() != fuchsia_tee::wire::Direction::OUTPUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_VALUE_INOUT) &&
      (zx_value.direction() != fuchsia_tee::wire::Direction::INOUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (IsDirectionOutput(zx_value.direction())) {
    if (!zx_value.has_a() || !zx_value.has_b()) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }

    // The TEEC_Value type only includes two generic fields, whereas the Fuchsia TEE interface
    // supports three. The c field cannot be used by the TEE Client API.
    out_teec_value->a = static_cast<uint32_t>(zx_value.a());
    out_teec_value->b = static_cast<uint32_t>(zx_value.b());
  }
  return TEEC_SUCCESS;
}

TEEC_Result PostprocessTemporaryMemref(uint32_t param_type,
                                       const fuchsia_tee::wire::Parameter& zx_param,
                                       TEEC_TempMemoryReference* out_temp_memory_ref) {
  ZX_DEBUG_ASSERT(out_temp_memory_ref);
  ZX_DEBUG_ASSERT(param_type == TEEC_MEMREF_TEMP_INPUT || param_type == TEEC_MEMREF_TEMP_OUTPUT ||
                  param_type == TEEC_MEMREF_TEMP_INOUT);

  if (zx_param.which() != fuchsia_tee::wire::Parameter::Tag::kBuffer) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  const fuchsia_tee::wire::Buffer& zx_buffer = zx_param.buffer();
  if (!zx_buffer.has_direction()) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if ((param_type == TEEC_MEMREF_TEMP_INPUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::INPUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_MEMREF_TEMP_OUTPUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::OUTPUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_MEMREF_TEMP_INOUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::INOUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  TEEC_Result rc = TEEC_SUCCESS;
  if (IsDirectionOutput(zx_buffer.direction())) {
    // For output buffers, if we don't have enough space in the temporary memory reference to
    // copy the data out, we still need to update the size to indicate to the user how large of
    // a buffer they need to perform the requested operation.
    if (!zx_buffer.has_size()) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }

    if (out_temp_memory_ref->buffer && out_temp_memory_ref->size >= zx_buffer.size()) {
      if (!zx_buffer.has_offset() || !zx_buffer.has_vmo()) {
        return TEEC_ERROR_BAD_PARAMETERS;
      }

      zx_status_t status =
          zx_buffer.vmo().read(out_temp_memory_ref->buffer, zx_buffer.offset(), zx_buffer.size());
      rc = ConvertStatusToResult(status);
    }
    out_temp_memory_ref->size = zx_buffer.size();
  }

  return rc;
}

TEEC_Result PostprocessWholeMemref(const fuchsia_tee::wire::Parameter& zx_param,
                                   TEEC_RegisteredMemoryReference* out_memory_ref) {
  ZX_DEBUG_ASSERT(out_memory_ref);
  ZX_DEBUG_ASSERT(out_memory_ref->parent);

  if (zx_param.which() != fuchsia_tee::wire::Parameter::Tag::kBuffer) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  const fuchsia_tee::wire::Buffer& zx_buffer = zx_param.buffer();
  if (!zx_buffer.has_direction()) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (IsDirectionOutput(zx_buffer.direction())) {
    if (!zx_buffer.has_size()) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }
    out_memory_ref->size = zx_buffer.size();
  }

  return TEEC_SUCCESS;
}

TEEC_Result PostprocessPartialMemref(uint32_t param_type,
                                     const fuchsia_tee::wire::Parameter& zx_param,
                                     TEEC_RegisteredMemoryReference* out_memory_ref) {
  ZX_DEBUG_ASSERT(out_memory_ref);
  ZX_DEBUG_ASSERT(param_type == TEEC_MEMREF_PARTIAL_INPUT ||
                  param_type == TEEC_MEMREF_PARTIAL_OUTPUT ||
                  param_type == TEEC_MEMREF_PARTIAL_INOUT);

  if (zx_param.which() != fuchsia_tee::wire::Parameter::Tag::kBuffer) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  const fuchsia_tee::wire::Buffer& zx_buffer = zx_param.buffer();
  if (!zx_buffer.has_direction()) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if ((param_type == TEEC_MEMREF_PARTIAL_INPUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::INPUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_MEMREF_PARTIAL_OUTPUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::OUTPUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_MEMREF_PARTIAL_INOUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::INOUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (IsDirectionOutput(zx_buffer.direction())) {
    if (!zx_buffer.has_size()) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }
    out_memory_ref->size = zx_buffer.size();
  }

  return TEEC_SUCCESS;
}

TEEC_Result PostprocessOperation(
    const fidl::VectorView<fuchsia_tee::wire::Parameter>& parameter_set,
    TEEC_Operation* out_operation) {
  if (!out_operation) {
    return TEEC_SUCCESS;
  }

  size_t num_params = CountOperationParameters(*out_operation);

  TEEC_Result rc = TEEC_SUCCESS;
  for (size_t i = 0; i < num_params; i++) {
    uint32_t param_type = GetParamTypeForIndex(out_operation->paramTypes, i);

    // This check catches the case where we did not receive all the parameters we expected.
    if (i >= parameter_set.count() && param_type != TEEC_NONE) {
      rc = TEEC_ERROR_BAD_PARAMETERS;
      break;
    }

    switch (param_type) {
      case TEEC_NONE:
        if (parameter_set[i].which() != fuchsia_tee::wire::Parameter::Tag::kNone) {
          rc = TEEC_ERROR_BAD_PARAMETERS;
        }
        break;
      case TEEC_VALUE_INPUT:
      case TEEC_VALUE_OUTPUT:
      case TEEC_VALUE_INOUT:
        rc = PostprocessValue(param_type, parameter_set[i], &out_operation->params[i].value);
        break;
      case TEEC_MEMREF_TEMP_INPUT:
      case TEEC_MEMREF_TEMP_OUTPUT:
      case TEEC_MEMREF_TEMP_INOUT:
        rc = PostprocessTemporaryMemref(param_type, parameter_set[i],
                                        &out_operation->params[i].tmpref);
        break;
      case TEEC_MEMREF_WHOLE:
        rc = PostprocessWholeMemref(parameter_set[i], &out_operation->params[i].memref);
        break;
      case TEEC_MEMREF_PARTIAL_INPUT:
      case TEEC_MEMREF_PARTIAL_OUTPUT:
      case TEEC_MEMREF_PARTIAL_INOUT:
        rc = PostprocessPartialMemref(param_type, parameter_set[i],
                                      &out_operation->params[i].memref);
        break;
      default:
        rc = TEEC_ERROR_BAD_PARAMETERS;
    }

    if (rc != TEEC_SUCCESS) {
      break;
    }
  }

  // This check catches the case where we received more parameters than we expected.
  for (size_t i = num_params; i < parameter_set.count(); i++) {
    if (parameter_set[i].which() != fuchsia_tee::wire::Parameter::Tag::kNone) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }
  }

  return rc;
}

UuidToChannelContainer* GetUuidToChannelContainerFromContext(TEEC_Context* context) {
  ZX_DEBUG_ASSERT(context);
  return reinterpret_cast<UuidToChannelContainer*>(context->imp.uuid_to_channel);
}

UuidToChannelContainer::iterator FindInUuidToChannelContainer(UuidToChannelContainer* container,
                                                              const fuchsia_tee::wire::Uuid& uuid) {
  ZX_DEBUG_ASSERT(container);

  return std::find_if(container->begin(), container->end(), [&](const auto& uuid_channel_pair) {
    return UuidEqualityComparator{}(uuid_channel_pair.first, uuid);
  });
}

constexpr bool ShouldUseDeviceConnectorChannel(const TEEC_Context* context) {
  return context->imp.device_connector_channel != ZX_HANDLE_INVALID;
}

// Connects the client directly to the TEE Driver's DeviceConnector interface.
//
// This is a temporary measure to allow clients that come up before component services to still
// access the TEE. This requires that the client has access to the TEE device class. Additionally,
// the client's entire context will not have any filesystem support, so if the client opens a
// session and sends a command to a trusted application that then needs persistent storage to
// complete, the persistent storage request will be rejected by the driver.
zx_status_t ConnectToDeviceConnector(const char* tee_device, zx::channel* connector_channel) {
  ZX_DEBUG_ASSERT(tee_device);
  ZX_DEBUG_ASSERT(connector_channel);

  int fd = open(tee_device, O_RDWR);
  if (fd < 0) {
    return ZX_ERR_NOT_FOUND;
  }

  zx::channel temp_channel;
  zx_status_t status = fdio_get_service_handle(fd, temp_channel.reset_and_get_address());
  if (status != ZX_OK) {
    return status;
  }

  *connector_channel = std::move(temp_channel);
  return ZX_OK;
}

// Opens a connection to a `fuchsia.tee.Application` via a device connector.
TEEC_Result ConnectApplicationViaDeviceConnector(const fuchsia_tee::wire::Uuid& app_uuid,
                                                 zx::unowned_channel device_connector_channel,
                                                 zx::channel* out_app_channel) {
  ZX_DEBUG_ASSERT(device_connector_channel->is_valid());
  ZX_DEBUG_ASSERT(out_app_channel);

  zx::channel app_client_end;
  zx::channel app_server_end;
  if (zx_status_t status = zx::channel::create(0 /* flags */, &app_client_end, &app_server_end);
      status != ZX_OK) {
    return TEEC_ERROR_COMMUNICATION;
  }

  auto result = fuchsia_hardware_tee::DeviceConnector::Call::ConnectToApplication(
      std::move(device_connector_channel), app_uuid,
      zx::channel(ZX_HANDLE_INVALID) /* service_provider */, std::move(app_server_end));
  if (!result.ok()) {
    return TEEC_ERROR_COMMUNICATION;
  }

  *out_app_channel = std::move(app_client_end);
  return TEEC_SUCCESS;
}

// Opens a connection to a `fuchsia.tee.Application` via the service.
TEEC_Result ConnectApplicationViaService(const fuchsia_tee::wire::Uuid& app_uuid,
                                         zx::channel* out_app_channel) {
  ZX_DEBUG_ASSERT(out_app_channel);

  zx::channel app_client_end;
  zx::channel app_server_end;
  if (zx_status_t status = zx::channel::create(0 /* flags */, &app_client_end, &app_server_end);
      status != ZX_OK) {
    return TEEC_ERROR_COMMUNICATION;
  }

  std::string service_path = GetApplicationServicePath(app_uuid);
  if (zx_status_t status = fdio_service_connect(service_path.c_str(), app_server_end.release());
      status != ZX_OK) {
    return TEEC_ERROR_COMMUNICATION;
  }

  *out_app_channel = std::move(app_client_end);
  return TEEC_SUCCESS;
}

TEEC_Result ConnectApplication(const fuchsia_tee::wire::Uuid& app_uuid, TEEC_Context* context,
                               zx::unowned_channel* out_app_channel) {
  ZX_DEBUG_ASSERT(context);
  ZX_DEBUG_ASSERT(out_app_channel);

  UuidToChannelContainer* uuid_to_channel = GetUuidToChannelContainerFromContext(context);
  if (uuid_to_channel == nullptr) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (auto iter = FindInUuidToChannelContainer(uuid_to_channel, app_uuid);
      iter != uuid_to_channel->end()) {
    // A connection to this application already exists, so just reuse the channel.
    *out_app_channel = zx::unowned_channel(iter->second);
    return TEEC_SUCCESS;
  }

  // This is a new connection to this application, so a new connection must be made.
  zx::channel app_channel_owned;
  TEEC_Result result =
      ShouldUseDeviceConnectorChannel(context)
          ? ConnectApplicationViaDeviceConnector(
                app_uuid, zx::unowned_channel(context->imp.device_connector_channel),
                &app_channel_owned)
          : ConnectApplicationViaService(app_uuid, &app_channel_owned);
  if (result != TEEC_SUCCESS) {
    return result;
  }

  *out_app_channel = zx::unowned_channel(app_channel_owned);

  // Stash the channel into the `uuid_to_channel` for ownership and future use.
  uuid_to_channel->emplace_back(app_uuid, std::move(app_channel_owned));

  return TEEC_SUCCESS;
}

}  // namespace

__EXPORT
TEEC_Result TEEC_InitializeContext(const char* name, TEEC_Context* context) {
  if (!context) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  auto name_view = std::string_view(name != nullptr ? name : "");
  zx::channel device_connector_channel;

  // TODO: use `std::string_view::starts_with()` when C++20 is available.
  if (name == nullptr ||
      name_view.compare(0, kServiceDirectoryPath.size(), kServiceDirectoryPath) == 0) {
    device_connector_channel = zx::channel(ZX_HANDLE_INVALID);
  } else if (name_view.compare(0, kTeeDevClass.size(), kTeeDevClass) == 0) {
    if (zx_status_t status = ConnectToDeviceConnector(name, &device_connector_channel);
        status != ZX_OK) {
      return TEEC_ERROR_COMMUNICATION;
    }
  } else {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (TEEC_Result result =
          CheckGlobalPlatformCompliance(zx::unowned_channel(device_connector_channel));
      result != TEEC_SUCCESS) {
    return result;
  }

  context->imp.device_connector_channel = device_connector_channel.release();
  context->imp.uuid_to_channel = new UuidToChannelContainer();

  return TEEC_SUCCESS;
}

__EXPORT
void TEEC_FinalizeContext(TEEC_Context* context) {
  if (context) {
    zx_handle_close(context->imp.device_connector_channel);
    context->imp.device_connector_channel = ZX_HANDLE_INVALID;

    delete GetUuidToChannelContainerFromContext(context);
    context->imp.uuid_to_channel = nullptr;
  }
}

__EXPORT
TEEC_Result TEEC_RegisterSharedMemory(TEEC_Context* context, TEEC_SharedMemory* sharedMem) {
  /* This function is supposed to register an existing buffer for use as shared memory. We don't
   * have a way of discovering the VMO handle for an arbitrary address, so implementing this would
   * require an extra VMO that would be copied into at invocation. Since we currently don't have
   * any use cases for this function and TEEC_AllocateSharedMemory should be the preferred method
   * of acquiring shared memory, we're going to leave this unimplemented for now. */
  return TEEC_ERROR_NOT_IMPLEMENTED;
}

__EXPORT
TEEC_Result TEEC_AllocateSharedMemory(TEEC_Context* context, TEEC_SharedMemory* sharedMem) {
  if (!context || !sharedMem) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (sharedMem->flags & ~(TEEC_MEM_INPUT | TEEC_MEM_OUTPUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  std::memset(&sharedMem->imp, 0, sizeof(sharedMem->imp));

  size_t size = sharedMem->size;

  zx::vmo vmo;
  zx_status_t status = CreateVmoWithName(size, 0, "teec_shared_memory", &vmo);
  if (status != ZX_OK) {
    return ConvertStatusToResult(status);
  }

  uintptr_t mapped_addr;
  status =
      zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0, size, &mapped_addr);
  if (status != ZX_OK) {
    return ConvertStatusToResult(status);
  }

  sharedMem->buffer = reinterpret_cast<void*>(mapped_addr);
  sharedMem->imp.vmo = vmo.release();
  sharedMem->imp.mapped_addr = mapped_addr;
  sharedMem->imp.mapped_size = size;

  return TEEC_SUCCESS;
}

__EXPORT
void TEEC_ReleaseSharedMemory(TEEC_SharedMemory* sharedMem) {
  if (!sharedMem) {
    return;
  }
  zx::vmar::root_self()->unmap(sharedMem->imp.mapped_addr, sharedMem->imp.mapped_size);
  zx_handle_close(sharedMem->imp.vmo);
  sharedMem->imp.vmo = ZX_HANDLE_INVALID;
}

__EXPORT
TEEC_Result TEEC_OpenSession(TEEC_Context* context, TEEC_Session* session,
                             const TEEC_UUID* destination, uint32_t connectionMethod,
                             const void* connectionData, TEEC_Operation* operation,
                             uint32_t* returnOrigin) {
  if (!context || !session || !destination) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_API;
    }
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (connectionMethod != TEEC_LOGIN_PUBLIC) {
    // TODO(rjascani): Investigate whether non public login is needed.
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_API;
    }
    return TEEC_ERROR_NOT_IMPLEMENTED;
  }

  fuchsia_tee::wire::Uuid app_uuid_fidl;
  ConvertTeecUuidToZxUuid(*destination, &app_uuid_fidl);

  ParameterSet parameter_set;
  TEEC_Result processing_rc = PreprocessOperation(operation, &parameter_set);
  if (processing_rc != TEEC_SUCCESS) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return processing_rc;
  }

  zx::unowned_channel app_channel;
  if (TEEC_Result result = ConnectApplication(app_uuid_fidl, context, &app_channel);
      result != TEEC_SUCCESS) {
    return result;
  }

  auto result = fuchsia_tee::Application::Call::OpenSession2(zx::unowned_channel(app_channel),
                                                             parameter_set.to_llcpp());
  zx_status_t status = result.status();

  if (status != ZX_OK) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }

    if (status == ZX_ERR_PEER_CLOSED) {
      // If the channel has closed, drop the entry from the map, closing the channel end.
      UuidToChannelContainer* uuid_to_channel = GetUuidToChannelContainerFromContext(context);
      if (auto iter = FindInUuidToChannelContainer(uuid_to_channel, app_uuid_fidl);
          iter != uuid_to_channel->end()) {
        uuid_to_channel->erase(iter);
      }
    }

    return ConvertStatusToResult(status);
  }

  uint32_t out_session_id = result->session_id;
  fuchsia_tee::wire::OpResult& out_result = result->op_result;

  if (!out_result.has_return_code() || !out_result.has_return_origin()) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return TEEC_ERROR_COMMUNICATION;
  }

  // Try and run post-processing regardless of TEE operation status. Even if an error occurred,
  // the parameter set may have been updated.
  processing_rc = out_result.has_parameter_set()
                      ? PostprocessOperation(out_result.parameter_set(), operation)
                      : TEEC_ERROR_COMMUNICATION;

  if (out_result.return_code() != TEEC_SUCCESS) {
    // If the TEE operation failed, use that return code above any processing failure codes.
    if (returnOrigin) {
      *returnOrigin = ConvertZxToTeecReturnOrigin(out_result.return_origin());
    }
    return static_cast<uint32_t>(out_result.return_code());
  }
  if (processing_rc != TEEC_SUCCESS) {
    // The TEE operation succeeded but the processing operation failed.
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return processing_rc;
  }

  session->imp.session_id = out_session_id;
  session->imp.application_channel = app_channel->get();

  return static_cast<uint32_t>(out_result.return_code());
}

__EXPORT
void TEEC_CloseSession(TEEC_Session* session) {
  if (!session || session->imp.application_channel == ZX_HANDLE_INVALID) {
    return;
  }

  // TEEC_CloseSession simply swallows errors, so no need to check here.
  fuchsia_tee::Application::Call::CloseSession(
      zx::unowned_channel(session->imp.application_channel), session->imp.session_id);
  session->imp.application_channel = ZX_HANDLE_INVALID;
}

__EXPORT
TEEC_Result TEEC_InvokeCommand(TEEC_Session* session, uint32_t commandID, TEEC_Operation* operation,
                               uint32_t* returnOrigin) {
  if (!session || session->imp.application_channel == ZX_HANDLE_INVALID) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_API;
    }
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  ParameterSet parameter_set;
  TEEC_Result processing_rc = PreprocessOperation(operation, &parameter_set);
  if (processing_rc != TEEC_SUCCESS) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return processing_rc;
  }

  auto result = fuchsia_tee::Application::Call::InvokeCommand(
      zx::unowned_channel(session->imp.application_channel), session->imp.session_id, commandID,
      parameter_set.to_llcpp());
  zx_status_t status = result.status();
  if (status != ZX_OK) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return ConvertStatusToResult(status);
  }

  fuchsia_tee::wire::OpResult& out_result = result->op_result;

  if (!out_result.has_return_code() || !out_result.has_return_origin()) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return TEEC_ERROR_COMMUNICATION;
  }

  // Try and run post-processing regardless of TEE operation status. Even if an error occurred,
  // the parameter set may have been updated.
  processing_rc = out_result.has_parameter_set()
                      ? PostprocessOperation(out_result.parameter_set(), operation)
                      : TEEC_ERROR_COMMUNICATION;

  if (out_result.return_code() != TEEC_SUCCESS) {
    // If the TEE operation failed, use that return code above any processing failure codes.
    if (returnOrigin) {
      *returnOrigin = ConvertZxToTeecReturnOrigin(out_result.return_origin());
    }
    return static_cast<uint32_t>(out_result.return_code());
  }
  if (processing_rc != TEEC_SUCCESS) {
    // The TEE operation succeeded but the processing operation failed.
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return processing_rc;
  }

  return static_cast<uint32_t>(out_result.return_code());
}

__EXPORT
void TEEC_RequestCancellation(TEEC_Operation* operation) {}

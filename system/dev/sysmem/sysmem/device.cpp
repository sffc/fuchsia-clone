// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "device.h"

#include "allocator.h"
#include "buffer_collection_token.h"
#include "macros.h"

#include <ddk/device.h>
#include <ddk/protocol/platform/bus.h>
#include <lib/fidl-async-2/simple_binding.h>
#include <lib/fidl-utils/bind.h>
#include <lib/zx/event.h>
#include <zircon/assert.h>

namespace {

fuchsia_sysmem_DriverConnector_ops_t driver_connector_ops = {
    .Connect = fidl::Binder<Device>::BindMember<&Device::Connect>,
};

zx_status_t sysmem_message(void* device_ctx, fidl_msg_t* msg, fidl_txn_t* txn) {
    return fuchsia_sysmem_DriverConnector_dispatch(device_ctx, txn, msg,
                                                   &driver_connector_ops);
}

// -Werror=missing-field-initializers seems more paranoid than I want here.
zx_protocol_device_t sysmem_device_ops = [] {
    zx_protocol_device_t tmp{};
    tmp.version = DEVICE_OPS_VERSION;
    tmp.message = sysmem_message;
    return tmp;
}();

zx_protocol_device_t out_of_proc_sysmem_protocol_ops = [] {
    zx_protocol_device_t tmp{};
    tmp.version = DEVICE_OPS_VERSION;
    // tmp.message is not used - sysmem_device_ops.message is used for incoming
    // FIDL messages.
    return tmp;
}();

zx_status_t in_proc_sysmem_Connect(void* ctx,
                                   zx_handle_t allocator2_request_param) {
    Device* self = static_cast<Device*>(ctx);
    return self->Connect(allocator2_request_param);
}

// In-proc sysmem interface.  Essentially an in-proc version of
// fuchsia.sysmem.DriverConnector.
sysmem_protocol_ops_t in_proc_sysmem_protocol_ops = {
    .connect = in_proc_sysmem_Connect,
};

// This function is used as a platform_proxy_cb.callback.
void sysmem_proxy_cb(void* ctx, const void* req_buffer, size_t req_size,
                     const zx_handle_t* req_handle_list,
                     size_t req_handle_count, void* out_resp_buffer,
                     size_t resp_size, size_t* out_resp_actual,
                     zx_handle_t* out_resp_handle_list,
                     size_t resp_handle_count, size_t* out_resp_handle_actual) {
    ZX_DEBUG_ASSERT(false && "not yet implemented");
}

} // namespace

Device::Device(zx_device_t* parent_device, Driver* parent_driver)
    : parent_device_(parent_device),
      parent_driver_(parent_driver), in_proc_sysmem_protocol_{
                                         .ops = &in_proc_sysmem_protocol_ops,
                                         .ctx = this} {
    ZX_DEBUG_ASSERT(parent_device_);
    ZX_DEBUG_ASSERT(parent_driver_);
}

zx_status_t Device::Bind() {
    zx_status_t status =
        device_get_protocol(parent_device_, ZX_PROTOCOL_PDEV, &pdev_);
    if (status != ZX_OK) {
        DRIVER_ERROR(
            "Failed device_get_protocol() ZX_PROTOCOL_PDEV - status: %d",
            status);
        return status;
    }

    pdev_device_info_t info;
    status = pdev_get_device_info(&pdev_, &info);
    if (status != ZX_OK) {
        DRIVER_ERROR("pdev_get_device_info() failed");
        return status;
    }
    pdev_device_info_vid_ = info.vid;
    pdev_device_info_pid_ = info.pid;

    status = pdev_get_bti(&pdev_, 0, bti_.reset_and_get_address());
    if (status != ZX_OK) {
        DRIVER_ERROR("Failed pdev_get_bti() - status: %d", status);
        return status;
    }

    pbus_protocol_t pbus;
    status = device_get_protocol(parent_device_, ZX_PROTOCOL_PBUS, &pbus);
    if (status != ZX_OK) {
        DRIVER_ERROR("ZX_PROTOCOL_PBUS not available %d \n", status);
        return status;
    }

    device_add_args_t device_add_args = {};
    device_add_args.version = DEVICE_ADD_ARGS_VERSION;
    device_add_args.name = "sysmem";
    device_add_args.ctx = this;
    device_add_args.ops = &sysmem_device_ops;

    // ZX_PROTOCOL_SYSMEM causes /dev/class/sysmem to get created, and flags
    // support for the fuchsia.sysmem.DriverConnector protocol.  The .message
    // callback used is sysmem_device_ops.message, not
    // sysmem_protocol_ops.message.
    device_add_args.proto_id = ZX_PROTOCOL_SYSMEM;
    device_add_args.proto_ops = &out_of_proc_sysmem_protocol_ops;
    device_add_args.flags = DEVICE_ADD_INVISIBLE;

    status = device_add(parent_device_, &device_add_args, &device_);
    if (status != ZX_OK) {
        DRIVER_ERROR("Failed to bind device");
        return status;
    }

    // Register the sysmem protocol with the platform bus.
    //
    // This is essentially the in-proc version of
    // fuchsia.sysmem.DriverConnector.
    //
    // We should only pbus_register_protocol() if device_add() succeeded, but if
    // pbus_register_protocol() fails, we should remove the device without it
    // ever being visible.
    const platform_proxy_cb_t callback = {sysmem_proxy_cb, this};
    status = pbus_register_protocol(
        &pbus, ZX_PROTOCOL_SYSMEM, &in_proc_sysmem_protocol_,
        sizeof(in_proc_sysmem_protocol_), &callback);
    if (status != ZX_OK) {
        zx_status_t remove_status = device_remove(device_);
        // If this failed, we're potentially leaving the device invisible in a
        // --release build, which is about the best we can do if removing fails.
        // Of course, remove shouldn't fail in the first place.
        ZX_DEBUG_ASSERT(remove_status == ZX_OK);
        return status;
    }

    // We only do this if Bind() fully worked.  Else we don't want any client
    // to be able to see the device.  This call returns void, thankfully.
    device_make_visible(device_);

    return ZX_OK;
}

zx_status_t Device::Connect(zx_handle_t allocator_request) {
    zx::channel local_allocator_request(allocator_request);
    // The Allocator is channel-owned / self-owned.
    Allocator::CreateChannelOwned(std::move(local_allocator_request), this);
    return ZX_OK;
}

const zx::bti& Device::bti() {
    return bti_;
}

uint32_t Device::pdev_device_info_vid() {
    ZX_DEBUG_ASSERT(pdev_device_info_vid_ !=
                    std::numeric_limits<uint32_t>::max());
    return pdev_device_info_vid_;
}

uint32_t Device::pdev_device_info_pid() {
    ZX_DEBUG_ASSERT(pdev_device_info_pid_ !=
                    std::numeric_limits<uint32_t>::max());
    return pdev_device_info_pid_;
}

void Device::TrackToken(BufferCollectionToken* token) {
    zx_koid_t server_koid = token->server_koid();
    ZX_DEBUG_ASSERT(server_koid != ZX_KOID_INVALID);
    ZX_DEBUG_ASSERT(tokens_by_koid_.find(server_koid) == tokens_by_koid_.end());
    tokens_by_koid_.insert({server_koid, token});
}

void Device::UntrackToken(BufferCollectionToken* token) {
    zx_koid_t server_koid = token->server_koid();
    if (server_koid == ZX_KOID_INVALID) {
        // The caller is allowed to un-track a token that never saw
        // SetServerKoid().
        return;
    }
    auto iter = tokens_by_koid_.find(server_koid);
    ZX_DEBUG_ASSERT(iter != tokens_by_koid_.end());
    tokens_by_koid_.erase(iter);
}

BufferCollectionToken* Device::FindTokenByServerChannelKoid(
    zx_koid_t token_server_koid) {
    auto iter = tokens_by_koid_.find(token_server_koid);
    if (iter == tokens_by_koid_.end()) {
        return nullptr;
    }
    return iter->second;
}

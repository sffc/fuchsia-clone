// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "bt_transport_usb.h"

#include <assert.h>
#include <fuchsia/hardware/bt/hci/c/banjo.h>
#include <fuchsia/hardware/usb/c/banjo.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/fit/defer.h>
#include <lib/sync/completion.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <threads.h>
#include <unistd.h>
#include <zircon/device/bt-hci.h>
#include <zircon/status.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <usb/usb-request.h>
#include <usb/usb.h>

#include "src/connectivity/bluetooth/hci/transport/usb/bt_transport_usb_bind.h"
#include "src/lib/listnode/listnode.h"

#define EVENT_REQ_COUNT 8
#define ACL_READ_REQ_COUNT 8
#define ACL_WRITE_REQ_COUNT 8

// The maximum HCI ACL frame size used for data transactions
#define ACL_MAX_FRAME_SIZE 1028  // (1024 + 4 bytes for the ACL header)

#define CMD_BUF_SIZE 255 + 3    // 3 byte header + payload
#define EVENT_BUF_SIZE 255 + 2  // 2 byte header + payload

// TODO(fxbug.dev/90072): move these to hw/usb.h (or hw/bluetooth.h if that exists)
#define USB_SUBCLASS_BLUETOOTH 1
#define USB_PROTOCOL_BLUETOOTH 1

namespace bt_transport_usb {

namespace {

// Endpoints: interrupt, bulk in, bulk out
constexpr uint8_t kNumInterface0Endpoints = 3;
// Endpoints: isoc in, isoc out
constexpr uint8_t kNumInterface1Endpoints = 2;

constexpr uint8_t kIsocInterfaceNum = 1;
constexpr uint8_t kIsocAltSettingInactive = 0;

constexpr uint8_t kScoWriteReqCount = 8;
constexpr uint16_t kScoMaxFrameSize = 255 + 3;  // payload + 3 bytes header

struct HciEventHeader {
  uint8_t event_code;
  uint8_t parameter_total_size;
} __PACKED;

}  // namespace

zx_status_t Device::Create(void* ctx, zx_device_t* parent) {
  std::unique_ptr<Device> dev = std::make_unique<Device>(parent);

  zx_status_t bind_status = dev->Bind();
  if (bind_status != ZX_OK) {
    zxlogf(ERROR, "bt-transport-usb: failed to bind: %s", zx_status_get_string(bind_status));
    return bind_status;
  }

  // Driver Manager is now in charge of the device.
  // Memory will be explicitly freed in DdkUnbind().
  __UNUSED Device* unused = dev.release();
  return ZX_OK;
}

zx_status_t Device::Bind() {
  zxlogf(DEBUG, "%s", __FUNCTION__);

  mtx_init(&mutex_, mtx_plain);
  mtx_init(&pending_request_lock_, mtx_plain);

  usb_protocol_t usb;
  zx_status_t status = device_get_protocol(parent(), ZX_PROTOCOL_USB, &usb);
  if (status != ZX_OK) {
    zxlogf(ERROR, "bt-transport-usb: get protocol failed: %s", zx_status_get_string(status));
    return status;
  }
  memcpy(&usb_, &usb, sizeof(usb));

  // Get the configuration descriptor, which contains interface descriptors.
  usb_desc_iter_t config_desc_iter;
  zx_status_t result = usb_desc_iter_init(&usb, &config_desc_iter);
  if (result < 0) {
    zxlogf(ERROR, "bt-transport-usb: failed to get usb configuration descriptor: %s",
           zx_status_get_string(status));
    return result;
  }
  auto config_desc_release =
      fit::defer([&config_desc_iter] { usb_desc_iter_release(&config_desc_iter); });

  // Interface 0 should contain the interrupt, bulk in, and bulk out endpoints.
  // See Core Spec v5.3, Vol 4, Part B, Sec 2.1.1.
  usb_interface_descriptor_t* intf =
      usb_desc_iter_next_interface(&config_desc_iter, /*skip_alt=*/true);

  if (!intf) {
    zxlogf(DEBUG, "%s: failed to get usb interface 0", __FUNCTION__);
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (intf->b_num_endpoints != kNumInterface0Endpoints) {
    zxlogf(DEBUG, "%s: interface has %hhu endpoints, expected %hhu", __FUNCTION__,
           intf->b_num_endpoints, kNumInterface0Endpoints);
    return ZX_ERR_NOT_SUPPORTED;
  }

  uint16_t intr_max_packet = 0;

  usb_endpoint_descriptor_t* endp = usb_desc_iter_next_endpoint(&config_desc_iter);
  while (endp) {
    if (usb_ep_direction(endp) == USB_ENDPOINT_OUT) {
      if (usb_ep_type(endp) == USB_ENDPOINT_BULK) {
        bulk_out_addr_ = endp->b_endpoint_address;
      }
    } else {
      ZX_ASSERT(usb_ep_direction(endp) == USB_ENDPOINT_IN);
      if (usb_ep_type(endp) == USB_ENDPOINT_BULK) {
        bulk_in_addr_ = endp->b_endpoint_address;
      } else if (usb_ep_type(endp) == USB_ENDPOINT_INTERRUPT) {
        intr_addr_ = endp->b_endpoint_address;
        intr_max_packet = usb_ep_max_packet(endp);
      }
    }
    endp = usb_desc_iter_next_endpoint(&config_desc_iter);
  }

  if (!bulk_in_addr_ || !bulk_out_addr_ || !intr_addr_) {
    zxlogf(ERROR, "bind could not find endpoints (bulk in: %d, bulk out: %d, intr: %d)",
           bulk_in_addr_.has_value(), bulk_out_addr_.has_value(), intr_addr_.has_value());
    return ZX_ERR_NOT_SUPPORTED;
  }

  // If SCO is supported, interface 1 should contain the isoc in and isoc out endpoints. We assume
  // that the alternate settings use the same endpoint addresses, so we skip them. See Core Spec
  // v5.3, Vol 4, Part B, Sec 2.1.1.
  uint8_t isoc_out_addr = 0;
  sco_supported_ = ReadIsocEndpointsFromConfig(&config_desc_iter, &isoc_out_addr);
  if (!sco_supported_) {
    zxlogf(DEBUG, "SCO is not supported");
  }

  list_initialize(&free_event_reqs_);
  list_initialize(&free_acl_read_reqs_);
  list_initialize(&free_acl_write_reqs_);
  list_initialize(&free_sco_write_reqs_);

  // Initialize events used by read thread.
  zx_status_t read_thread_port_status = zx_port_create(/*options=*/0, &read_thread_port_);
  if (read_thread_port_status != ZX_OK) {
    OnBindFailure(read_thread_port_status, "zx_port_create failure");
    return read_thread_port_status;
  }

  parent_req_size_ = usb_get_request_size(&usb);
  size_t req_size = parent_req_size_ + sizeof(usb_req_internal_t) + sizeof(void*);
  status = AllocBtUsbPackets(EVENT_REQ_COUNT, intr_max_packet, intr_addr_.value(), req_size,
                             &free_event_reqs_);
  if (status != ZX_OK) {
    OnBindFailure(status, "event USB request allocation failure");
    return status;
  }
  status = AllocBtUsbPackets(ACL_READ_REQ_COUNT, ACL_MAX_FRAME_SIZE, bulk_in_addr_.value(),
                             req_size, &free_acl_read_reqs_);
  if (status != ZX_OK) {
    OnBindFailure(status, "ACL read USB request allocation failure");
    return status;
  }
  status = AllocBtUsbPackets(ACL_WRITE_REQ_COUNT, ACL_MAX_FRAME_SIZE, bulk_out_addr_.value(),
                             req_size, &free_acl_write_reqs_);
  if (status != ZX_OK) {
    OnBindFailure(status, "ACL write USB request allocation failure");
    return status;
  }

  if (sco_supported_) {
    status = AllocBtUsbPackets(kScoWriteReqCount, kScoMaxFrameSize, isoc_out_addr, req_size,
                               &free_sco_write_reqs_);
    if (status != ZX_OK) {
      OnBindFailure(status, "SCO write USB request allocation failure");
      return status;
    }
  }

  mtx_lock(&mutex_);
  QueueInterruptRequestsLocked();
  QueueAclReadRequestsLocked();
  mtx_unlock(&mutex_);

  // Copy the PID and VID from the underlying BT so that it can be filtered on
  // for HCI drivers
  usb_device_descriptor_t dev_desc;
  usb_get_device_descriptor(&usb, &dev_desc);
  zx_device_prop_t props[] = {
      {.id = BIND_PROTOCOL, .reserved = 0, .value = ZX_PROTOCOL_BT_TRANSPORT},
      {.id = BIND_USB_VID, .reserved = 0, .value = dev_desc.id_vendor},
      {.id = BIND_USB_PID, .reserved = 0, .value = dev_desc.id_product},
  };
  zxlogf(DEBUG, "bt-transport-usb: vendor id = %hu, product id = %hu", dev_desc.id_vendor,
         dev_desc.id_product);

  ddk::DeviceAddArgs args("bt_transport_usb");
  args.set_props(props);
  args.set_proto_id(ZX_PROTOCOL_BT_TRANSPORT);
  status = DdkAdd(args);
  if (status != ZX_OK) {
    OnBindFailure(status, "DdkAdd");
    return status;
  }

  // Start the read thread.
  zxlogf(DEBUG, "starting read thread");
  thrd_create(&read_thread_, &Device::HciReadThread, this);
  return ZX_OK;
}

zx_status_t Device::DdkGetProtocol(uint32_t proto_id, void* out) {
  if (proto_id != ZX_PROTOCOL_BT_HCI) {
    // Pass this on for drivers to load firmware / initialize
    return device_get_protocol(parent(), proto_id, out);
  }

  bt_hci_protocol_t* hci_proto = static_cast<bt_hci_protocol_t*>(out);
  hci_proto->ops = &bt_hci_protocol_ops_;
  hci_proto->ctx = this;
  return ZX_OK;
}

void Device::DdkUnbind(ddk::UnbindTxn txn) {
  zxlogf(DEBUG, "%s", __FUNCTION__);

  // Spawn a thread to avoid blocking the main thread.
  unbind_thread_ = std::thread([this, unbind_txn = std::move(txn)]() mutable {
    // Copy the thread so it can be used without the mutex.
    thrd_t read_thread;

    // Close the transport channels so that the host stack is notified of device removal.
    mtx_lock(&mutex_);
    mtx_lock(&pending_request_lock_);

    read_thread = read_thread_;
    unbound_ = true;

    mtx_unlock(&pending_request_lock_);

    ChannelCleanupLocked(&cmd_channel_);
    ChannelCleanupLocked(&acl_channel_);
    ChannelCleanupLocked(&sco_channel_);
    ChannelCleanupLocked(&snoop_channel_);

    mtx_unlock(&mutex_);

    zxlogf(DEBUG, "DdkUnbind: canceling all requests");
    // usb_cancel_all synchronously cancels all requests.
    zx_status_t status = usb_cancel_all(&usb_, bulk_out_addr_.value());
    if (status != ZX_OK) {
      zxlogf(ERROR, "canceling bulk out requests failed with status: %s",
             zx_status_get_string(status));
    }
    status = usb_cancel_all(&usb_, bulk_in_addr_.value());
    if (status != ZX_OK) {
      zxlogf(ERROR, "canceling bulk in requests failed with status: %s",
             zx_status_get_string(status));
    }
    status = usb_cancel_all(&usb_, intr_addr_.value());
    if (status != ZX_OK) {
      zxlogf(ERROR, "canceling interrupt requests failed with status: %s",
             zx_status_get_string(status));
    }

    zxlogf(DEBUG, "DdkUnbind: waiting for read thread to complete");
    // Signal and wait for the read thread to complete (this is necessary to prevent use-after-free
    // of member variables in the read thread).
    zx_port_packet_t unbind_pkt;
    unbind_pkt.key = static_cast<uint64_t>(ReadThreadPortKey::kUnbind);
    unbind_pkt.type = ZX_PKT_TYPE_USER;
    zx_port_queue(read_thread_port_, &unbind_pkt);

    int join_res = 0;
    thrd_join(read_thread, &join_res);
    zxlogf(DEBUG, "read thread completed with status %d", join_res);

    unbind_txn.Reply();
  });
}

void Device::DdkRelease() {
  zxlogf(DEBUG, "%s", __FUNCTION__);

  unbind_thread_.join();

  mtx_lock(&mutex_);

  usb_request_t* req;
  while ((req = usb_req_list_remove_head(&free_event_reqs_, parent_req_size_)) != nullptr) {
    InstrumentedRequestRelease(req);
  }
  while ((req = usb_req_list_remove_head(&free_acl_read_reqs_, parent_req_size_)) != nullptr) {
    InstrumentedRequestRelease(req);
  }
  while ((req = usb_req_list_remove_head(&free_acl_write_reqs_, parent_req_size_)) != nullptr) {
    InstrumentedRequestRelease(req);
  }
  while ((req = usb_req_list_remove_head(&free_sco_write_reqs_, parent_req_size_)) != nullptr) {
    InstrumentedRequestRelease(req);
  }

  mtx_unlock(&mutex_);
  // Wait for all the requests in the pipeline to asynchronously fail.
  // Either the completion routine or the submitter should free the requests.
  // It shouldn't be possible to have any "stray" requests that aren't in-flight at this point,
  // so this is guaranteed to complete.
  zxlogf(DEBUG, "%s: waiting for all requests to be freed before releasing", __FUNCTION__);
  sync_completion_wait(&requests_freed_completion_, ZX_TIME_INFINITE);
  zxlogf(DEBUG, "%s: all requests freed", __FUNCTION__);

  // Driver manager is given a raw pointer to this dynamically allocated object in Bind(), so when
  // DdkRelease() is called we need to free the allocated memory.
  delete this;
}

bool Device::ReadIsocEndpointsFromConfig(usb_desc_iter_t* config_desc_iter,
                                         uint8_t* isoc_out_addr) {
  ZX_ASSERT(config_desc_iter);
  ZX_ASSERT(isoc_out_addr);

  usb_interface_descriptor_t* intf =
      usb_desc_iter_next_interface(config_desc_iter, /*skip_alt=*/true);
  if (!intf) {
    zxlogf(DEBUG, "USB interface 1 not present");
    return false;
  }

  if (intf->b_num_endpoints != kNumInterface1Endpoints) {
    zxlogf(DEBUG, "USB interface 1 does not have 2 SCO endpoints");
    return false;
  }

  usb_endpoint_descriptor_t* endp = usb_desc_iter_next_endpoint(config_desc_iter);
  while (endp) {
    if (usb_ep_direction(endp) == USB_ENDPOINT_OUT &&
        usb_ep_type(endp) == USB_ENDPOINT_ISOCHRONOUS) {
      *isoc_out_addr = endp->b_endpoint_address;
      return true;
    }
    endp = usb_desc_iter_next_endpoint(config_desc_iter);
  }
  return false;
}

// Allocates a USB request and keeps track of how many requests have been allocated.
zx_status_t Device::InstrumentedRequestAlloc(usb_request_t** out, uint64_t data_size,
                                             uint8_t ep_address, size_t req_size) {
  atomic_fetch_add(&allocated_requests_count_, 1);
  return usb_request_alloc(out, data_size, ep_address, req_size);
}

// Releases a USB request and decrements the usage count.
// Signals a completion when all requests have been released.
void Device::InstrumentedRequestRelease(usb_request_t* req) {
  usb_request_release(req);
  size_t req_count = atomic_fetch_sub(&allocated_requests_count_, 1);
  zxlogf(TRACE, "remaining allocated requests: %zu", req_count - 1);
  // atomic_fetch_sub returns the value prior to being updated, so a value of 1 means that this is
  // the last request.
  if (req_count == 1) {
    sync_completion_signal(&requests_freed_completion_);
  }
}

// usb_request_callback is a hook that is inserted for every USB request
// which guarantees the following conditions:
// * No completions will be invoked during driver unbind.
// * pending_request_count shall indicate the number of requests outstanding.
// * pending_requests_completed shall be asserted when the number of requests pending equals zero.
// * Requests are properly freed during shutdown.
void Device::UsbRequestCallback(usb_request_t* req) {
  zxlogf(TRACE, "%s", __FUNCTION__);
  // Invoke the real completion if not shutting down.
  mtx_lock(&pending_request_lock_);
  if (!unbound_) {
    // Request callback pointer is stored at the end of the usb_request_t after
    // other data that has been appended to the request by drivers elsewhere in the stack.
    // memcpy is necessary here to prevent undefined behavior since there are no guarantees
    // about the alignment of data that other drivers append to the usb_request_t.
    usb_callback_t callback;
    memcpy(&callback,
           reinterpret_cast<unsigned char*>(req) + parent_req_size_ + sizeof(usb_req_internal_t),
           sizeof(callback));
    // Our threading model allows a callback to immediately re-queue a request here
    // which would result in attempting to recursively lock pending_request_lock.
    // Unlocking the mutex is necessary to prevent a crash.
    mtx_unlock(&pending_request_lock_);
    callback(this, req);
    mtx_lock(&pending_request_lock_);
  } else {
    InstrumentedRequestRelease(req);
  }
  size_t pending_request_count = std::atomic_fetch_sub(&pending_request_count_, 1);
  zxlogf(TRACE, "%s: pending requests: %zu", __FUNCTION__, pending_request_count - 1);
  mtx_unlock(&pending_request_lock_);
}

void Device::UsbRequestSend(usb_protocol_t* function, usb_request_t* req, usb_callback_t callback) {
  mtx_lock(&pending_request_lock_);
  if (unbound_) {
    mtx_unlock(&pending_request_lock_);
    return;
  }
  std::atomic_fetch_add(&pending_request_count_, 1);
  size_t parent_req_size = parent_req_size_;
  mtx_unlock(&pending_request_lock_);

  usb_request_complete_callback_t internal_completion = {
      .callback =
          [](void* ctx, usb_request_t* request) {
            static_cast<Device*>(ctx)->UsbRequestCallback(request);
          },
      .ctx = this};
  memcpy(reinterpret_cast<unsigned char*>(req) + parent_req_size + sizeof(usb_req_internal_t),
         &callback, sizeof(callback));
  usb_request_queue(function, req, &internal_completion);
}

void Device::QueueAclReadRequestsLocked() {
  usb_request_t* req = nullptr;
  while ((req = usb_req_list_remove_head(&free_acl_read_reqs_, parent_req_size_)) != nullptr) {
    UsbRequestSend(&usb_, req, [](void* ctx, usb_request_t* req) {
      static_cast<Device*>(ctx)->HciAclReadComplete(req);
    });
  }
}

void Device::QueueInterruptRequestsLocked() {
  usb_request_t* req = nullptr;
  while ((req = usb_req_list_remove_head(&free_event_reqs_, parent_req_size_)) != nullptr) {
    UsbRequestSend(&usb_, req, [](void* ctx, usb_request_t* req) {
      static_cast<Device*>(ctx)->HciEventComplete(req);
    });
  }
}

void Device::ChannelCleanupLocked(zx::channel* channel) { channel->reset(); }

void Device::SnoopChannelWriteLocked(uint8_t flags, uint8_t* bytes, size_t length) {
  if (snoop_channel_ == ZX_HANDLE_INVALID)
    return;

  // We tack on a flags byte to the beginning of the payload.
  uint8_t snoop_buffer[length + 1];
  snoop_buffer[0] = flags;
  memcpy(snoop_buffer + 1, bytes, length);
  zx_status_t status = zx_channel_write(snoop_channel_.get(), 0, snoop_buffer,
                                        static_cast<uint32_t>(length + 1), nullptr, 0);
  if (status < 0) {
    if (status != ZX_ERR_PEER_CLOSED) {
      zxlogf(ERROR, "bt-transport-usb: failed to write to snoop channel: %s",
             zx_status_get_string(status));
    }
    ChannelCleanupLocked(&snoop_channel_);
  }
}

void Device::RemoveDeviceLocked() { DdkAsyncRemove(); }

void Device::HciEventComplete(usb_request_t* req) {
  zxlogf(TRACE, "bt-transport-usb: Event received");
  mtx_lock(&mutex_);

  if (req->response.status != ZX_OK) {
    HandleUsbResponseError(req, "hci event");
    mtx_unlock(&mutex_);
    return;
  }

  // Handle the interrupt as long as either the command channel or the snoop channel is open.
  if (cmd_channel_ == ZX_HANDLE_INVALID && snoop_channel_ == ZX_HANDLE_INVALID) {
    zxlogf(
        DEBUG,
        "bt-transport-usb: received hci event while command channel and snoop channel are closed");
    // Re-queue the HCI event USB request.
    zx_status_t status = usb_req_list_add_head(&free_event_reqs_, req, parent_req_size_);
    ZX_ASSERT(status == ZX_OK);
    QueueInterruptRequestsLocked();
    mtx_unlock(&mutex_);
    return;
  }

  void* buffer;
  zx_status_t status = usb_request_mmap(req, &buffer);
  if (status != ZX_OK) {
    zxlogf(ERROR, "bt-transport-usb: usb_req_mmap failed: %s", zx_status_get_string(status));
    mtx_unlock(&mutex_);
    return;
  }
  size_t length = req->response.actual;
  uint8_t event_parameter_total_size = static_cast<uint8_t*>(buffer)[1];
  size_t packet_size = event_parameter_total_size + sizeof(HciEventHeader);

  // simple case - packet fits in received data
  if (event_buffer_offset_ == 0 && length >= sizeof(HciEventHeader)) {
    if (packet_size == length) {
      if (cmd_channel_ != ZX_HANDLE_INVALID) {
        zx_status_t status = zx_channel_write(cmd_channel_.get(), 0, buffer,
                                              static_cast<uint32_t>(length), nullptr, 0);
        if (status < 0) {
          zxlogf(ERROR,
                 "bt-transport-usb: hci_event_complete failed to write to command channel: %s",
                 zx_status_get_string(status));
        }
      }
      SnoopChannelWriteLocked(bt_hci_snoop_flags(BT_HCI_SNOOP_TYPE_EVT, true),
                              static_cast<uint8_t*>(buffer), length);

      // Re-queue the HCI event USB request.
      status = usb_req_list_add_head(&free_event_reqs_, req, parent_req_size_);
      ZX_ASSERT(status == ZX_OK);
      QueueInterruptRequestsLocked();
      mtx_unlock(&mutex_);
      return;
    }
  }

  // complicated case - need to accumulate into hci->event_buffer

  if (event_buffer_offset_ + length > sizeof(event_buffer_)) {
    zxlogf(ERROR, "bt-transport-usb: event_buffer would overflow!");
    mtx_unlock(&mutex_);
    return;
  }

  memcpy(&event_buffer_[event_buffer_offset_], buffer, length);
  if (event_buffer_offset_ == 0) {
    event_buffer_packet_length_ = packet_size;
  } else {
    packet_size = event_buffer_packet_length_;
  }
  event_buffer_offset_ += length;

  // check to see if we have a full packet
  if (packet_size <= event_buffer_offset_) {
    zxlogf(TRACE,
           "bt-transport-usb: Accumulated full HCI event packet, sending on command & snoop "
           "channels.");
    zx_status_t status = zx_channel_write(cmd_channel_.get(), 0, event_buffer_,
                                          static_cast<uint32_t>(packet_size), nullptr, 0);
    if (status < 0) {
      zxlogf(ERROR, "bt-transport-usb: failed to write to command channel: %s",
             zx_status_get_string(status));
    }

    SnoopChannelWriteLocked(bt_hci_snoop_flags(BT_HCI_SNOOP_TYPE_EVT, true), event_buffer_,
                            packet_size);

    uint32_t remaining = static_cast<uint32_t>(event_buffer_offset_ - packet_size);
    memmove(event_buffer_, event_buffer_ + packet_size, remaining);
    event_buffer_offset_ = 0;
    event_buffer_packet_length_ = 0;
  } else {
    zxlogf(TRACE,
           "bt-transport-usb: Received incomplete chunk of HCI event packet. Appended to buffer.");
  }

  // Re-queue the HCI event USB request.
  status = usb_req_list_add_head(&free_event_reqs_, req, parent_req_size_);
  ZX_ASSERT(status == ZX_OK);
  QueueInterruptRequestsLocked();
  mtx_unlock(&mutex_);
}

void Device::HciAclReadComplete(usb_request_t* req) {
  zxlogf(TRACE, "bt-transport-usb: ACL frame received");
  mtx_lock(&mutex_);

  if (req->response.status == ZX_ERR_IO_INVALID) {
    zxlogf(TRACE, "bt-transport-usb: request stalled, ignoring.");
    zx_status_t status = usb_req_list_add_head(&free_acl_read_reqs_, req, parent_req_size_);
    ZX_DEBUG_ASSERT(status == ZX_OK);
    QueueAclReadRequestsLocked();

    mtx_unlock(&mutex_);
    return;
  }

  if (req->response.status != ZX_OK) {
    HandleUsbResponseError(req, "acl read");
    mtx_unlock(&mutex_);
    return;
  }

  void* buffer;
  zx_status_t status = usb_request_mmap(req, &buffer);
  if (status != ZX_OK) {
    zxlogf(ERROR, "bt-transport-usb: usb_req_mmap failed: %s", zx_status_get_string(status));
    mtx_unlock(&mutex_);
    return;
  }

  if (acl_channel_ == ZX_HANDLE_INVALID) {
    zxlogf(ERROR, "bt-transport-usb: ACL data received while channel is closed");
  } else {
    status = zx_channel_write(acl_channel_.get(), 0, buffer,
                              static_cast<uint32_t>(req->response.actual), nullptr, 0);
    if (status < 0) {
      zxlogf(ERROR, "bt-transport-usb: hci_acl_read_complete failed to write: %s",
             zx_status_get_string(status));
    }
  }

  // If the snoop channel is open then try to write the packet even if acl_channel was closed.
  SnoopChannelWriteLocked(bt_hci_snoop_flags(BT_HCI_SNOOP_TYPE_ACL, true),
                          static_cast<uint8_t*>(buffer), req->response.actual);

  status = usb_req_list_add_head(&free_acl_read_reqs_, req, parent_req_size_);
  ZX_DEBUG_ASSERT(status == ZX_OK);
  QueueAclReadRequestsLocked();

  mtx_unlock(&mutex_);
}

void Device::HciAclWriteComplete(usb_request_t* req) {
  mtx_lock(&mutex_);

  if (req->response.status != ZX_OK) {
    HandleUsbResponseError(req, "acl write");
    mtx_unlock(&mutex_);
    return;
  }

  zx_status_t status = usb_req_list_add_tail(&free_acl_write_reqs_, req, parent_req_size_);
  ZX_DEBUG_ASSERT(status == ZX_OK);

  if (snoop_channel_) {
    void* buffer;
    zx_status_t status = usb_request_mmap(req, &buffer);
    if (status != ZX_OK) {
      zxlogf(ERROR, "bt-transport-usb: usb_req_mmap failed: %s", zx_status_get_string(status));
      mtx_unlock(&mutex_);
      return;
    }

    SnoopChannelWriteLocked(bt_hci_snoop_flags(BT_HCI_SNOOP_TYPE_ACL, false),
                            static_cast<uint8_t*>(buffer), req->response.actual);
  }

  mtx_unlock(&mutex_);
}

void Device::HciScoWriteComplete(usb_request_t* req) {
  mtx_lock(&mutex_);

  if (req->response.status != ZX_OK) {
    zxlogf(
        ERROR,
        "bt-transport-usb: sco write request completed with error status %d (%s). Removing device",
        req->response.status, zx_status_get_string(req->response.status));
    RemoveDeviceLocked();
    InstrumentedRequestRelease(req);
    mtx_unlock(&mutex_);
    return;
  }

  zx_status_t status = usb_req_list_add_tail(&free_sco_write_reqs_, req, parent_req_size_);
  ZX_ASSERT(status == ZX_OK);

  if (snoop_channel_) {
    void* buffer;
    zx_status_t status = usb_request_mmap(req, &buffer);
    if (status != ZX_OK) {
      zxlogf(ERROR, "usb_req_mmap failed: %s", zx_status_get_string(status));
      mtx_unlock(&mutex_);
      return;
    }

    SnoopChannelWriteLocked(bt_hci_snoop_flags(BT_HCI_SNOOP_TYPE_SCO, false),
                            static_cast<uint8_t*>(buffer), req->response.actual);
  }

  mtx_unlock(&mutex_);
}

void Device::HciHandleCmdReadEvents(const zx_port_packet_t& packet) {
  ZX_ASSERT(packet.signal.observed & (ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED));

  mtx_lock(&mutex_);

  if (packet.signal.observed & ZX_CHANNEL_PEER_CLOSED) {
    ChannelCleanupLocked(&cmd_channel_);
    mtx_unlock(&mutex_);
    return;
  }

  zx_status_t wait_status =
      zx_object_wait_async(cmd_channel_.get(), read_thread_port_,
                           static_cast<uint64_t>(ReadThreadPortKey::kCommandChannel),
                           ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED, /*options=*/0);
  if (wait_status != ZX_OK) {
    zxlogf(ERROR, "Failed to wait on command channel %s", zx_status_get_string(wait_status));
    ChannelCleanupLocked(&cmd_channel_);
    mtx_unlock(&mutex_);
    return;
  }

  uint8_t buf[CMD_BUF_SIZE];
  uint32_t length = sizeof(buf);

  // Read messages until the channel is empty or an error occurs.
  while (true) {
    zx_status_t read_status =
        zx_channel_read(cmd_channel_.get(), 0, buf, nullptr, length, 0, &length, nullptr);
    if (read_status == ZX_ERR_SHOULD_WAIT) {
      // The channel is empty.
      break;
    };
    if (read_status != ZX_OK) {
      zxlogf(ERROR, "hci_read_thread: failed to read from command channel %s",
             zx_status_get_string(read_status));
      ChannelCleanupLocked(&cmd_channel_);
      break;
    }

    zx_status_t control_status =
        usb_control_out(&usb_, USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_DEVICE, 0, 0, 0,
                        ZX_TIME_INFINITE, buf, length);
    if (control_status != ZX_OK) {
      zxlogf(ERROR, "hci_read_thread: usb_control_out failed: %s",
             zx_status_get_string(control_status));
      ChannelCleanupLocked(&cmd_channel_);
      break;
    }

    SnoopChannelWriteLocked(bt_hci_snoop_flags(BT_HCI_SNOOP_TYPE_CMD, false), buf, length);
  }

  mtx_unlock(&mutex_);
}

void Device::HciHandleAclReadEvents(const zx_port_packet_t& packet) {
  ZX_ASSERT(packet.signal.observed & (ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED));

  if (packet.signal.observed & ZX_CHANNEL_PEER_CLOSED) {
    mtx_lock(&mutex_);
    ChannelCleanupLocked(&acl_channel_);
    mtx_unlock(&mutex_);
    return;
  }

  mtx_lock(&mutex_);
  zx_status_t wait_status = zx_object_wait_async(
      acl_channel_.get(), read_thread_port_, static_cast<uint64_t>(ReadThreadPortKey::kAclChannel),
      ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED, /*options=*/0);
  if (wait_status != ZX_OK) {
    zxlogf(ERROR, "Failed to wait on acl channel %s", zx_status_get_string(wait_status));
    ChannelCleanupLocked(&acl_channel_);
    mtx_unlock(&mutex_);
    return;
  }
  mtx_unlock(&mutex_);

  // Read until the channel is empty.
  while (true) {
    mtx_lock(&mutex_);
    if (!acl_channel_.is_valid()) {
      mtx_unlock(&mutex_);
      break;
    }
    list_node_t* node = list_peek_head(&free_acl_write_reqs_);

    // We don't have enough reqs. Simply punt the channel read until later.
    if (!node) {
      mtx_unlock(&mutex_);
      break;
    }

    uint8_t buf[ACL_MAX_FRAME_SIZE];
    uint32_t length = sizeof(buf);
    zx_status_t read_status =
        zx_channel_read(acl_channel_.get(), 0, buf, nullptr, length, 0, &length, nullptr);
    if (read_status == ZX_ERR_SHOULD_WAIT) {
      // There's nothing to read for now, so wait for future signals.
      mtx_unlock(&mutex_);
      break;
    }
    if (read_status != ZX_OK) {
      zxlogf(ERROR, "hci_read_thread: failed to read from ACL channel %s",
             zx_status_get_string(read_status));
      ChannelCleanupLocked(&acl_channel_);
      mtx_unlock(&mutex_);
      break;
    }

    node = list_remove_head(&free_acl_write_reqs_);
    // Unlock so that the write callback doesn't try to recursively lock the mutex if called
    // synchronously.
    mtx_unlock(&mutex_);

    // At this point if we don't get a free node from |free_acl_write_reqs| that means that
    // they were cleaned up in hci_release(). Just drop the packet.
    if (!node) {
      return;
    }

    usb_req_internal_t* req_int = containerof(node, usb_req_internal_t, node);
    usb_request_t* req = REQ_INTERNAL_TO_USB_REQ(req_int, parent_req_size_);
    size_t result = usb_request_copy_to(req, buf, length, 0);
    ZX_ASSERT(result == length);
    req->header.length = length;
    UsbRequestSend(&usb_, req, [](void* ctx, usb_request_t* req) {
      static_cast<Device*>(ctx)->HciAclWriteComplete(req);
    });
  }
}

void Device::HciHandleScoReadEvents(const zx_port_packet_t& packet) {
  ZX_ASSERT(packet.signal.observed & (ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED));

  if (packet.signal.observed & ZX_CHANNEL_PEER_CLOSED) {
    mtx_lock(&mutex_);
    ChannelCleanupLocked(&sco_channel_);
    mtx_unlock(&mutex_);
    return;
  }

  mtx_lock(&mutex_);
  zx_status_t wait_status = zx_object_wait_async(
      sco_channel_.get(), read_thread_port_, static_cast<uint64_t>(ReadThreadPortKey::kScoChannel),
      ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED, /*options=*/0);
  if (wait_status != ZX_OK) {
    zxlogf(ERROR, "Failed to wait on sco channel %s", zx_status_get_string(wait_status));
    ChannelCleanupLocked(&sco_channel_);
    mtx_unlock(&mutex_);
    return;
  }
  mtx_unlock(&mutex_);

  // Read until the channel is empty.
  while (true) {
    mtx_lock(&mutex_);
    list_node_t* node = list_peek_head(&free_sco_write_reqs_);

    // We don't have enough reqs. Simply punt the channel read until later.
    if (!node) {
      mtx_unlock(&mutex_);
      break;
    }

    uint8_t buf[kScoMaxFrameSize];
    uint32_t actual_length = 0;
    zx_status_t status =
        zx_channel_read(sco_channel_.get(), /*options=*/0, buf, /*handles=*/nullptr, sizeof(buf),
                        /*num_handles=*/0, &actual_length, /*actual_handles=*/nullptr);
    if (status == ZX_ERR_SHOULD_WAIT) {
      mtx_unlock(&mutex_);
      break;
    }
    if (status != ZX_OK) {
      zxlogf(ERROR, "hci_read_thread: failed to read from SCO channel %s",
             zx_status_get_string(status));
      ChannelCleanupLocked(&sco_channel_);
      mtx_unlock(&mutex_);
      break;
    }

    // To prevent a backup, just drop packets while the alt setting is being changed.
    if (!isoc_alt_setting_mutex_.try_lock()) {
      zxlogf(DEBUG, "couldn't get alt setting lock - dropping outbound packet for SCO channel");
      mtx_unlock(&mutex_);
      continue;
    }

    // Drop packets if no alternate setting is selected.
    if (isoc_alt_setting_ == kIsocAltSettingInactive) {
      zxlogf(DEBUG,
             "Dropping packet for SCO channel due to no alt setting configured. "
             "BtHciConfigureSco() must be called first.");
      isoc_alt_setting_mutex_.unlock();
      mtx_unlock(&mutex_);
      continue;
    }

    node = list_remove_head(&free_sco_write_reqs_);
    mtx_unlock(&mutex_);
    // The mutex was held between the peek and the remove, so if we got this far the list must have
    // a node.
    ZX_ASSERT(node);

    usb_req_internal_t* req_int = containerof(node, usb_req_internal_t, node);
    usb_request_t* req = REQ_INTERNAL_TO_USB_REQ(req_int, parent_req_size_);
    size_t result = usb_request_copy_to(req, buf, actual_length, 0);
    ZX_ASSERT(result == actual_length);
    req->header.length = actual_length;
    // Callback may be called synchronously, so mutex_ must not be held.
    UsbRequestSend(&usb_, req, [](void* ctx, usb_request_t* req) {
      static_cast<Device*>(ctx)->HciScoWriteComplete(req);
    });

    isoc_alt_setting_mutex_.unlock();
  }
}

int Device::HciReadThread(void* void_dev) {
  zxlogf(DEBUG, "read thread started");
  Device* dev = static_cast<Device*>(void_dev);

  while (true) {
    zx_port_packet_t port_packet;
    zx_status_t port_wait_status =
        zx_port_wait(dev->read_thread_port_, ZX_TIME_INFINITE, &port_packet);
    if (port_wait_status != ZX_OK) {
      zxlogf(ERROR, "%s: zx_object_wait_many failed (%s) - exiting", __FUNCTION__,
             zx_status_get_string(port_wait_status));
      mtx_lock(&dev->mutex_);
      dev->ChannelCleanupLocked(&dev->cmd_channel_);
      dev->ChannelCleanupLocked(&dev->acl_channel_);
      mtx_unlock(&dev->mutex_);
      break;
    }

    switch (static_cast<ReadThreadPortKey>(port_packet.key)) {
      case ReadThreadPortKey::kCommandChannel:
        zxlogf(TRACE, "%s: handling cmd read event (signal count: %zu)", __FUNCTION__,
               port_packet.signal.count);
        dev->HciHandleCmdReadEvents(port_packet);
        break;
      case ReadThreadPortKey::kAclChannel:
        zxlogf(TRACE, "%s: handling acl read event (signal count: %zu)", __FUNCTION__,
               port_packet.signal.count);
        dev->HciHandleAclReadEvents(port_packet);
        break;
      case ReadThreadPortKey::kScoChannel:
        zxlogf(TRACE, "%s: handling sco read event (signal count: %zu)", __FUNCTION__,
               port_packet.signal.count);
        dev->HciHandleScoReadEvents(port_packet);
        break;
      case ReadThreadPortKey::kUnbind:
        // The driver is being unbound, so terminate the read thread.
        zxlogf(DEBUG, "%s: unbinding", __FUNCTION__);
        return 0;
    }
  }

  zxlogf(DEBUG, "%s exiting", __FUNCTION__);
  return 0;
}

zx_status_t Device::HciOpenChannel(zx::channel* out, zx::channel in, ReadThreadPortKey key) {
  mtx_lock(&mutex_);
  mtx_lock(&pending_request_lock_);
  if (unbound_) {
    mtx_unlock(&pending_request_lock_);
    mtx_unlock(&mutex_);
    return ZX_ERR_CANCELED;
  }
  mtx_unlock(&pending_request_lock_);

  if (*out != ZX_HANDLE_INVALID) {
    zxlogf(ERROR, "bt-transport-usb: already bound, failing");
    mtx_unlock(&mutex_);
    return ZX_ERR_ALREADY_BOUND;
  }

  *out = std::move(in);
  zx_status_t status =
      zx_object_wait_async(out->get(), read_thread_port_, static_cast<uint64_t>(key),
                           ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED, /*options=*/0);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: failed to wait on channel: %s", __FUNCTION__, zx_status_get_string(status));
    mtx_unlock(&mutex_);
    return status;
  }

  mtx_unlock(&mutex_);
  return ZX_OK;
}

zx_status_t Device::BtHciOpenCommandChannel(zx::channel channel) {
  zxlogf(TRACE, "%s", __FUNCTION__);
  return HciOpenChannel(&cmd_channel_, std::move(channel), ReadThreadPortKey::kCommandChannel);
}

zx_status_t Device::BtHciOpenAclDataChannel(zx::channel channel) {
  zxlogf(TRACE, "%s", __FUNCTION__);
  return HciOpenChannel(&acl_channel_, std::move(channel), ReadThreadPortKey::kAclChannel);
}

zx_status_t Device::BtHciOpenScoChannel(zx::channel channel) {
  zxlogf(TRACE, "%s", __FUNCTION__);
  if (!sco_supported_) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  return HciOpenChannel(&sco_channel_, std::move(channel), ReadThreadPortKey::kScoChannel);
}

void Device::BtHciConfigureSco(sco_coding_format_t coding_format, sco_encoding_t encoding,
                               sco_sample_rate_t sample_rate,
                               bt_hci_configure_sco_callback callback, void* cookie) {
  if (!sco_supported_) {
    callback(cookie, ZX_ERR_NOT_SUPPORTED);
    return;
  }

  mtx_lock(&pending_request_lock_);
  if (unbound_) {
    mtx_unlock(&pending_request_lock_);
    callback(cookie, ZX_ERR_CANCELED);
    return;
  }
  mtx_unlock(&pending_request_lock_);

  // Prevent multiple calls to this method from racing.
  isoc_alt_setting_mutex_.lock();

  // Only the settings for 1 voice channel are supported.
  // MSBC uses alt setting 1 because few controllers support alt setting 6.
  // See Core Spec v5.3, Vol 4, Part B, Sec 2.1.1 for settings table.
  if (coding_format == SCO_CODING_FORMAT_MSBC ||
      (sample_rate == SCO_SAMPLE_RATE_KHZ_8 && encoding == SCO_ENCODING_BITS_8)) {
    isoc_alt_setting_ = 1;
  } else if (sample_rate == SCO_SAMPLE_RATE_KHZ_8 && encoding == SCO_ENCODING_BITS_16) {
    isoc_alt_setting_ = 2;
  } else if (sample_rate == SCO_SAMPLE_RATE_KHZ_16 && encoding == SCO_ENCODING_BITS_16) {
    isoc_alt_setting_ = 4;
  } else {
    isoc_alt_setting_mutex_.unlock();
    callback(cookie, ZX_ERR_NOT_SUPPORTED);
    return;
  }

  zx_status_t status = usb_set_interface(&usb_, kIsocInterfaceNum, isoc_alt_setting_);

  isoc_alt_setting_mutex_.unlock();
  callback(cookie, status);
}

void Device::BtHciResetSco(bt_hci_reset_sco_callback callback, void* cookie) {
  if (!sco_supported_) {
    callback(cookie, ZX_ERR_NOT_SUPPORTED);
    return;
  }

  isoc_alt_setting_mutex_.lock();

  if (isoc_alt_setting_ == kIsocAltSettingInactive) {
    isoc_alt_setting_mutex_.unlock();
    callback(cookie, ZX_OK);
    return;
  }
  isoc_alt_setting_ = kIsocAltSettingInactive;

  zx_status_t status = usb_set_interface(&usb_, kIsocInterfaceNum, kIsocAltSettingInactive);
  isoc_alt_setting_mutex_.unlock();
  callback(cookie, status);
}

zx_status_t Device::BtHciOpenSnoopChannel(zx::channel channel) {
  zxlogf(TRACE, "%s", __FUNCTION__);

  mtx_lock(&mutex_);
  mtx_lock(&pending_request_lock_);
  if (unbound_) {
    mtx_unlock(&mutex_);
    mtx_unlock(&pending_request_lock_);
    return ZX_ERR_CANCELED;
  }
  mtx_unlock(&pending_request_lock_);

  // Initialize snoop_watch_ port for detecting if a previous client closed the channel.
  // This is only necessary for the first snoop client.
  if (snoop_watch_ == ZX_HANDLE_INVALID) {
    zx_status_t status = zx_port_create(0, &snoop_watch_);
    if (status != ZX_OK) {
      zxlogf(ERROR,
             "bt-transport-usb: failed to create a port to watch snoop channel: "
             "%s\n",
             zx_status_get_string(status));
      mtx_unlock(&mutex_);
      return status;
    }
  } else {
    // Check if previous snoop client closed the channel, in which case the new channel can be
    // configured.
    zx_port_packet_t packet;
    zx_status_t status = zx_port_wait(snoop_watch_, /*deadline=*/0, &packet);
    if (status == ZX_ERR_TIMED_OUT) {
      zxlogf(TRACE, "bt-transport-usb: snoop port wait timed out: %s",
             zx_status_get_string(status));
    } else if (packet.signal.observed & ZX_CHANNEL_PEER_CLOSED) {
      zxlogf(
          TRACE,
          "previous snoop channel peer closed; proceeding with configuration of new snoop channel");
      snoop_channel_.reset();
    }
  }

  if (snoop_channel_.is_valid()) {
    mtx_unlock(&mutex_);
    return ZX_ERR_ALREADY_BOUND;
  }

  snoop_channel_ = std::move(channel);
  // Send a signal to the snoop_watch_ port when the snoop channel is closed by the peer.
  zx_object_wait_async(snoop_channel_.get(), snoop_watch_, 0, ZX_CHANNEL_PEER_CLOSED, 0);
  mtx_unlock(&mutex_);
  return ZX_OK;
}

zx_status_t Device::AllocBtUsbPackets(int limit, uint64_t data_size, uint8_t ep_address,
                                      size_t req_size, list_node_t* list) {
  zx_status_t status;
  for (int i = 0; i < limit; i++) {
    usb_request_t* req;
    status = InstrumentedRequestAlloc(&req, data_size, ep_address, req_size);
    if (status != ZX_OK) {
      return status;
    }
    status = usb_req_list_add_head(list, req, parent_req_size_);
    if (status != ZX_OK) {
      return status;
    }
  }
  return ZX_OK;
}

void Device::OnBindFailure(zx_status_t status, const char* msg) {
  zxlogf(ERROR, "bind failed due to %s: %s", msg, zx_status_get_string(status));
  DdkRelease();
}

void Device::HandleUsbResponseError(usb_request_t* req, const char* msg) {
  zxlogf(ERROR, "%s request completed with error status %d (%s). Removing device", msg,
         req->response.status, zx_status_get_string(req->response.status));
  InstrumentedRequestRelease(req);
  RemoveDeviceLocked();
}

// A lambda is used to create an empty instance of zx_driver_ops_t.
static zx_driver_ops_t usb_bt_hci_driver_ops = []() {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = Device::Create;
  return ops;
}();

}  // namespace bt_transport_usb

ZIRCON_DRIVER(bt_transport_usb, bt_transport_usb::usb_bt_hci_driver_ops, "zircon", "0.1");

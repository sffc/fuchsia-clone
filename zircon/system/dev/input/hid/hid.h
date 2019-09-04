// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_SYSTEM_DEV_INPUT_HID_HID_H_
#define ZIRCON_SYSTEM_DEV_INPUT_HID_HID_H_

#include <fuchsia/hardware/input/llcpp/fidl.h>

#include <array>
#include <memory>
#include <vector>

#include <ddk/binding.h>
#include <ddk/debug.h>
#include <ddk/device.h>
#include <ddk/driver.h>
#include <ddktl/device.h>
#include <ddktl/fidl.h>
#include <ddktl/protocol/empty-protocol.h>
#include <ddktl/protocol/hidbus.h>
#include <ddktl/protocol/hiddevice.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/mutex.h>

#include "hid-fifo.h"
#include "hid-parser.h"

namespace hid_driver {

class HidDevice;

using ::llcpp::fuchsia::hardware::input::BootProtocol;
using ::llcpp::fuchsia::hardware::input::ReportType;

class HidInstance;
using HidInstanceDeviceType =
    ddk::Device<HidInstance, ddk::Readable, ddk::Closable, ddk::Messageable>;

class HidInstance : public HidInstanceDeviceType,
                    public fbl::DoublyLinkedListable<HidInstance*>,
                    public ::llcpp::fuchsia::hardware::input::Device::Interface,
                    public ddk::EmptyProtocol<ZX_PROTOCOL_HID_DEVICE> {
 public:
  explicit HidInstance(zx_device_t* parent) : HidInstanceDeviceType(parent) {
    zx_hid_fifo_init(&fifo_);
  }
  ~HidInstance() = default;

  zx_status_t Bind(HidDevice* base);
  zx_status_t DdkRead(void* data, size_t len, zx_off_t off, size_t* actual);
  zx_status_t DdkMessage(fidl_msg_t* msg, fidl_txn_t* txn);
  void DdkRelease();
  zx_status_t DdkClose(uint32_t flags);

  void GetBootProtocol(GetBootProtocolCompleter::Sync _completer) override;
  void GetReportDescSize(GetReportDescSizeCompleter::Sync _completer) override;
  void GetReportDesc(GetReportDescCompleter::Sync _completer) override;
  void GetNumReports(GetNumReportsCompleter::Sync _completer) override;
  void GetReportIds(GetReportIdsCompleter::Sync _completer) override;
  void GetReportSize(ReportType type, uint8_t id, GetReportSizeCompleter::Sync _completer) override;
  void GetMaxInputReportSize(GetMaxInputReportSizeCompleter::Sync _completer) override;
  void GetReport(ReportType type, uint8_t id, GetReportCompleter::Sync _completer) override;
  void SetReport(ReportType type, uint8_t id, ::fidl::VectorView<uint8_t> report,
                 SetReportCompleter::Sync _completer) override;
  void SetTraceId(uint32_t id, SetTraceIdCompleter::Sync _completer) override;

  void CloseInstance();
  void WriteToFifo(const uint8_t* report, size_t report_len);

 private:
  HidDevice* base_ = nullptr;

  uint32_t flags_ = 0;

  zx_hid_fifo_t fifo_ = {};
  uint32_t trace_id_ = 0;
  uint32_t reports_written_ = 0;
  uint32_t reports_read_ = 0;
};

using HidDeviceType = ddk::Device<HidDevice, ddk::Unbindable, ddk::Openable>;

class HidDevice : public HidDeviceType,
                  public ddk::HidDeviceProtocol<HidDevice, ddk::base_protocol> {
 public:
  explicit HidDevice(zx_device_t* parent) : HidDeviceType(parent) {}
  ~HidDevice() = default;

  zx_status_t Bind(ddk::HidbusProtocolClient hidbus_proto);
  void DdkRelease();
  zx_status_t DdkOpen(zx_device_t** dev_out, uint32_t flags);
  void DdkUnbind();

  // |HidDeviceProtocol|
  zx_status_t HidDeviceRegisterListener(const hid_report_listener_protocol_t* listener);
  // |HidDeviceProtocol|
  void HidDeviceUnregisterListener();
  // |HidDeviceProtocol|
  zx_status_t HidDeviceGetDescriptor(uint8_t* out_descriptor_data, size_t descriptor_count,
                                     size_t* out_descriptor_actual);
  // |HidDeviceProtocol|
  zx_status_t HidDeviceGetReport(hid_report_type_t rpt_type, uint8_t rpt_id,
                                 uint8_t* out_report_data, size_t report_count,
                                 size_t* out_report_actual);
  // |HidDeviceProtocol|
  zx_status_t HidDeviceSetReport(hid_report_type_t rpt_type, uint8_t rpt_id,
                                 const uint8_t* report_data, size_t report_count);

  static void IoQueue(void* cookie, const void* _buf, size_t len);

  input_report_size_t GetMaxInputReportSize();
  input_report_size_t GetReportSizeById(input_report_id_t id, ReportType type);
  BootProtocol GetBootProtocol();

  ddk::HidbusProtocolClient* GetHidbusProtocol() { return &hidbus_; }

  void RemoveHidInstanceFromList(HidInstance* instance);

  size_t GetReportDescLen() { return hid_report_desc_.size(); }
  const uint8_t* GetReportDesc() { return hid_report_desc_.data(); }
  size_t GetNumReports() { return num_reports_; }

  // Needs to be called with an array of size |fuchsia_hardware_input_MAX_REPORT_IDS|.
  void GetReportIds(uint8_t* report_ids);

  const char* GetName();

 private:
  // TODO(dgilhooley): Don't hardcode this limit
  static constexpr size_t kHidMaxReportIds = 32;

  zx_status_t ProcessReportDescriptor();
  zx_status_t InitReassemblyBuffer();
  void ReleaseReassemblyBuffer();
  zx_status_t SetReportDescriptor();

  hid_info_t info_ = {};
  ddk::HidbusProtocolClient hidbus_;

  // Reassembly buffer for input events too large to fit in a single interrupt
  // transaction.
  uint8_t* rbuf_ = nullptr;
  size_t rbuf_size_ = 0;
  size_t rbuf_filled_ = 0;
  size_t rbuf_needed_ = 0;

  std::vector<uint8_t> hid_report_desc_;

  size_t num_reports_ = 0;
  std::array<hid_report_size_t, kHidMaxReportIds> sizes_;

  fbl::Mutex instance_lock_;
  // Unmanaged linked-list because the HidInstances free themselves through DdkRelease.
  fbl::DoublyLinkedList<HidInstance*> instance_list_ __TA_GUARDED(instance_lock_);

  std::array<char, ZX_DEVICE_NAME_MAX + 1> name_;

  fbl::Mutex listener_lock_;
  ddk::HidReportListenerProtocolClient report_listener_ __TA_GUARDED(listener_lock_);
};

}  // namespace hid_driver

#endif  // ZIRCON_SYSTEM_DEV_INPUT_HID_HID_H_

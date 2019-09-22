// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "tools/fidlcat/lib/syscall_decoder_dispatcher.h"

#include <zircon/system/public/zircon/errors.h>
#include <zircon/system/public/zircon/types.h>

#include <cstdint>
#include <memory>

#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "tools/fidlcat/lib/syscall_decoder.h"

namespace fidlcat {

constexpr int kPatternColorSize = 4;
constexpr int kPatternSize = 8;

void CantDecode(const uint8_t* bytes, uint32_t num_bytes, uint32_t num_handles,
                SyscallDisplayDispatcher* dispatcher, std::string_view line_header, int tabs,
                std::ostream& os) {
  os << line_header << std::string(tabs * fidl_codec::kTabSize, ' ') << dispatcher->colors().red
     << "Can't decode message num_bytes=" << num_bytes << " num_handles=" << num_handles;
  if ((bytes != nullptr) && (num_bytes >= sizeof(fidl_message_header_t))) {
    auto header = reinterpret_cast<const fidl_message_header_t*>(bytes);
    os << " ordinal=" << std::hex << header->ordinal << std::dec;
    if (dispatcher->message_decoder_dispatcher().loader() != nullptr) {
      const std::vector<const fidl_codec::InterfaceMethod*>* methods =
          dispatcher->message_decoder_dispatcher().loader()->GetByOrdinal(header->ordinal);
      if ((methods != nullptr) && !methods->empty()) {
        const fidl_codec::InterfaceMethod* method = (*methods)[0];
        os << '(' << method->enclosing_interface().name() << '.' << method->name() << ')';
      }
    }
  }
  os << '\n';
  os << line_header << std::string((tabs + 1) * fidl_codec::kTabSize, ' ') << "data=";
  const char* separator = " ";
  for (uint32_t i = 0; i < num_bytes; ++i) {
    // Display 4 bytes in red then four bytes in black ...
    if (i % kPatternSize == 0) {
      os << dispatcher->colors().red;
    } else if (i % kPatternColorSize == 0) {
      os << dispatcher->colors().reset;
    }
    std::vector<char> buffer(sizeof(uint8_t) * kCharactersPerByte + 1);
    snprintf(buffer.data(), buffer.size(), "%02x", bytes[i]);
    os << separator << buffer.data();
    separator = ", ";
  }
  os << dispatcher->colors().reset << '\n';
}

void DisplayString(const fidl_codec::Colors& colors, const char* string, size_t size,
                   std::ostream& os) {
  if (string == nullptr) {
    os << "nullptr\n";
  } else {
    if (size == 0) {
      os << "empty\n";
    } else {
      os << colors.red << '"';
      for (size_t i = 0; i < size; ++i) {
        char value = string[i];
        switch (value) {
          case 0:
            break;
          case '\\':
            os << "\\\\";
            break;
          case '\n':
            os << "\\n";
            break;
          default:
            os << value;
            break;
        }
      }
      os << '"' << colors.reset;
    }
  }
}

const char* SyscallInputOutputFixedSizeString::DisplayInline(SyscallDisplayDispatcher* dispatcher,
                                                             SyscallDecoder* decoder, Stage stage,
                                                             const char* separator,
                                                             std::ostream& os) const {
  const fidl_codec::Colors& colors = dispatcher->colors();
  os << separator;
  os << name() << ':' << colors.green << "string" << colors.reset << ": ";
  const char* string = string_->Content(decoder, stage);
  size_t string_size = (string == nullptr) ? 0 : strnlen(string, string_size_);
  DisplayString(colors, string, string_size, os);
  return ", ";
}

void SyscallFidlMessageHandle::DisplayOutline(SyscallDisplayDispatcher* dispatcher,
                                              SyscallDecoder* decoder, Stage stage,
                                              std::string_view line_header, int tabs,
                                              std::ostream& os) const {
  zx_handle_t handle = handle_->Value(decoder, stage);
  const uint8_t* bytes = bytes_->Content(decoder, stage);
  uint32_t num_bytes = num_bytes_->Value(decoder, stage);
  const zx_handle_t* handles = handles_->Content(decoder, stage);
  uint32_t num_handles = num_handles_->Value(decoder, stage);
  zx_handle_info_t* handle_infos = nullptr;
  if (num_handles > 0) {
    handle_infos = new zx_handle_info_t[num_handles];
    for (uint32_t i = 0; i < num_handles; ++i) {
      handle_infos[i].handle = handles[i];
      handle_infos[i].type = ZX_OBJ_TYPE_NONE;
      handle_infos[i].rights = 0;
    }
  }
  if (!dispatcher->message_decoder_dispatcher().DecodeMessage(
          decoder->thread()->GetProcess()->GetKoid(), handle, bytes, num_bytes, handle_infos,
          num_handles, type_, os, line_header, tabs)) {
    CantDecode(bytes, num_bytes, num_handles, dispatcher, line_header, tabs, os);
  }
  delete[] handle_infos;
}

void SyscallFidlMessageHandleInfo::DisplayOutline(SyscallDisplayDispatcher* dispatcher,
                                                  SyscallDecoder* decoder, Stage stage,
                                                  std::string_view line_header, int tabs,
                                                  std::ostream& os) const {
  zx_handle_t handle = handle_->Value(decoder, stage);
  const uint8_t* bytes = bytes_->Content(decoder, stage);
  uint32_t num_bytes = num_bytes_->Value(decoder, stage);
  const zx_handle_info_t* handle_infos = handles_->Content(decoder, stage);
  uint32_t num_handles = num_handles_->Value(decoder, stage);
  if (!dispatcher->message_decoder_dispatcher().DecodeMessage(
          decoder->thread()->GetProcess()->GetKoid(), handle, bytes, num_bytes, handle_infos,
          num_handles, type_, os, line_header, tabs)) {
    CantDecode(bytes, num_bytes, num_handles, dispatcher, line_header, tabs, os);
  }
}

void SyscallDecoderDispatcher::DecodeSyscall(InterceptingThreadObserver* thread_observer,
                                             zxdb::Thread* thread, Syscall* syscall) {
  uint64_t thread_id = thread->GetKoid();
  auto current = syscall_decoders_.find(thread_id);
  if (current != syscall_decoders_.end()) {
    FXL_LOG(INFO) << "internal error: already decoding thread " << thread_id;
    return;
  }
  auto decoder = CreateDecoder(thread_observer, thread, thread_id, syscall);
  auto tmp = decoder.get();
  syscall_decoders_[thread_id] = std::move(decoder);
  tmp->Decode();
}

void SyscallDecoderDispatcher::DecodeException(InterceptionWorkflow* workflow,
                                               zxdb::Thread* thread) {
  uint64_t thread_id = thread->GetKoid();
  auto current = exception_decoders_.find(thread_id);
  if (current != exception_decoders_.end()) {
    FXL_LOG(INFO) << "internal error: already decoding an exception for thread " << thread_id;
    return;
  }
  auto decoder = CreateDecoder(workflow, thread, thread_id);
  auto tmp = decoder.get();
  exception_decoders_[thread_id] = std::move(decoder);
  tmp->Decode();
}

void SyscallDecoderDispatcher::DeleteDecoder(SyscallDecoder* decoder) {
  decoder->thread()->Continue();
  syscall_decoders_.erase(decoder->thread_id());
}

void SyscallDecoderDispatcher::DeleteDecoder(ExceptionDecoder* decoder) {
  decoder->thread()->Continue();
  exception_decoders_.erase(decoder->thread_id());
}

std::unique_ptr<SyscallDecoder> SyscallDisplayDispatcher::CreateDecoder(
    InterceptingThreadObserver* thread_observer, zxdb::Thread* thread, uint64_t thread_id,
    const Syscall* syscall) {
  return std::make_unique<SyscallDecoder>(this, thread_observer, thread, thread_id, syscall,
                                          std::make_unique<SyscallDisplay>(this, os_));
}

std::unique_ptr<ExceptionDecoder> SyscallDisplayDispatcher::CreateDecoder(
    InterceptionWorkflow* workflow, zxdb::Thread* thread, uint64_t thread_id) {
  return std::make_unique<ExceptionDecoder>(workflow, this, thread->GetProcess()->GetKoid(), thread,
                                            thread_id,
                                            std::make_unique<ExceptionDisplay>(this, os_));
}

void SyscallDisplayDispatcher::ProcessLaunched(const std::string& command,
                                               std::string_view error_message) {
  last_displayed_syscall_ = nullptr;
  if (error_message.empty()) {
    os_ << colors().green << "\nLaunched " << colors().blue << command << colors().reset << '\n';
  } else {
    os_ << colors().red << "\nCan't launch " << colors().blue << command << colors().reset << " : "
        << colors().red << error_message << colors().reset << '\n';
  }
}

void SyscallDisplayDispatcher::ProcessMonitored(std::string_view name, zx_koid_t koid,
                                                std::string_view error_message) {
  last_displayed_syscall_ = nullptr;
  if (error_message.empty()) {
    os_ << colors().green << "\nMonitoring ";
  } else {
    os_ << colors().red << "\nCan't monitor ";
  }

  if (name.empty()) {
    os_ << colors().reset << "process with koid ";
  } else {
    os_ << colors().blue << name << colors().reset << " koid=";
  }

  os_ << colors().red << koid << colors().reset;
  if (!error_message.empty()) {
    os_ << " : " << colors().red << error_message << colors().reset;
  }
  os_ << '\n';
}

void SyscallDisplayDispatcher::StopMonitoring(zx_koid_t koid) {
  last_displayed_syscall_ = nullptr;
  os_ << colors().green << "\nStop monitoring process with koid ";
  os_ << colors().red << koid << colors().reset << '\n';
}

}  // namespace fidlcat

// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "util.h"

#include <ctype.h>
#include <lib/boot-options/word-view.h>
#include <lib/stdcompat/string_view.h>
#include <string.h>
#include <zircon/assert.h>
#include <zircon/boot/image.h>
#include <zircon/process.h>
#include <zircon/processargs.h>
#include <zircon/status.h>

#include <optional>
#include <string>
#include <string_view>

#include <fbl/algorithm.h>
#include <safemath/checked_math.h>

#include "src/lib/storage/vfs/cpp/vfs_types.h"
#include "zircon/device/vfs.h"

namespace {

constexpr std::string_view kBootsvcNextArg = "bootsvc.next=";

// Returns true for boot item types that should be stored.
bool StoreItem(uint32_t type) {
  switch (type) {
    case ZBI_TYPE_CMDLINE:
    case ZBI_TYPE_CRASHLOG:
    case ZBI_TYPE_KERNEL_DRIVER:
    case ZBI_TYPE_PLATFORM_ID:
    case ZBI_TYPE_STORAGE_RAMDISK:
    case ZBI_TYPE_IMAGE_ARGS:
    case ZBI_TYPE_SERIAL_NUMBER:
    case ZBI_TYPE_BOOTLOADER_FILE:
    case ZBI_TYPE_DEVICETREE:
      return true;
    default:
      return ZBI_TYPE_DRV_METADATA(type);
  }
}

// Discards the boot item from the boot image VMO.
void DiscardItem(zx::vmo* vmo, uint32_t begin_in, uint32_t end_in) {
  uint64_t begin = fbl::round_up(begin_in, static_cast<uint32_t>(zx_system_get_page_size()));
  uint64_t end = fbl::round_down(end_in, static_cast<uint32_t>(zx_system_get_page_size()));
  if (begin < end) {
    printf(
        "bootsvc: Would have decommitted BOOTDATA VMO from %#lx to %#lx, but deferring to "
        "component manager's ZBI parser instead\n",
        begin, end);
  }
}

bootsvc::ItemKey CreateItemKey(uint32_t type, uint32_t extra) {
  switch (type) {
    case ZBI_TYPE_STORAGE_RAMDISK:
      // If this is for a ramdisk, set the extra value to zero.
      return bootsvc::ItemKey{.type = type, .extra = 0};
    default:
      // Otherwise, store the extra value.
      return bootsvc::ItemKey{.type = type, .extra = extra};
  }
}

bootsvc::ItemValue CreateItemValue(uint32_t type, uint32_t off, uint32_t len) {
  switch (type) {
    case ZBI_TYPE_STORAGE_RAMDISK:
      // If this is for a ramdisk, capture the ZBI header.
      len = safemath::CheckAdd(len, sizeof(zbi_header_t)).ValueOrDie<uint32_t>();
      break;
    default:
      // Otherwise, adjust the offset to skip the ZBI header.
      off = safemath::CheckAdd(off, sizeof(zbi_header_t)).ValueOrDie<uint32_t>();
  }
  return bootsvc::ItemValue{.offset = off, .length = len};
}

zx_status_t ProcessBootloaderFile(const zx::vmo& vmo, uint32_t offset, uint32_t length,
                                  std::string* out_filename,
                                  bootsvc::ItemValue* out_bootloader_file) {
  offset = safemath::CheckAdd(offset, sizeof(zbi_header_t)).ValueOrDie<uint32_t>();

  if (length < sizeof(uint8_t)) {
    printf("bootsvc: Bootloader File ZBI item too small\n");
    return ZX_ERR_OUT_OF_RANGE;
  }

  uint8_t name_len;
  zx_status_t status = vmo.read(&name_len, offset, sizeof(uint8_t));
  if (status != ZX_OK) {
    printf("bootsvc: Failed to read input VMO: %s\n", zx_status_get_string(status));
    return status;
  }

  if (length <= sizeof(uint8_t) + name_len) {
    printf("bootsvc: Bootloader File ZBI item too small.\n");
    return ZX_ERR_OUT_OF_RANGE;
  }

  std::string name(name_len, '\0');

  offset = safemath::CheckAdd(offset, sizeof(uint8_t)).ValueOrDie<uint32_t>();
  status = vmo.read(name.data(), offset, name_len);
  if (status != ZX_OK) {
    printf("bootsvc: Failed to read input VMO: %s\n", zx_status_get_string(status));
    return status;
  }

  offset = safemath::CheckAdd(offset, name_len).ValueOrDie<uint32_t>();
  uint32_t payload_length = length - name_len - sizeof(uint8_t);

  *out_bootloader_file = bootsvc::ItemValue{.offset = offset, .length = payload_length};
  *out_filename = std::move(name);
  return ZX_OK;
}

}  // namespace

namespace bootsvc {

const char* const kLastPanicFilePath = "log/last-panic.txt";

zx_status_t RetrieveBootImage(zx::vmo vmo, zx::vmo* out_vmo, ItemMap* out_map,
                              BootloaderFileMap* out_bootloader_file_map) {
  // Validate boot image VMO provided by startup handle.
  zbi_header_t header;
  zx_status_t status = vmo.read(&header, 0, sizeof(header));
  if (status != ZX_OK) {
    printf("bootsvc: Failed to read ZBI image header: %s\n", zx_status_get_string(status));
    return status;
  } else if (header.type != ZBI_TYPE_CONTAINER || header.extra != ZBI_CONTAINER_MAGIC ||
             header.magic != ZBI_ITEM_MAGIC || !(header.flags & ZBI_FLAG_VERSION)) {
    printf("bootsvc: Invalid ZBI image header\n");
    return ZX_ERR_IO_DATA_INTEGRITY;
  }

  // Used to discard pages from the boot image VMO.
  uint32_t discard_begin = 0;
  uint32_t discard_end = 0;

  // Read boot items from the boot image VMO.
  ItemMap map;
  BootloaderFileMap bootloader_file_map;

  uint32_t off = sizeof(header);
  uint32_t len = header.length;
  while (len > sizeof(header)) {
    status = vmo.read(&header, off, sizeof(header));
    if (status != ZX_OK) {
      printf("bootsvc: Failed to read ZBI item header: %s\n", zx_status_get_string(status));
      return status;
    } else if (header.type == ZBI_CONTAINER_MAGIC || header.magic != ZBI_ITEM_MAGIC) {
      printf("bootsvc: Invalid ZBI item header\n");
      return ZX_ERR_IO_DATA_INTEGRITY;
    }
    uint32_t item_len = ZBI_ALIGN(header.length + static_cast<uint32_t>(sizeof(zbi_header_t)));
    uint32_t next_off = safemath::CheckAdd(off, item_len).ValueOrDie();

    if (item_len > len) {
      printf("bootsvc: ZBI item too large (%u > %u)\n", item_len, len);
      return ZX_ERR_IO_DATA_INTEGRITY;
    } else if (StoreItem(header.type)) {
      if (header.type == ZBI_TYPE_BOOTLOADER_FILE) {
        std::string filename;
        ItemValue bootloader_file;

        status = ProcessBootloaderFile(vmo, off, header.length, &filename, &bootloader_file);
        if (status == ZX_OK) {
          bootloader_file_map.emplace(std::move(filename), std::move(bootloader_file));
        } else {
          printf("bootsvc: Failed to process bootloader file: %s\n", zx_status_get_string(status));
        }

      } else {
        const ItemKey key = CreateItemKey(header.type, header.extra);
        const ItemValue val = CreateItemValue(header.type, off, header.length);
        auto [it, _] = map.emplace(key, std::vector<ItemValue>{});
        it->second.push_back(val);
      }
      DiscardItem(&vmo, discard_begin, discard_end);
      discard_begin = next_off;
    } else {
      discard_end = next_off;
    }
    off = next_off;
    len = safemath::CheckSub(len, item_len).ValueOrDie();
  }

  if (discard_end > discard_begin) {
    // We are at the end of the last element and it should be discarded.
    // We should discard until the end of the page.
    discard_end = fbl::round_up(discard_end, static_cast<uint32_t>(zx_system_get_page_size()));
  }

  DiscardItem(&vmo, discard_begin, discard_end);
  *out_vmo = std::move(vmo);
  *out_map = std::move(map);
  *out_bootloader_file_map = std::move(bootloader_file_map);
  return ZX_OK;
}

std::optional<std::string> ParseBootArgs(std::string_view str, std::vector<char>* buf) {
  buf->reserve(buf->size() + str.size());
  std::optional<std::string> next;
  for (std::string_view word : WordView(str)) {
    for (char c : word) {
      buf->push_back(c);
    }
    buf->push_back('\0');

    if (cpp20::starts_with(word, kBootsvcNextArg)) {
      next = word.substr(kBootsvcNextArg.size());
    }
  }
  return next;
}

zx_status_t ParseLegacyBootArgs(std::string_view str, std::vector<char>* buf) {
  buf->reserve(buf->size() + str.size());
  for (auto it = str.begin(); it != str.end();) {
    // Skip any leading whitespace.
    if (isspace(*it)) {
      it++;
      continue;
    }
    // Is the line a comment or a zero-length name?
    bool is_comment = *it == '#' || *it == '=';
    // Append line, if it is not a comment.
    for (; it != str.end(); it++) {
      if (*it == '\n') {
        // We've reached the end of the line.
        it++;
        break;
      } else if (is_comment) {
        // Skip this character, as it is part of a comment.
        continue;
      } else if (isspace(*it)) {
        // It is invalid to have a space within an argument.
        return ZX_ERR_INVALID_ARGS;
      } else {
        buf->push_back(*it);
      }
    }
    if (!is_comment) {
      buf->push_back(0);
    }
  }
  return ZX_OK;
}

zx_status_t CreateVnodeConnection(fs::FuchsiaVfs* vfs, fbl::RefPtr<fs::Vnode> vnode,
                                  fs::Rights rights, zx::channel* out) {
  zx::channel local, remote;
  zx_status_t status = zx::channel::create(0, &local, &remote);
  if (status != ZX_OK) {
    return status;
  }

  status = vfs->ServeDirectory(std::move(vnode), std::move(local), rights);
  if (status != ZX_OK) {
    return status;
  }
  *out = std::move(remote);
  return ZX_OK;
}

std::vector<std::string> SplitString(std::string_view input, char delimiter) {
  const bool ends_with_delimiter = !input.empty() && input.back() == delimiter;

  std::vector<std::string> result;
  while (!input.empty()) {
    std::string_view word = input.substr(0, input.find_first_of(delimiter));
    result.push_back(std::string{word});
    input.remove_prefix(std::min(input.size(), word.size() + sizeof(delimiter)));
  }

  if (ends_with_delimiter) {
    result.push_back("");
  }

  return result;
}

}  // namespace bootsvc

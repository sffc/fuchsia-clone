// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/memory.h>
#include <lib/stdcompat/span.h>

#include <string_view>

#include <zxtest/zxtest.h>

namespace {

using namespace std::literals;

constexpr uintptr_t kBaseAddress = 0x12340;
constexpr std::string_view kHeaderBytes = "HeaderOf16Bytes\0"sv;
struct Header {
  char bytes_[16];
};

TEST(ElfldltlDirectMemoryTests, FileApi) {
  char file_image[] = "HeaderOf16Bytes\0Dataaabb";
  auto image_bytes = cpp20::as_writable_bytes(cpp20::span(file_image));
  elfldltl::DirectMemory file(image_bytes, kBaseAddress);

  auto header = file.ReadFromFile<Header>(0);
  ASSERT_TRUE(header.has_value());
  std::string_view header_bytes(header->get().bytes_, 16);
  EXPECT_EQ(header_bytes, kHeaderBytes);

  auto bad_offset = file.ReadFromFile<uint32_t>(30);
  EXPECT_FALSE(bad_offset.has_value());

  auto array = file.ReadArrayFromFile<char, 32>(16, 4);
  ASSERT_TRUE(array.has_value());
  EXPECT_EQ(4, array->size());
  EXPECT_STREQ(std::string_view(array->data(), array->size()), "Data");

  auto array2 = file.ReadArrayFromFile<uint16_t, 32>(20, 2);
  ASSERT_TRUE(array2.has_value());
  ASSERT_EQ(2, array2->size());
  EXPECT_EQ(uint16_t{('a' << 8) | 'a'}, (*array2)[0]);
  EXPECT_EQ(uint16_t{('b' << 8) | 'b'}, (*array2)[1]);

  auto bad_array = file.ReadArrayFromFile<uint32_t, 36>(24, 36);
  EXPECT_FALSE(bad_array.has_value());
}

TEST(ElfldltlDirectMemoryTests, MemoryApi) {
  char file_image[] = "HeaderOf16Bytes\0Dataaabb";
  auto image_bytes = cpp20::as_writable_bytes(cpp20::span(file_image));
  elfldltl::DirectMemory file(image_bytes, kBaseAddress);

  auto array = file.ReadArray<char>(kBaseAddress + 16, 4);
  ASSERT_TRUE(array.has_value());
  EXPECT_EQ(4, array->size());
  EXPECT_STREQ(std::string_view(array->data(), array->size()), "Data");

  auto bad_address_low = file.ReadArray<uint64_t>(kBaseAddress - 4, 16);
  EXPECT_FALSE(bad_address_low.has_value());

  auto bad_address_high = file.ReadArray<uint64_t>(kBaseAddress + 40, 16);
  EXPECT_FALSE(bad_address_high.has_value());

  auto unbounded = file.ReadArray<char>(kBaseAddress + 16);
  ASSERT_TRUE(unbounded.has_value());
  EXPECT_EQ(9, unbounded->size());
  EXPECT_EQ(array->data(), unbounded->data());

  EXPECT_TRUE(file.Store<uint32_t>(kBaseAddress + 16, 0xaabbccdd));
  EXPECT_EQ(0xaabbccdd, *reinterpret_cast<uint32_t*>(file_image + 16));

  EXPECT_TRUE(file.StoreAdd<uint32_t>(kBaseAddress + 16, 0x11111111));
  EXPECT_EQ(0xbbccddee, *reinterpret_cast<uint32_t*>(file_image + 16));

  EXPECT_FALSE(file.Store<uint32_t>(kBaseAddress - 4, 0x12345678));
  EXPECT_FALSE(file.Store<uint32_t>(kBaseAddress + 40, 0x12345678));

  EXPECT_FALSE(file.StoreAdd<uint32_t>(kBaseAddress - 4, 0x12345678));
  EXPECT_FALSE(file.StoreAdd<uint32_t>(kBaseAddress + 40, 0x12345678));
}

}  // namespace

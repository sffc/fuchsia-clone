// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/symbols/index.h"

#include <inttypes.h>
#include <time.h>

#include <ostream>
#include <sstream>

#include "gtest/gtest.h"
#include "src/developer/debug/zxdb/common/string_util.h"
#include "src/developer/debug/zxdb/symbols/dwarf_symbol_factory.h"
#include "src/developer/debug/zxdb/symbols/module_symbols_impl.h"
#include "src/developer/debug/zxdb/symbols/test_symbol_module.h"
#include "src/lib/fxl/strings/split_string.h"

namespace zxdb {

// Generates the symbol index of our simple test app. This may get updated if we change things
// but the important thing is that when this happens to check that the new index makes sense and
// then add it.
TEST(Index, IndexDump) {
  auto module = fxl::MakeRefCounted<ModuleSymbolsImpl>(TestSymbolModule::GetCheckedInTestFileName(),
                                                       "test", "build_id");
  Err err = module->Load(false);
  ASSERT_TRUE(err.ok()) << err.msg();

  Index index;
  index.CreateIndex(module->object_file());

  // Symbol index.
  std::ostringstream out;
  index.root().Dump(out, module->symbol_factory(), 0);
  const char kExpected[] = R"(  Namespaces:
    <<empty index string>>
      Functions:
        AnonNSFunction: {[0x1450, 0x145f)}
        LineLookupTest<0>: {[0x1040, 0x104f)}
        LineLookupTest<1>: {[0x1050, 0x105d)}
    my_ns
      Types:
        Base1: 0x26a
        Base2: 0x280
        MyClass: 0x733
          Types:
            Inner: 0x75e
              Functions:
                MyMemberTwo: {[0x1370, 0x137b)}
          Functions:
            MyMemberOne: {[0x1460, 0x146f)}
          Variables:
            kClassStatic: 0x720
        Struct: 0x1f3
          Functions:
            MyFunc: {[0x1310, 0x1323)}
          Variables:
            kConstInt: 0x22d
            kConstLongDouble: 0x239
        StructMemberPtr: 0x35a
        TypeForUsing: 0x1a3
      Functions:
        DoStructCall: {[0x1100, 0x114c)}
        GetStruct: {[0x10a0, 0x10c5)}
        GetStructMemberPtr: {[0x10d0, 0x10e1)}
        InlinedFunction: {[0x1150, 0x115f)}
        NamespaceFunction: {[0x1380, 0x138b)}
        PassRValueRef: {[0x10f0, 0x10fa)}
      Variables:
        kGlobal: 0x707
    std
      Types:
        nullptr_t: 0x186
  Types:
    ClassInTest2: 0x873
      Functions:
        FunctionInTest2: {[0x1470, 0x147b)}
    ForInline: 0x4d4
      Functions:
        ForInline: {[0x1350, 0x1364)}
    MyTemplate<my_ns::Struct, 42>: 0x465
      Functions:
        MyTemplate: {[0x1330, 0x1345)}
    StructWithEnums: 0x104
      Types:
        RegularEnum: 0x119
        TypedEnum: 0x152
    __ARRAY_SIZE_TYPE__: 0x6a0
    char: 0x3d7
    int: 0xd2
    long double: 0x3cb
    signed char: 0x17a
    unsigned int: 0x16c
  Functions:
    CallInline: {[0x1290, 0x12a8)}
    CallInlineMember: {[0x11e0, 0x1287)}
    DoLineLookupTest: {[0x1000, 0x1034)}
    GetIntPtr: {[0x1060, 0x1068)}
    GetNullPtrT: {[0x12e0, 0x12e8)}
    GetString: {[0x1070, 0x1099)}
    GetStructWithEnums: {[0x12b0, 0x12da)}
    GetTemplate: {[0x1190, 0x11d6)}
    GetUsing: {[0x12f0, 0x1309)}
    My2DArray: {[0x1160, 0x118b)}
    MyFunction: {[0x1390, 0x1446)}
)";
  EXPECT_EQ(kExpected, out.str());

  // File index.
  std::ostringstream files;
  index.DumpFileIndex(files);
  const char kExpectedFiles[] =
      R"(line_lookup_symbol_test.cc -> ../../src/developer/debug/zxdb/symbols/test_data/line_lookup_symbol_test.cc -> 1 units
type_test.cc -> ../../src/developer/debug/zxdb/symbols/test_data/type_test.cc -> 1 units
zxdb_symbol_test.cc -> ../../src/developer/debug/zxdb/symbols/test_data/zxdb_symbol_test.cc -> 1 units
zxdb_symbol_test2.cc -> ../../src/developer/debug/zxdb/symbols/test_data/zxdb_symbol_test2.cc -> 1 units
)";
  EXPECT_EQ(kExpectedFiles, files.str());

  // Test that the slow indexing path produces the same result as the fast path.
  Index slow_index;
  slow_index.CreateIndex(module->object_file(), true);
  out = std::ostringstream();
  slow_index.root().Dump(out, module->symbol_factory(), 0);
  EXPECT_EQ(kExpected, out.str());
}

TEST(Index, FindExactFunction) {
  auto module = fxl::MakeRefCounted<ModuleSymbolsImpl>(TestSymbolModule::GetCheckedInTestFileName(),
                                                       "test", "build_id");
  Err err = module->Load(false);
  ASSERT_TRUE(err.ok()) << err.msg();

  Index index;
  index.CreateIndex(module->object_file());

  // Standalone function search.
  auto result = index.FindExact(TestSymbolModule::SplitName(TestSymbolModule::kMyFunctionName));
  EXPECT_EQ(1u, result.size()) << "Symbol not found: " << TestSymbolModule::kMyFunctionName;

  // Standalone function inside a named namespace.
  result = index.FindExact(TestSymbolModule::SplitName(TestSymbolModule::kNamespaceFunctionName));
  EXPECT_EQ(1u, result.size()) << "Symbol not found: " << TestSymbolModule::kNamespaceFunctionName;

  // Standalone function inside an anonymous namespace. Currently this is indexed as if the
  // anonymous namespace wasn't there, but this may need to change in the future.
  result = index.FindExact(TestSymbolModule::SplitName(TestSymbolModule::kAnonNSFunctionName));
  EXPECT_EQ(1u, result.size()) << "Symbol not found: " << TestSymbolModule::kAnonNSFunctionName;

  // Namespace + class member function search.
  result = index.FindExact(TestSymbolModule::SplitName(TestSymbolModule::kMyMemberOneName));
  EXPECT_EQ(1u, result.size()) << "Symbol not found: " << TestSymbolModule::kMyMemberOneName;

  // Same but in the 2nd compilation unit (tests unit-relative addressing).
  result = index.FindExact(TestSymbolModule::SplitName(TestSymbolModule::kFunctionInTest2Name));
  EXPECT_EQ(1u, result.size()) << "Symbol not found: " << TestSymbolModule::kFunctionInTest2Name;

  // Namespace + class + struct with static member function search.
  result = index.FindExact(TestSymbolModule::SplitName(TestSymbolModule::kMyMemberTwoName));
  EXPECT_EQ(1u, result.size()) << "Symbol not found: " << TestSymbolModule::kMyMemberTwoName;

  // Global variable.
  result = index.FindExact(TestSymbolModule::SplitName(TestSymbolModule::kGlobalName));
  EXPECT_EQ(1u, result.size()) << "Symbol not found: " << TestSymbolModule::kGlobalName;

  // Class static variable.
  result = index.FindExact(TestSymbolModule::SplitName(TestSymbolModule::kClassStaticName));
  EXPECT_EQ(1u, result.size()) << "Symbol not found: " << TestSymbolModule::kClassStaticName;

  // Something not found.
  result = index.FindExact(TestSymbolModule::SplitName("my_ns::MyClass::NotFoundThing"));
  EXPECT_TRUE(result.empty());
}

TEST(Index, FindFileMatches) {
  auto module = fxl::MakeRefCounted<ModuleSymbolsImpl>(TestSymbolModule::GetCheckedInTestFileName(),
                                                       "test", "build_id");
  Err err = module->Load(false);
  ASSERT_TRUE(err.ok()) << err.msg();

  Index index;
  index.CreateIndex(module->object_file());

  // Simple filename-only query that succeeds.
  std::vector<std::string> result = index.FindFileMatches("zxdb_symbol_test.cc");
  ASSERT_EQ(1u, result.size());
  EXPECT_TRUE(StringEndsWith(result[0], "symbols/test_data/zxdb_symbol_test.cc"));

  // Save the full path for later.
  std::string full_path = result[0];

  // Simple filename-only query that fails.
  result = index.FindFileMatches("nonexistant.cc");
  EXPECT_EQ(0u, result.size());

  // Multiple path components.
  result = index.FindFileMatches("symbols/test_data/zxdb_symbol_test.cc");
  EXPECT_EQ(1u, result.size());

  // Ends-with match but doesn't start on a slash boundary.
  result = index.FindFileMatches("nt/test_data/zxdb_symbol_test.cc");
  EXPECT_EQ(0u, result.size());

  // Full path match.
  result = index.FindFileMatches(full_path);
  EXPECT_EQ(1u, result.size());

  // More-than-full path match.
  result = index.FindFileMatches("/a" + full_path);
  EXPECT_EQ(0u, result.size());
}

TEST(Index, FindFilePrefixes) {
  auto module = fxl::MakeRefCounted<ModuleSymbolsImpl>(TestSymbolModule::GetCheckedInTestFileName(),
                                                       "test", "build_id");
  Err err = module->Load(false);
  ASSERT_TRUE(err.ok()) << err.msg();

  Index index;
  index.CreateIndex(module->object_file());

  // Should find both files. Order not guaranteed.
  std::vector<std::string> result = index.FindFilePrefixes("z");
  ASSERT_EQ(2u, result.size());
  EXPECT_NE(result.end(), std::find(result.begin(), result.end(), "zxdb_symbol_test.cc"));
  EXPECT_NE(result.end(), std::find(result.begin(), result.end(), "zxdb_symbol_test2.cc"));
}

// Enable and substitute a path on your system to dump the index for a DWARF file.
#if 0
TEST(Index, DumpIndex) {
  auto module= fxl::MakeRefCounted<ModuleSymbolsImpl>("chrome",
  "test", "build_id");
  Err err = module->Load(false);
  ASSERT_TRUE(err.ok()) << err.msg();

  Index index;
  index.CreateIndex(module->object_file());

  std::cout << index.main_functions().size() << " main function(s) found.\n\n";

  std::cout << "Symbol index dump:\n";
  index.root().Dump(std::cout, 1);

  std::cout << "File index dump:\n";
  index.DumpFileIndex(std::cout);
}
#endif

// Enable and substitute a path on your system for kFilename to run the
// indexing benchmark.
#if 0
static int64_t GetTickMicroseconds() {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);

  constexpr int64_t kMicrosecondsPerSecond = 1000000;
  constexpr int64_t kNanosecondsPerMicrosecond = 1000;

  int64_t result = ts.tv_sec * kMicrosecondsPerSecond;
  result += (ts.tv_nsec / kNanosecondsPerMicrosecond);
  return result;
}

TEST(Index, BenchmarkIndexing) {
  const char kFilename[] = "chrome";
  int64_t begin_us = GetTickMicroseconds();

  auto module = fxl::MakeRefCounted<ModuleSymbolsImpl>(kFilename, "test", "build_id");
  Err err = module->Load(false);
  ASSERT_TRUE(err.ok()) << err.msg();

  int64_t load_complete_us = GetTickMicroseconds();

  Index index;
  index.CreateIndex(module->object_file());

  int64_t index_complete_us = GetTickMicroseconds();

  printf("\nIndexing results for %s:\n   Load: %" PRId64
         " µs\n  Index: %" PRId64 " µs\n\n",
         kFilename, load_complete_us - begin_us,
         index_complete_us - load_complete_us);

  sleep(10);
}
#endif  // End indexing benchmark.

}  // namespace zxdb

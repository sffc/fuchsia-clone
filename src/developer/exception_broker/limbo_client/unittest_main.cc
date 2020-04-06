// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include <gtest/gtest.h>

#include "src/lib/fxl/test/test_settings.h"
#include "src/lib/syslog/cpp/logger.h"

int main(int argc, char* argv[]) {
  if (!fxl::SetTestSettings(argc, argv))
    return EXIT_FAILURE;

  testing::InitGoogleTest(&argc, argv);
  syslog::SetTags({"exception-broker", "unittest"});

  return RUN_ALL_TESTS();
}

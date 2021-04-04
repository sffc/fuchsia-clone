// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_LAUNCH_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_LAUNCH_H_
#include "dap/typeof.h"
#include "src/developer/debug/zxdb/debug_adapter/context.h"

namespace dap {

class LaunchRequestZxdb : public LaunchRequest {
 public:
  // Name of the component or process that will be launched
  string process;
  // Shell command to launch the program. This is a list of args with the first argument being the
  // command.
  array<string> runCommand;
  // Current working directory for running the shell command.
  optional<string> cwd;
};

DAP_DECLARE_STRUCT_TYPEINFO(LaunchRequestZxdb);

}  // namespace dap

namespace zxdb {

dap::ResponseOrError<dap::LaunchResponse> OnRequestLaunch(DebugAdapterContext* context,
                                                          const dap::LaunchRequestZxdb& req);
}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_LAUNCH_H_

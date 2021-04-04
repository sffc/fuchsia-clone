// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_CONTINUE_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_CONTINUE_H_
#include "src/developer/debug/zxdb/debug_adapter/context.h"

namespace zxdb {

dap::ResponseOrError<dap::ContinueResponse> OnRequestContinue(DebugAdapterContext* ctx,
                                                              const dap::ContinueRequest& request);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_CONTINUE_H_

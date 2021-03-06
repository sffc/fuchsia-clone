// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_ZX_HANDLE_H_
#define LIB_ZX_HANDLE_H_

#include <lib/zx/object.h>
#include <zircon/availability.h>

namespace zx {

using handle = object<void> ZX_AVAILABLE_SINCE(7);
using unowned_handle = unowned<handle> ZX_AVAILABLE_SINCE(7);

}  // namespace zx

#endif  // LIB_ZX_HANDLE_H_

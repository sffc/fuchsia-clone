// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef GFX_COMMON_GFX_FONT_H_
#define GFX_COMMON_GFX_FONT_H_

#include <stdint.h>

typedef struct gfx_font {
  uint32_t width, height;
  uint16_t data[];
} gfx_font;

#endif  // GFX_COMMON_GFX_FONT_H_

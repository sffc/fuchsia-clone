// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_FOSTR_FIDL_FUCHSIA_MATH_AMENDMENTS_H_
#define SRC_LIB_FOSTR_FIDL_FUCHSIA_MATH_AMENDMENTS_H_

#include <fuchsia/math/cpp/fidl.h>

#include <iosfwd>

namespace fuchsia {
namespace math {

// NOTE:
// //src/lib/fostr/fidl/fuchsia.math automatically generates ostream
// formatters for fuchsia.math *except* those formatters that are listed here.
// The code generator knows which formatters to exclude from the generated code
// by consulting the 'amendments.json' file.
//
// If you add or remove formatters from this file, please be sure that the
// amendments.json file is updated accordingly.

std::ostream& operator<<(std::ostream& os, const Point& value);
std::ostream& operator<<(std::ostream& os, const PointF& value);
std::ostream& operator<<(std::ostream& os, const Point3F& value);
std::ostream& operator<<(std::ostream& os, const Rect& value);
std::ostream& operator<<(std::ostream& os, const RectF& value);
std::ostream& operator<<(std::ostream& os, const RRectF& value);
std::ostream& operator<<(std::ostream& os, const Size& value);
std::ostream& operator<<(std::ostream& os, const SizeF& value);
std::ostream& operator<<(std::ostream& os, const Inset& value);
std::ostream& operator<<(std::ostream& os, const InsetF& value);
std::ostream& operator<<(std::ostream& os, const Transform& value);

}  // namespace math
}  // namespace fuchsia

#endif  // SRC_LIB_FOSTR_FIDL_FUCHSIA_MATH_AMENDMENTS_H_

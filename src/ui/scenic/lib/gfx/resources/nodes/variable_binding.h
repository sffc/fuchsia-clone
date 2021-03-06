// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_GFX_RESOURCES_NODES_VARIABLE_BINDING_H_
#define SRC_UI_SCENIC_LIB_GFX_RESOURCES_NODES_VARIABLE_BINDING_H_

#include <lib/fit/function.h>

#include "src/ui/scenic/lib/gfx/resources/variable.h"

namespace scenic_impl {
namespace gfx {

class Node;

// Binds a Variable to a particular callback function. Observes when the
// Variable's value changes and invokes the callback. The act of creating the
// binding automatically sets the value of the target.
class VariableBinding {
 public:
  virtual ~VariableBinding(){}
};

template <::fuchsia::ui::gfx::Value::Tag VT, typename T>
class TypedVariableBinding : public VariableBinding, public OnVariableValueChangedListener<VT, T> {
 public:
  TypedVariableBinding(fxl::RefPtr<TypedVariable<VT, T>> variable,
                       fit::function<void(T value)> on_value_changed_callback);
  virtual ~TypedVariableBinding();

 private:
  void OnVariableValueChanged(TypedVariable<VT, T>* v) override;

  fxl::RefPtr<TypedVariable<VT, T>> variable_;
  fit::function<void(T value)> on_value_changed_callback_;
};

using Vector3VariableBinding =
    TypedVariableBinding<::fuchsia::ui::gfx::Value::Tag::kVector3, escher::vec3>;
using QuaternionVariableBinding =
    TypedVariableBinding<::fuchsia::ui::gfx::Value::Tag::kQuaternion, escher::quat>;

}  // namespace gfx
}  // namespace scenic_impl

#endif  // SRC_UI_SCENIC_LIB_GFX_RESOURCES_NODES_VARIABLE_BINDING_H_

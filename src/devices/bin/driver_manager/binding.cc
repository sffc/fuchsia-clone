// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ddk/binding.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <stdio.h>

#include <fbl/array.h>

#include "binding_internal.h"
#include "coordinator.h"
#include "device.h"
#include "src/devices/lib/bind/ffi_bindings.h"
#include "src/devices/lib/log/log.h"
#include "src/lib/fxl/strings/utf_codecs.h"

namespace internal {

uint32_t LookupBindProperty(BindProgramContext* ctx, uint32_t id) {
  for (const auto prop : *ctx->props) {
    if (prop.id == id) {
      return prop.value;
    }
  }

  // fallback for devices without properties
  switch (id) {
    case BIND_PROTOCOL:
      return ctx->protocol_id;
    case BIND_AUTOBIND:
      return ctx->autobind;
    default:
      // TODO: better process for missing properties
      return 0;
  }
}

bool EvaluateBindProgram(BindProgramContext* ctx) {
  const zx_bind_inst_t* ip = ctx->binding;
  const zx_bind_inst_t* end = ip + (ctx->binding_size / sizeof(zx_bind_inst_t));
  uint32_t flags = 0;

  while (ip < end) {
    uint32_t inst = ip->op;
    bool cond;

    if (BINDINST_CC(inst) != COND_AL) {
      uint32_t value = ip->arg;
      uint32_t pid = BINDINST_PB(inst);
      uint32_t pval;
      if (pid != BIND_FLAGS) {
        pval = LookupBindProperty(ctx, pid);
      } else {
        pval = flags;
      }

      // evaluate condition
      switch (BINDINST_CC(inst)) {
        case COND_EQ:
          cond = (pval == value);
          break;
        case COND_NE:
          cond = (pval != value);
          break;
        case COND_LT:
        case COND_GT:
        case COND_LE:
        case COND_GE:
          LOGF(ERROR, "Driver '%s' has deprecated inequality bind instruction %#08x", ctx->name,
               inst);
          return false;
        default:
          // illegal instruction: abort
          LOGF(ERROR, "Driver '%s' has illegal bind instruction %#08x", ctx->name, inst);
          return false;
      }
    } else {
      cond = true;
    }

    if (cond) {
      switch (BINDINST_OP(inst)) {
        case OP_ABORT:
          return false;
        case OP_MATCH:
          return true;
        case OP_GOTO: {
          uint32_t label = BINDINST_PA(inst);
          while (++ip < end) {
            if ((BINDINST_OP(ip->op) == OP_LABEL) && (BINDINST_PA(ip->op) == label)) {
              goto next_instruction;
            }
          }
          LOGF(ERROR, "Driver '%s' illegal GOTO", ctx->name);
          return false;
        }
        case OP_LABEL:
          // no op
          break;
        default:
          // illegal instruction: abort
          LOGF(ERROR, "Driver '%s' illegal bind instruction %#08x", ctx->name, inst);
          return false;
      }
    }

  next_instruction:
    ip++;
  }

  // default if we leave the program is no-match
  return false;
}

}  // namespace internal

bool can_driver_bind(const Driver* drv, uint32_t protocol_id,
                     const fbl::Array<const zx_device_prop_t>& props,
                     const fbl::Array<const StrProperty>& str_props, bool autobind) {
  if (drv->bytecode_version == 1) {
    auto* binding = std::get_if<std::unique_ptr<zx_bind_inst_t[]>>(&drv->binding);
    if (!binding && drv->binding_size > 0) {
      return false;
    }

    internal::BindProgramContext ctx;
    ctx.props = &props;
    ctx.protocol_id = protocol_id;
    ctx.binding = binding ? binding->get() : nullptr;
    ctx.binding_size = drv->binding_size;
    ctx.name = drv->name.c_str();
    ctx.autobind = autobind ? 1 : 0;
    return internal::EvaluateBindProgram(&ctx);
  } else if (drv->bytecode_version == 2) {
    auto* bytecode = std::get_if<std::unique_ptr<uint8_t[]>>(&drv->binding);
    if (!bytecode && drv->binding_size > 0) {
      return false;
    }

    fbl::Array<device_property_t> properties(new device_property_t[props.size()], props.size());
    for (size_t i = 0; i < props.size(); i++) {
      properties[i] = device_property_t{.key = props[i].id, .value = props[i].value};
    }

    fbl::Array<device_str_property_t> str_properties(new device_str_property_t[str_props.size()],
                                                     str_props.size());
    for (size_t i = 0; i < str_props.size(); i++) {
      if (!fxl::IsStringUTF8(str_props[i].key)) {
        LOGF(ERROR, "String property key is not in UTF-8 encoding");
        return false;
      }

      if (str_props[i].value.valueless_by_exception()) {
        LOGF(ERROR, "String property value is not set");
        return false;
      }

      if (std::holds_alternative<uint32_t>(str_props[i].value)) {
        auto* prop_val = std::get_if<uint32_t>(&str_props[i].value);
        str_properties[i] = str_property_with_int(str_props[i].key.c_str(), *prop_val);
      } else if (std::holds_alternative<std::string>(str_props[i].value)) {
        auto* prop_val = std::get_if<std::string>(&str_props[i].value);
        if (prop_val && !fxl::IsStringUTF8(*prop_val)) {
          LOGF(ERROR, "String property value is not in UTF-8 encoding");
          return false;
        }
        str_properties[i] = str_property_with_string(str_props[i].key.c_str(), prop_val->c_str());
      } else if (std::holds_alternative<bool>(str_props[i].value)) {
        auto* prop_val = std::get_if<bool>(&str_props[i].value);
        str_properties[i] = str_property_with_bool(str_props[i].key.c_str(), *prop_val);
      }
    }

    return match_bind_rules(bytecode ? bytecode->get() : nullptr, drv->binding_size,
                            properties.get(), props.size(), str_properties.get(),
                            str_properties.size(), protocol_id, autobind);
  }

  LOGF(ERROR, "Invalid bytecode version: %i", drv->bytecode_version);
  return false;
}

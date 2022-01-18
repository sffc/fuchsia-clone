// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef TOOLS_FIDL_FIDLC_INCLUDE_FIDL_FLAT_ATTRIBUTE_SCHEMA_H_
#define TOOLS_FIDL_FIDLC_INCLUDE_FIDL_FLAT_ATTRIBUTE_SCHEMA_H_

#include <lib/fit/function.h>

#include "fidl/flat_ast.h"
#include "fidl/reporter.h"

namespace fidl::flat {

using reporter::Reporter;

class CompileStep;

// AttributeArgSchema defines a schema for a single argument in an attribute.
// This includes its type (string, uint64, etc.), whether it is optional or
// required, and (if applicable) a special-case rule for resolving its value.
class AttributeArgSchema {
 public:
  enum class Optionality {
    kOptional,
    kRequired,
  };

  explicit AttributeArgSchema(ConstantValue::Kind type,
                              Optionality optionality = Optionality::kRequired)
      : type_(type), optionality_(optionality) {
    assert(type != ConstantValue::Kind::kDocComment);
  }

  bool IsOptional() const { return optionality_ == Optionality::kOptional; }

  void ResolveArg(CompileStep* step, Attribute* attribute, AttributeArg* arg,
                  bool literal_only) const;

 private:
  const ConstantValue::Kind type_;
  const Optionality optionality_;
};

// AttributeSchema defines a schema for attributes. This includes the allowed
// placement (e.g. on a method, on a struct), names and schemas for arguments,
// and an optional constraint validator.
class AttributeSchema {
 public:
  // Note: Constraints get access to the fully compiled Element.
  // This is one reason why VerifyAttributesStep is a separate step.
  using Constraint =
      fit::function<bool(Reporter* reporter, const Attribute* attribute, const Element* element)>;

  // Constructs a new schema that allows any placement, takes no arguments, and
  // has no constraint. Use the methods below to customize it.
  AttributeSchema() = default;

  // Chainable mutators for customizing the schema.
  AttributeSchema& RestrictTo(std::set<Element::Kind> placements);
  AttributeSchema& RestrictToAnonymousLayouts();
  AttributeSchema& AddArg(AttributeArgSchema arg_schema);
  AttributeSchema& AddArg(std::string name, AttributeArgSchema arg_schema);
  AttributeSchema& Constrain(AttributeSchema::Constraint constraint);
  // Marks as use-early. See Kind::kUseEarly below.
  AttributeSchema& UseEarly();
  // Marks as compile-early. See Kind::kCompileEarly below.
  AttributeSchema& CompileEarly();
  // Marks as deprecated. See Kind::kDeprecated below.
  AttributeSchema& Deprecate();

  // Special schema for arbitrary user-defined attributes.
  static const AttributeSchema kUserDefined;

  // Returns true if this schema allows early compilations.
  bool CanCompileEarly() const { return kind_ == Kind::kCompileEarly; }

  // Resolves constants in the attribute's arguments. In the case of an
  // anonymous argument like @foo("abc"), infers the argument's name too.
  void ResolveArgs(CompileStep* step, Attribute* attribute) const;

  // Validates the attribute's placement and constraints. Must call
  // `ResolveArgs` first.
  void Validate(Reporter* reporter, const Attribute* attribute, const Element* element) const;

 private:
  enum class Kind {
    // Most attributes are validate-only. They do not participate in compilation
    // apart from validation at the end (possibly with a custom constraint).
    kValidateOnly,
    // Some attributes influence compilation and are used early, before
    // VerifyAttributesStep. These schemas do not allow a constraint, since
    // constraint validation happens too late to be relied on.
    kUseEarly,
    // Some attributes get compiled and used early, before the main CompileStep.
    // These schemas ensure all arguments are literals to avoid kicking off
    // other compilations. Like kUseEarly, they do not allow a constraint.
    kCompileEarly,
    // Deprecated attributes produce an error if used.
    kDeprecated,
    // All unrecognized attributes are considered user-defined. They receive
    // minimal validation since we don't know what to expect. They allow any
    // placement, only support string and bool arguments (lacking a way to
    // decide between int8, uint32, etc.), and have no constraint.
    kUserDefined,
  };

  enum class Placement {
    // Allowed anywhere.
    kAnywhere,
    // Only allowed in certain places specified by std::set<Element::Kind>.
    kSpecific,
    // Only allowed on anonymous layouts (not directly bound to a type
    // declaration like `type foo = struct { ... };`).
    kAnonymousLayout,
  };

  explicit AttributeSchema(Kind kind) : kind_(kind) {}

  static void ResolveArgsWithoutSchema(CompileStep* compile_step, Attribute* attribute);

  Kind kind_ = Kind::kValidateOnly;
  Placement placement_ = Placement::kAnywhere;
  std::set<Element::Kind> specific_placements_;
  // Use transparent comparator std::less<> to allow std::string_view lookups.
  std::map<std::string, AttributeArgSchema, std::less<>> arg_schemas_;
  Constraint constraint_ = nullptr;
};

}  // namespace fidl::flat

#endif  // TOOLS_FIDL_FIDLC_INCLUDE_FIDL_FLAT_ATTRIBUTE_SCHEMA_H_

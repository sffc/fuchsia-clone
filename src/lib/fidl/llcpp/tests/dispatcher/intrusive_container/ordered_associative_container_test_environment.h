// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_FIDL_LLCPP_TESTS_DISPATCHER_INTRUSIVE_CONTAINER_ORDERED_ASSOCIATIVE_CONTAINER_TEST_ENVIRONMENT_H_
#define SRC_LIB_FIDL_LLCPP_TESTS_DISPATCHER_INTRUSIVE_CONTAINER_ORDERED_ASSOCIATIVE_CONTAINER_TEST_ENVIRONMENT_H_

#include <iterator>

#include <zxtest/zxtest.h>

#include "associative_container_test_environment.h"

namespace fidl {
namespace internal_wavl {
namespace tests {
namespace intrusive_containers {

// OrderedAssociativeContainerTestEnvironment<>
//
// Test environment which defines and implements tests and test utilities which
// are applicable to all ordered associative containers (containers which keep
// their elements sorted by key) such as binary search trees.
template <typename TestEnvTraits>
class OrderedAssociativeContainerTestEnvironment
    : public AssociativeContainerTestEnvironment<TestEnvTraits> {
 public:
  using ACTE = AssociativeContainerTestEnvironment<TestEnvTraits>;
  using PopulateMethod = typename ACTE::PopulateMethod;
  using ContainerType = typename ACTE::ContainerType;
  using KeyTraits = typename ContainerType::KeyTraits;
  using KeyType = typename ContainerType::KeyType;
  using RawPtrType = typename ContainerType::RawPtrType;

  struct NonConstTraits {
    using ContainerType = typename ACTE::ContainerType;
    using IterType = typename ACTE::ContainerType::iterator;
  };

  struct ConstTraits {
    using ContainerType = const typename ACTE::ContainerType;
    using IterType = typename ACTE::ContainerType::const_iterator;
  };

  template <typename ContainerTraits>
  struct UpperBoundTraits {
    static typename ContainerTraits::IterType Search(
        typename ContainerTraits::ContainerType& container, const KeyType& key) {
      return container.upper_bound(key);
    }

    static bool BoundedBy(KeyType& key, const KeyType& bound_key) {
      return KeyTraits::LessThan(key, bound_key);
    }
  };

  template <typename ContainerTraits>
  struct LowerBoundTraits {
    static typename ContainerTraits::IterType Search(
        typename ContainerTraits::ContainerType& container, const KeyType& key) {
      return container.lower_bound(key);
    }

    static bool BoundedBy(const KeyType& key, const KeyType& bound_key) {
      return KeyTraits::LessThan(key, bound_key) || KeyTraits::EqualTo(key, bound_key);
    }
  };

  void DoOrderedIter(PopulateMethod populate_method) {
    ASSERT_NO_FAILURES(ACTE::Populate(container(), populate_method));

    auto iter = container().begin();
    EXPECT_TRUE(iter.IsValid());

    for (auto prev = iter++; iter.IsValid(); prev = iter++) {
      // None of the associative containers currently support storing
      // mutliple nodes with the same key, therefor the iteration ordering
      // of the keys should be strictly monotonically increasing.
      ASSERT_TRUE(prev.IsValid());

      auto iter_key = KeyTraits::GetKey(*iter);
      auto prev_key = KeyTraits::GetKey(*prev);

      EXPECT_TRUE(KeyTraits::LessThan(prev_key, iter_key));
      EXPECT_FALSE(KeyTraits::LessThan(iter_key, prev_key));
      EXPECT_FALSE(KeyTraits::EqualTo(prev_key, iter_key));
      EXPECT_FALSE(KeyTraits::EqualTo(iter_key, prev_key));
    }

    ASSERT_NO_FAILURES(TestEnvironment<TestEnvTraits>::Reset());
  }

  void OrderedIter() {
    ASSERT_NO_FATAL_FAILURE(DoOrderedIter(PopulateMethod::AscendingKey));
    ASSERT_NO_FATAL_FAILURE(DoOrderedIter(PopulateMethod::DescendingKey));
    ASSERT_NO_FATAL_FAILURE(DoOrderedIter(PopulateMethod::RandomKey));
  }

  void DoOrderedReverseIter(PopulateMethod populate_method) {
    ASSERT_NO_FAILURES(ACTE::Populate(container(), populate_method));

    auto iter = container().end();
    EXPECT_FALSE(iter.IsValid());
    --iter;
    EXPECT_TRUE(iter.IsValid());

    for (auto prev = iter--; iter.IsValid(); prev = iter--) {
      // None of the associative containers currently support storing
      // mutliple nodes with the same key, therefor the reverse iteration
      // ordering of the keys should be strictly monotonically decreasing.
      ASSERT_TRUE(prev.IsValid());

      auto iter_key = KeyTraits::GetKey(*iter);
      auto prev_key = KeyTraits::GetKey(*prev);

      EXPECT_TRUE(KeyTraits::LessThan(iter_key, prev_key));
      EXPECT_FALSE(KeyTraits::LessThan(prev_key, iter_key));
      EXPECT_FALSE(KeyTraits::EqualTo(prev_key, iter_key));
      EXPECT_FALSE(KeyTraits::EqualTo(iter_key, prev_key));
    }

    ASSERT_NO_FAILURES(TestEnvironment<TestEnvTraits>::Reset());
  }

  void OrderedReverseIter() {
    ASSERT_NO_FATAL_FAILURE(DoOrderedReverseIter(PopulateMethod::AscendingKey));
    ASSERT_NO_FATAL_FAILURE(DoOrderedReverseIter(PopulateMethod::DescendingKey));
    ASSERT_NO_FATAL_FAILURE(DoOrderedReverseIter(PopulateMethod::RandomKey));
  }

  template <typename BoundTraits>
  void DoBoundTest(PopulateMethod populate_method) {
    // Searching for a value while the tree is empty should always fail.
    auto found = BoundTraits::Search(container(), 0u);
    EXPECT_FALSE(found.IsValid());

    // Populate the container.
    ASSERT_NO_FAILURES(ACTE::Populate(container(), populate_method));

    // For every object we just put into the container compute the bound of
    // obj.key, (obj.key - 1) and (obj.key + 1) using brute force as well as
    // by using (upper|lower)_bound.  Make sure that the result agree.
    for (size_t i = 0; i < ACTE::OBJ_COUNT; ++i) {
      auto ptr = ACTE::objects()[i];
      ASSERT_NOT_NULL(ptr);

      struct {
        KeyType key;
        RawPtrType bound;
      } tests[] = {
          {.key = KeyTraits::GetKey(*ptr) - 1, .bound = nullptr},  // prev (key - 1)
          {.key = KeyTraits::GetKey(*ptr), .bound = nullptr},      // this (key)
          {.key = KeyTraits::GetKey(*ptr) + 1, .bound = nullptr},  // next (key + 1)
      };

      // Brute force search all of the objects we have populated the
      // collect with to find the objects with the smallest keys which
      // bound this/prev/next.
      for (size_t j = 0; j < ACTE::OBJ_COUNT; ++j) {
        auto tmp = ACTE::objects()[j];
        ASSERT_NOT_NULL(tmp);
        KeyType tmp_key = KeyTraits::GetKey(*tmp);

        for (size_t k = 0; k < std::size(tests); ++k) {
          auto& test = tests[k];

          if (BoundTraits::BoundedBy(test.key, tmp_key) &&
              (!test.bound || KeyTraits::LessThan(tmp_key, KeyTraits::GetKey(*test.bound))))
            test.bound = tmp;
        }
      }

      // Now perform the same searchs using upper_bound/lower_bound.
      for (size_t k = 0; k < std::size(tests); ++k) {
        auto& test = tests[k];
        auto iter = BoundTraits::Search(container(), test.key);

        // We should successfully find a bound using (upper|lower)_bound
        // if (and only if) we successfully found a bound using brute
        // force.  If we did find a bound, it should be the same bound
        // we found using brute force.
        if (test.bound != nullptr) {
          ASSERT_TRUE(iter.IsValid());
          EXPECT_EQ(test.bound, iter->raw_ptr());
        } else {
          EXPECT_FALSE(iter.IsValid());
        }
      }
    }

    ASSERT_NO_FAILURES(TestEnvironment<TestEnvTraits>::Reset());
  }

  void UpperBound() {
    using NonConstBoundTraits = UpperBoundTraits<NonConstTraits>;
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<NonConstBoundTraits>(PopulateMethod::AscendingKey));
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<NonConstBoundTraits>(PopulateMethod::DescendingKey));
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<NonConstBoundTraits>(PopulateMethod::RandomKey));

    using ConstBoundTraits = UpperBoundTraits<ConstTraits>;
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<ConstBoundTraits>(PopulateMethod::AscendingKey));
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<ConstBoundTraits>(PopulateMethod::DescendingKey));
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<ConstBoundTraits>(PopulateMethod::RandomKey));
  }

  void LowerBound() {
    using NonConstBoundTraits = LowerBoundTraits<NonConstTraits>;
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<NonConstBoundTraits>(PopulateMethod::AscendingKey));
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<NonConstBoundTraits>(PopulateMethod::DescendingKey));
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<NonConstBoundTraits>(PopulateMethod::RandomKey));

    using ConstBoundTraits = LowerBoundTraits<ConstTraits>;
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<ConstBoundTraits>(PopulateMethod::AscendingKey));
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<ConstBoundTraits>(PopulateMethod::DescendingKey));
    ASSERT_NO_FATAL_FAILURE(DoBoundTest<ConstBoundTraits>(PopulateMethod::RandomKey));
  }

 private:
  ContainerType& container() { return this->container_; }
  const ContainerType& const_container() { return this->container_; }
};

}  // namespace intrusive_containers
}  // namespace tests
}  // namespace internal_wavl
}  // namespace fidl

#endif  // SRC_LIB_FIDL_LLCPP_TESTS_DISPATCHER_INTRUSIVE_CONTAINER_ORDERED_ASSOCIATIVE_CONTAINER_TEST_ENVIRONMENT_H_

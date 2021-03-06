// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/ui/composition/cpp/fidl.h>
#include <fuchsia/ui/lifecycle/cpp/fidl.h>
#include <fuchsia/ui/pointer/cpp/fidl.h>
#include <fuchsia/ui/pointerinjector/cpp/fidl.h>
#include <lib/sys/cpp/testing/test_with_environment_fixture.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_identity.h>
#include <zircon/status.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "sdk/lib/ui/scenic/cpp/view_creation_tokens.h"

// These tests exercise the integration between Flatland and the InputSystem, including the
// View-to-View transform logic between the injection point and the receiver.
// Setup:
// - The test fixture sets up the display + the root session and view.
// - Injection done in context View Space, with fuchsia.ui.pointerinjector
// - Target(s) specified by View (using view ref koids)
// - Dispatch done to fuchsia.ui.pointer.MouseSource in receiver View Space.
namespace integration_tests {
namespace {
using fuc_ParentViewportWatcher = fuchsia::ui::composition::ParentViewportWatcher;
using fuc_ChildViewWatcher = fuchsia::ui::composition::ChildViewWatcher;
using fuc_FlatlandPtr = fuchsia::ui::composition::FlatlandPtr;
using fuc_ViewBoundProtocols = fuchsia::ui::composition::ViewBoundProtocols;
using fuc_FlatlandDisplayPtr = fuchsia::ui::composition::FlatlandDisplayPtr;
using fuc_ViewportProperties = fuchsia::ui::composition::ViewportProperties;
using fuc_TransformId = fuchsia::ui::composition::TransformId;
using fuc_ContentId = fuchsia::ui::composition::ContentId;
using fuv_ViewRefFocusedPtr = fuchsia::ui::views::ViewRefFocusedPtr;
using fuv_ViewRef = fuchsia::ui::views::ViewRef;
using fuv_FocusState = fuchsia::ui::views::FocusState;
using fup_MouseEvent = fuchsia::ui::pointer::MouseEvent;
using fup_MouseSourcePtr = fuchsia::ui::pointer::MouseSourcePtr;
using fupi_Config = fuchsia::ui::pointerinjector::Config;
using fupi_DispatchPolicy = fuchsia::ui::pointerinjector::DispatchPolicy;
using fupi_Event = fuchsia::ui::pointerinjector::Event;
using fupi_EventPhase = fuchsia::ui::pointerinjector::EventPhase;
using fupi_PointerSample = fuchsia::ui::pointerinjector::PointerSample;
using fupi_Context = fuchsia::ui::pointerinjector::Context;
using fupi_Data = fuchsia::ui::pointerinjector::Data;
using fupi_RegistryPtr = fuchsia::ui::pointerinjector::RegistryPtr;
using fupi_DevicePtr = fuchsia::ui::pointerinjector::DevicePtr;
using fupi_DeviceType = fuchsia::ui::pointerinjector::DeviceType;
using fupi_Target = fuchsia::ui::pointerinjector::Target;
using fupi_Viewport = fuchsia::ui::pointerinjector::Viewport;
using ful_LifecycleControllerSyncPtr = fuchsia::ui::lifecycle::LifecycleControllerSyncPtr;
using ful_LifecycleController = fuchsia::ui::lifecycle::LifecycleController;

const std::map<std::string, std::string> LocalServices() {
  return {{"fuchsia.ui.composition.Allocator",
           "fuchsia-pkg://fuchsia.com/flatland_integration_tests#meta/scenic.cmx"},
          {"fuchsia.ui.composition.Flatland",
           "fuchsia-pkg://fuchsia.com/flatland_integration_tests#meta/scenic.cmx"},
          {"fuchsia.ui.composition.FlatlandDisplay",
           "fuchsia-pkg://fuchsia.com/flatland_integration_tests#meta/scenic.cmx"},
          {"fuchsia.ui.pointerinjector.Registry",
           "fuchsia-pkg://fuchsia.com/flatland_integration_tests#meta/scenic.cmx"},
          // TODO(fxbug.dev/82655): Remove this after migrating to RealmBuilder.
          {"fuchsia.ui.lifecycle.LifecycleController",
           "fuchsia-pkg://fuchsia.com/flatland_integration_tests#meta/scenic.cmx"},
          {"fuchsia.hardware.display.Provider",
           "fuchsia-pkg://fuchsia.com/fake-hardware-display-controller-provider#meta/hdcp.cmx"}};
}

// Allow these global services.
const std::vector<std::string> GlobalServices() {
  return {"fuchsia.vulkan.loader.Loader", "fuchsia.sysmem.Allocator"};
}

class FlatlandMouseIntegrationTest : public gtest::TestWithEnvironmentFixture {
 protected:
  static constexpr uint32_t kDeviceId = 1111;

  static constexpr uint32_t kPointerId = 2222;

  static constexpr uint32_t kDefaultSize = 1;

  // clang-format off
  static constexpr std::array<float, 9> kIdentityMatrix = {
    1, 0, 0, // column one
    0, 1, 0, // column two
    0, 0, 1, // column three
  };
  // clang-format on

  void SetUp() override {
    TestWithEnvironmentFixture::SetUp();
    environment_ = CreateNewEnclosingEnvironment("flatland_mouse_integration_test_environment",
                                                 CreateServices());
    WaitForEnclosingEnvToStart(environment_.get());

    // Connects to scenic lifecycle controller in order to shutdown scenic at the end of the test.
    // This ensures the correct ordering of shutdown under CFv1: first scenic, then the fake display
    // controller.
    //
    // TODO(fxbug.dev/82655): Remove this after migrating to RealmBuilder.
    environment_->ConnectToService<ful_LifecycleController>(
        scenic_lifecycle_controller_.NewRequest());

    environment_->ConnectToService(flatland_display_.NewRequest());
    flatland_display_.set_error_handler([](zx_status_t status) {
      FAIL() << "Lost connection to Scenic: " << zx_status_get_string(status);
    });

    environment_->ConnectToService(pointerinjector_registry_.NewRequest());
    pointerinjector_registry_.set_error_handler([](zx_status_t status) {
      FAIL() << "Lost connection to pointerinjector Registry: " << zx_status_get_string(status);
    });

    // Set up root view.
    environment_->ConnectToService(root_session_.NewRequest());
    root_session_.set_error_handler([](zx_status_t status) {
      FAIL() << "Lost connection to Scenic: " << zx_status_get_string(status);
    });

    fidl::InterfacePtr<fuc_ChildViewWatcher> child_view_watcher;
    fuc_ViewBoundProtocols protocols;
    fuv_ViewRefFocusedPtr root_focused_ptr;
    auto [child_token, parent_token] = scenic::ViewCreationTokenPair::New();
    fidl::InterfacePtr<fuc_ParentViewportWatcher> parent_viewport_watcher;
    auto identity = scenic::NewViewIdentityOnCreation();
    root_view_ref_ = fidl::Clone(identity.view_ref);
    protocols.set_view_ref_focused(root_focused_ptr.NewRequest());
    root_session_->CreateView2(std::move(child_token), std::move(identity), std::move(protocols),
                               parent_viewport_watcher.NewRequest());

    parent_viewport_watcher->GetLayout([this](auto layout_info) {
      ASSERT_TRUE(layout_info.has_logical_size());
      const auto [width, height] = layout_info.logical_size();
      display_width_ = static_cast<float>(width);
      display_height_ = static_cast<float>(height);
    });

    flatland_display_->SetContent(std::move(parent_token), child_view_watcher.NewRequest());
    BlockingPresent(root_session_);

    // Wait until we get the display size.
    RunLoopUntil([this] { return display_width_ != 0 && display_height_ != 0; });
  }

  void TearDown() override {
    // Avoid spurious errors since we are about to kill scenic.
    //
    // TODO(fxbug.dev/82655): Remove this after migrating to RealmBuilder.
    flatland_display_.set_error_handler(nullptr);
    root_session_.set_error_handler(nullptr);
    pointerinjector_registry_.set_error_handler(nullptr);

    zx_status_t terminate_status = scenic_lifecycle_controller_->Terminate();
    FX_CHECK(terminate_status == ZX_OK)
        << "Failed to terminate Scenic with status: " << zx_status_get_string(terminate_status);
  }

  // Configures services available to the test environment. This method is called by |SetUp()|. It
  // shadows but calls |TestWithEnvironmentFixture::CreateServices()|.
  std::unique_ptr<sys::testing::EnvironmentServices> CreateServices() {
    auto services = TestWithEnvironmentFixture::CreateServices();
    for (const auto& [name, url] : LocalServices()) {
      const zx_status_t is_ok = services->AddServiceWithLaunchInfo({.url = url}, name);
      FX_CHECK(is_ok == ZX_OK) << "Failed to add service " << name;
    }

    for (const auto& service : GlobalServices()) {
      const zx_status_t is_ok = services->AllowParentService(service);
      FX_CHECK(is_ok == ZX_OK) << "Failed to add service " << service;
    }

    return services;
  }

  void BlockingPresent(fuc_FlatlandPtr& flatland) {
    bool presented = false;
    flatland.events().OnFramePresented = [&presented](auto) { presented = true; };
    flatland->Present({});
    RunLoopUntil([&presented] { return presented; });
    flatland.events().OnFramePresented = nullptr;
  }

  void Inject(float x, float y, fupi_EventPhase phase, std::vector<uint8_t> pressed_buttons = {},
              std::optional<int64_t> scroll_v = std::nullopt,
              std::optional<int64_t> scroll_h = std::nullopt) {
    FX_DCHECK(injector_);
    fupi_Event event;
    event.set_timestamp(0);
    {
      fupi_PointerSample pointer_sample;
      pointer_sample.set_pointer_id(kPointerId);
      pointer_sample.set_phase(phase);
      pointer_sample.set_position_in_viewport({x, y});
      if (scroll_v.has_value()) {
        pointer_sample.set_scroll_v(scroll_v.value());
      }
      if (scroll_h.has_value()) {
        pointer_sample.set_scroll_h(scroll_h.value());
      }
      if (!pressed_buttons.empty()) {
        pointer_sample.set_pressed_buttons(pressed_buttons);
      }
      fupi_Data data;
      data.set_pointer_sample(std::move(pointer_sample));
      event.set_data(std::move(data));
    }
    std::vector<fupi_Event> events;
    events.emplace_back(std::move(event));
    injector_->Inject(std::move(events), [] {});
  }

  void RegisterInjector(fuv_ViewRef context_view_ref, fuv_ViewRef target_view_ref,
                        fupi_DispatchPolicy dispatch_policy, std::vector<uint8_t> buttons,
                        std::array<float, 9> viewport_to_context_transform) {
    fupi_Config config;
    config.set_device_id(kDeviceId);
    config.set_device_type(fupi_DeviceType::MOUSE);
    config.set_dispatch_policy(dispatch_policy);
    if (!buttons.empty()) {
      config.set_buttons(buttons);
    }
    {
      {
        fupi_Context context;
        context.set_view(std::move(context_view_ref));
        config.set_context(std::move(context));
      }
      {
        fupi_Target target;
        target.set_view(std::move(target_view_ref));
        config.set_target(std::move(target));
      }
      {
        fupi_Viewport viewport;
        viewport.set_extents(FullScreenExtents());
        viewport.set_viewport_to_context_transform(viewport_to_context_transform);
        config.set_viewport(std::move(viewport));
      }
    }

    injector_.set_error_handler([this](zx_status_t) { injector_channel_closed_ = true; });
    bool register_callback_fired = false;
    pointerinjector_registry_->Register(
        std::move(config), injector_.NewRequest(),
        [&register_callback_fired] { register_callback_fired = true; });

    RunLoopUntil([&register_callback_fired] { return register_callback_fired; });
    EXPECT_FALSE(injector_channel_closed_);
  }

  // Starts a recursive MouseSource::Watch() loop that collects all received events into
  // |out_events|.
  void StartWatchLoop(fup_MouseSourcePtr& mouse_source, std::vector<fup_MouseEvent>& out_events) {
    const size_t index = watch_loops_.size();
    watch_loops_.emplace_back();
    watch_loops_.at(index) = [this, &mouse_source, &out_events,
                              index](std::vector<fup_MouseEvent> events) {
      std::move(events.begin(), events.end(), std::back_inserter(out_events));
      mouse_source->Watch([this, index](std::vector<fup_MouseEvent> events) {
        watch_loops_.at(index)(std::move(events));
      });
    };
    mouse_source->Watch(watch_loops_.at(index));
  }

  std::array<std::array<float, 2>, 2> FullScreenExtents() const {
    return {{{0, 0}, {display_width_, display_height_}}};
  }

  fuc_FlatlandPtr root_session_;

  fuv_ViewRef root_view_ref_;

  std::unique_ptr<sys::testing::EnclosingEnvironment> environment_;

  bool injector_channel_closed_ = false;

  float display_width_ = 0;

  float display_height_ = 0;

 private:
  ful_LifecycleControllerSyncPtr scenic_lifecycle_controller_;

  fuc_FlatlandDisplayPtr flatland_display_;

  fupi_RegistryPtr pointerinjector_registry_;

  fupi_DevicePtr injector_;

  // Holds watch loops so they stay alive through the duration of the test.
  std::vector<std::function<void(std::vector<fup_MouseEvent>)>> watch_loops_;
};

// The child view should receive focus and input events when the mouse button is pressed over its
// view.
TEST_F(FlatlandMouseIntegrationTest, ChildReceivesFocus_OnMouseLatch) {
  fuc_FlatlandPtr child_session;
  fup_MouseSourcePtr child_mouse_source;
  fuv_ViewRefFocusedPtr child_focused_ptr;

  environment_->ConnectToService(child_session.NewRequest());
  child_session.set_error_handler([](zx_status_t status) {
    FAIL() << "Lost connection to Scenic: " << zx_status_get_string(status);
  });
  child_mouse_source.set_error_handler([](zx_status_t status) {
    FX_LOGS(ERROR) << "Mouse source closed with status: " << zx_status_get_string(status);
  });
  child_focused_ptr.set_error_handler([](zx_status_t status) {
    FX_LOGS(ERROR) << "ViewRefFocused closed with status: " << zx_status_get_string(status);
  });

  // Set up the child view watcher.
  fidl::InterfacePtr<fuc_ChildViewWatcher> child_view_watcher;
  auto [child_token, parent_token] = scenic::ViewCreationTokenPair::New();
  fuc_ViewportProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});

  const fuc_TransformId kRootTransform{.value = 1};
  root_session_->CreateTransform(kRootTransform);
  root_session_->SetRootTransform(kRootTransform);

  const fuc_ContentId kRootContent{.value = 1};
  root_session_->CreateViewport(kRootContent, std::move(parent_token), std::move(properties),
                                child_view_watcher.NewRequest());
  root_session_->SetContent(kRootTransform, kRootContent);

  BlockingPresent(root_session_);

  // Set up the child view along with its MouseSource and ViewRefFocused channel.
  fidl::InterfacePtr<fuc_ParentViewportWatcher> parent_viewport_watcher;
  auto identity = scenic::NewViewIdentityOnCreation();
  auto child_view_ref = fidl::Clone(identity.view_ref);
  fuc_ViewBoundProtocols protocols;
  protocols.set_mouse_source(child_mouse_source.NewRequest());
  protocols.set_view_ref_focused(child_focused_ptr.NewRequest());
  child_session->CreateView2(std::move(child_token), std::move(identity), std::move(protocols),
                             parent_viewport_watcher.NewRequest());
  BlockingPresent(child_session);

  // Listen for input events.
  std::vector<fup_MouseEvent> child_events;
  StartWatchLoop(child_mouse_source, child_events);

  // Inject an input event at (0,0) which is the point of overlap between the parent and the
  // child.
  const std::vector<uint8_t> button_vec = {1};
  RegisterInjector(fidl::Clone(root_view_ref_), fidl::Clone(child_view_ref),
                   fupi_DispatchPolicy::MOUSE_HOVER_AND_LATCH_IN_TARGET, button_vec,
                   kIdentityMatrix);
  Inject(0, 0, fupi_EventPhase::ADD, button_vec);

  // Child should receive mouse input events.
  RunLoopUntil([&child_events] { return child_events.size() == 1u; });

  // Child view should receive focus.
  std::optional<fuv_FocusState> child_focused;
  child_focused_ptr->Watch([&child_focused](auto update) { child_focused = std::move(update); });
  RunLoopUntil([&child_focused] { return child_focused.has_value(); });
  EXPECT_TRUE(child_focused->focused());
}
}  // namespace
}  // namespace integration_tests

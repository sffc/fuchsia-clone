// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace-provider/provider.h>
#include <lib/zx/thread.h>

#include <memory>

#include "lib/inspect/cpp/inspect.h"
#include "lib/sys/inspect/cpp/component.h"
#include "src/lib/fxl/command_line.h"
#include "src/lib/fxl/log_settings_command_line.h"
#include "src/ui/lib/display/get_hardware_display_controller.h"
#include "src/ui/lib/display/hardware_display_controller_provider_impl.h"
#include "src/ui/scenic/bin/app.h"
#include "src/ui/scenic/lib/scenic/util/scheduler_profile.h"

int main(int argc, const char** argv) {
  auto command_line = fxl::CommandLineFromArgcArgv(argc, argv);
  if (!fxl::SetLogSettingsFromCommandLine(command_line, {"scenic"}))
    return 1;

  // TODO(fxbug.dev/92839): Remove this when it becomes easy to add the configs
  // in test realms via the configuration files.
  const bool force_flatland_use = command_line.HasOption("force-flatland-for-test");

  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  trace::TraceProviderWithFdio trace_provider(loop.dispatcher());
  // This call creates ComponentContext, but does not start serving immediately. Outgoing directory
  // is served by App, after App::InitializeServices() is completed.
  std::unique_ptr<sys::ComponentContext> app_context = sys::ComponentContext::Create();

  // Set up an inspect::Node to inject into the App.
  sys::ComponentInspector inspector(app_context.get());

  // Obtain the default display controller via the fuchsia.hardware.display.Provider service that we
  // find in our environment. Scenic provides its own default implementation through
  // |hdcp_service_impl|, which can be overridden by the environment (e.g. by a test's
  // "injected-services" facet).
  ui_display::HardwareDisplayControllerProviderImpl hdcp_service_impl(app_context.get());
  auto display_controller_promise = ui_display::GetHardwareDisplayController(&hdcp_service_impl);

  // Instantiate Scenic app.
  scenic_impl::App app(
      std::move(app_context), inspector.root().CreateChild("scenic"),
      std::move(display_controller_promise), [&loop] { loop.Quit(); }, force_flatland_use);

  // TODO(fxbug.dev/40858): Migrate to the role-based scheduler API when available,
  // instead of hard coding parameters.
  {
    // TODO(fxbug.dev/44209): Centralize default frame period.
    const zx::duration capacity = zx::msec(16);
    const zx::duration deadline = zx::msec(16);
    const zx::duration period = deadline;
    const auto profile = util::GetSchedulerProfile(capacity, deadline, period);
    if (profile) {
      const auto status = zx::thread::self()->set_profile(profile, 0);
      if (status != ZX_OK) {
        FX_LOGS(ERROR) << "Failed to apply profile to main thread: " << status;
      }
    }
  }

  loop.Run();
  FX_LOGS(INFO) << "Quit main Scenic loop.";

  return 0;
}

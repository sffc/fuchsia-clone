// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests the ability to route the built-in "boot" resolver from component manager's realm to
//! a new environment not-inherited from the built-in environment.
//!
//! This tested by starting a component manager instance, set to expose any protocol exposed to it
//! in its outgoing directory. A fake bootfs is offered to the component manager, which provides the
//! manifests and executables needed to run the test.
//!
//! The test then calls the expected test FIDL protocol in component manager's outgoing namespace.
//! If the call is successful, then the boot resolver was correctly routed.

use {
    fidl_fidl_test_components as ftest, fidl_fuchsia_io2 as fio2,
    fuchsia_component::server::ServiceFs,
    fuchsia_component_test::new::{Capability, ChildOptions, RealmBuilder, Ref, Route},
    futures::prelude::*,
};

#[fuchsia::test]
async fn boot_resolver_can_be_routed_from_component_manager() {
    let builder = RealmBuilder::new().await.unwrap();
    let component_manager = builder
        .add_child(
            "component-manager",
            "fuchsia-pkg://fuchsia.com/boot-resolver-routing-tests#meta/component_manager.cm",
            ChildOptions::new(),
        )
        .await
        .unwrap();
    let mock_boot = builder
        .add_local_child(
            "mock-boot",
            |mock_handles| {
                async move {
                    let pkg = io_util::directory::open_in_namespace(
                        "/pkg",
                        io_util::OPEN_RIGHT_READABLE | io_util::OPEN_RIGHT_EXECUTABLE,
                    )
                    .unwrap();
                    let mut fs = ServiceFs::new();
                    fs.add_remote("boot", pkg);
                    fs.serve_connection(mock_handles.outgoing_dir.into_channel()).unwrap();
                    fs.collect::<()>().await;
                    Ok::<(), anyhow::Error>(())
                }
                .boxed()
            },
            ChildOptions::new(),
        )
        .await
        .unwrap();

    // Supply a fake boot directory which is really just an alias to this package's pkg directory.
    // TODO(fxbug.dev/37534): Add the execute bit when supported.
    builder
        .add_route(
            Route::new()
                .capability(Capability::directory("boot").path("/boot").rights(fio2::R_STAR_DIR))
                .from(&mock_boot)
                .to(&component_manager),
        )
        .await
        .unwrap();

    // This is the test protocol that is expected to be callable.
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fidl.test.components.Trigger"))
                .from(&component_manager)
                .to(Ref::parent()),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                // Forward logging to debug test breakages.
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                // Component manager needs fuchsia.process.Launcher to spawn new processes.
                .capability(Capability::protocol_by_name("fuchsia.process.Launcher"))
                .from(Ref::parent())
                .to(&component_manager),
        )
        .await
        .unwrap();

    let realm_instance = builder.build().await.unwrap();
    let trigger =
        realm_instance.root.connect_to_protocol_at_exposed_dir::<ftest::TriggerMarker>().unwrap();
    let out = trigger.run().await.expect("trigger failed");
    assert_eq!(out, "Triggered");
}

// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    fidl::{
        endpoints::{Proxy, ServerEnd},
        HandleBased,
    },
    fidl_fuchsia_device::{ControllerMarker, ControllerProxy},
    fidl_fuchsia_hardware_block_encrypted::{DeviceManagerMarker, DeviceManagerProxy},
    fidl_fuchsia_hardware_block_partition::Guid,
    fidl_fuchsia_hardware_block_volume::VolumeManagerMarker,
    fidl_fuchsia_identity_account::{AccountManagerMarker, AccountManagerProxy},
    fuchsia_async as fasync,
    fuchsia_component_test::{
        builder::{Capability, CapabilityRoute, ComponentSource, RealmBuilder, RouteEndpoint},
        RealmInstance,
    },
    fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance},
    fuchsia_zircon::{sys::zx_status_t, Status},
    ramdevice_client::{RamdiskClient, RamdiskClientBuilder},
    rand::{rngs::SmallRng, FromEntropy, Rng},
    std::{
        fs,
        os::{raw::c_int, unix::io::AsRawFd},
        time::Duration,
    },
    storage_isolated_driver_manager::bind_fvm,
};

// Canonically defined in //zircon/system/public/zircon/hw/gpt.h
const FUCHSIA_DATA_GUID_VALUE: [u8; 16] = [
    // 08185F0C-892D-428A-A789-DBEEC8F55E6A
    0x0c, 0x5f, 0x18, 0x08, 0x2d, 0x89, 0x8a, 0x42, 0xa7, 0x89, 0xdb, 0xee, 0xc8, 0xf5, 0x5e, 0x6a,
];
const FUCHSIA_DATA_GUID: Guid = Guid { value: FUCHSIA_DATA_GUID_VALUE };
const ACCOUNT_LABEL: &str = "account";
const RAMCTL_PATH: &'static str = "sys/platform/00:00:2d/ramctl";
const BLOCK_SIZE: u64 = 4096;
const BLOCK_COUNT: u64 = 1024; // 4MB RAM ought to be good enough
const FVM_SLICE_SIZE: usize = 8192;
// For whatever reason, using `Duration::MAX` seems to trigger immediate ZX_ERR_TIMED_OUT in the
// wait_for_device_at calls, so we just set a quite large timeout here.
const DEVICE_WAIT_TIMEOUT: Duration = Duration::from_secs(20);

#[link(name = "fs-management")]
extern "C" {
    pub fn fvm_init(fd: c_int, slice_size: usize) -> zx_status_t;
}

struct TestEnv {
    realm_instance: RealmInstance,
}

impl TestEnv {
    async fn build() -> TestEnv {
        let mut builder = RealmBuilder::new().await.unwrap();
        builder.driver_test_realm_setup().await.unwrap();
        builder
            .add_component("password_authenticator", ComponentSource::url("fuchsia-pkg://fuchsia.com/password-authenticator-integration-tests#meta/password-authenticator.cm")).await.unwrap()
            .add_route(CapabilityRoute {
                capability: Capability::protocol("fuchsia.logger.LogSink"),
                source: RouteEndpoint::AboveRoot,
                targets: vec![
                    RouteEndpoint::component("password_authenticator"),
                ],
            }).unwrap()

            // Expose AccountManager so we can test it
            .add_route(CapabilityRoute {
                capability: Capability::protocol("fuchsia.identity.account.AccountManager"),
                source: RouteEndpoint::component("password_authenticator"),
                targets: vec![
                    RouteEndpoint::AboveRoot,
                ],
            }).unwrap()

            // Expose /dev from DriverTestrealm to password_authenticator, which makes use of it.
            .add_route(CapabilityRoute {
                capability: Capability::directory("dev", "/dev", fidl_fuchsia_io2::RW_STAR_DIR),
                source: RouteEndpoint::component("driver_test_realm"),
                targets: vec![
                    RouteEndpoint::component("password_authenticator"),
                ],
            }).unwrap();

        let realm_instance = builder.build().create().await.unwrap();
        let args = fidl_fuchsia_driver_test::RealmArgs {
            root_driver: Some("fuchsia-boot:///#driver/platform-bus.so".to_string()),
            ..fidl_fuchsia_driver_test::RealmArgs::EMPTY
        };
        realm_instance.driver_test_realm_start(args).await.unwrap();

        TestEnv { realm_instance }
    }

    pub async fn setup_ramdisk(&self, mut type_guid: Guid, name: &str) -> RamdiskClient {
        let dev_root_fd = self.dev_root_fd();

        // Wait for ramctl in namespace at /dev/sys/platform/00:00:2d/ramctl
        ramdevice_client::wait_for_device_at(&dev_root_fd, RAMCTL_PATH, DEVICE_WAIT_TIMEOUT)
            .expect("Could not wait for ramctl from isolated-devmgr");

        // Create ramdisk
        let ramdisk = RamdiskClientBuilder::new(BLOCK_SIZE, BLOCK_COUNT)
            .dev_root(self.dev_root_fd())
            .build()
            .expect("Could not create ramdisk");

        // Open ramdisk device and initialize FVM
        {
            let ramdisk_handle = ramdisk.open().expect("Could not re-open ramdisk").into_handle();
            let ramdisk_file =
                fs::File::from(fdio::create_fd(ramdisk_handle).expect("create fd of dev root"));
            let status = unsafe { fvm_init(ramdisk_file.as_raw_fd(), FVM_SLICE_SIZE) };
            Status::ok(status).expect("could not initialize FVM structures in ramdisk");
            // ramdisk_file drops, closing the fd we created
        }

        // Open ramdisk device again as fidl_fuchsia_device::ControllerProxy
        let ramdisk_chan = ramdisk.open().expect("Could not re-open ramdisk");
        let controller_chan = fasync::Channel::from_channel(ramdisk_chan)
            .expect("Could not convert ramdisk channel to async channel");
        let controller = ControllerProxy::from_channel(controller_chan);

        // Bind FVM to that ramdisk
        bind_fvm(&controller).await.expect("Could not bind FVM");

        // wait for /fvm child device to appear and open it
        let fvm_path = ramdisk.get_path().to_string() + "/fvm";
        ramdevice_client::wait_for_device_at(&dev_root_fd, &fvm_path, DEVICE_WAIT_TIMEOUT)
            .expect("Could not wait for fvm from isolated-devmgr");

        let (volume_manager_client, volume_manager_server) =
            fidl::endpoints::create_proxy::<VolumeManagerMarker>()
                .expect("Could not create volume manager channel pair");
        self.dev_root()
            .open(
                fidl_fuchsia_io::OPEN_RIGHT_READABLE | fidl_fuchsia_io::OPEN_RIGHT_WRITABLE,
                fidl_fuchsia_io::MODE_TYPE_SERVICE,
                &fvm_path,
                ServerEnd::new(volume_manager_server.into_channel()),
            )
            .expect("Could not connect to VolumeManager");

        // create FVM child volume with desired GUID/label
        let mut rng = SmallRng::from_entropy();
        let mut instance_guid = Guid { value: rng.gen() };
        let status = volume_manager_client
            .allocate_partition(1, &mut type_guid, &mut instance_guid, name, 0)
            .await
            .expect("Could not request to create volume");
        Status::ok(status).expect("Could not create volume");

        let fvm_inner_block_path = fvm_path + "/" + name + "-p-1/block";
        ramdevice_client::wait_for_device_at(
            &dev_root_fd,
            &fvm_inner_block_path,
            DEVICE_WAIT_TIMEOUT,
        )
        .expect("Could not wait for inner fvm block device");

        // Return handle to ramdisk since RamdiskClient's Drop impl destroys the ramdisk.
        ramdisk
    }

    pub fn open_zxcrypt_manager(&self, ramdisk: &RamdiskClient, name: &str) -> DeviceManagerProxy {
        let (manager_client, manager_server) =
            fidl::endpoints::create_proxy::<DeviceManagerMarker>()
                .expect("Could not create encryption volume manager channel pair");
        let mgr_path = ramdisk.get_path().to_string() + "/fvm/" + name + "-p-1/block/zxcrypt";
        self.dev_root()
            .open(
                fidl_fuchsia_io::OPEN_RIGHT_READABLE | fidl_fuchsia_io::OPEN_RIGHT_WRITABLE,
                fidl_fuchsia_io::MODE_TYPE_SERVICE,
                &mgr_path,
                ServerEnd::new(manager_server.into_channel()),
            )
            .expect("Could not connect to zxcrypt manager");

        manager_client
    }

    pub async fn format_zxcrypt(&self, ramdisk: &RamdiskClient, name: &str) {
        let (controller_client, controller_server) =
            fidl::endpoints::create_proxy::<ControllerMarker>().expect("create channel pair");
        let block_path = ramdisk.get_path().to_string() + "/fvm/" + name + "-p-1/block";
        self.dev_root()
            .open(
                fidl_fuchsia_io::OPEN_RIGHT_READABLE | fidl_fuchsia_io::OPEN_RIGHT_WRITABLE,
                fidl_fuchsia_io::MODE_TYPE_SERVICE,
                &block_path,
                ServerEnd::new(controller_server.into_channel()),
            )
            .expect("Could not connect to fvm block device");

        // Bind the zxcrypt driver to the block device
        controller_client
            .bind("zxcrypt.so")
            .await
            .expect("Could not send request to bind zxcrypt driver")
            .expect("Could not bind zxcrypt driver");

        // Wait for zxcrypt device manager node to appear
        let zxcrypt_path = block_path + "/zxcrypt";
        let dev_root_fd = self.dev_root_fd();
        ramdevice_client::wait_for_device_at(&dev_root_fd, &zxcrypt_path, DEVICE_WAIT_TIMEOUT)
            .expect("wait for zxcrypt from isolated-devmgr");

        // Open zxcrypt device manager node
        let manager = self.open_zxcrypt_manager(ramdisk, name);
        let key: [u8; 32] = [0; 32];
        manager.format(&key, 0).await.expect("Could not format zxcrypt");
    }

    pub fn dev_root(&self) -> fidl_fuchsia_io::DirectoryProxy {
        let (dev_dir_client, dev_dir_server) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_io::DirectoryMarker>()
                .expect("create channel pair");

        self.realm_instance
            .root
            .get_exposed_dir()
            .open(
                fidl_fuchsia_io::OPEN_RIGHT_READABLE | fidl_fuchsia_io::OPEN_RIGHT_WRITABLE,
                fidl_fuchsia_io::MODE_TYPE_DIRECTORY,
                "dev",
                ServerEnd::new(dev_dir_server.into_channel()),
            )
            .expect("Get /dev from isolated_devmgr");
        dev_dir_client
    }

    pub fn dev_root_fd(&self) -> fs::File {
        let dev_root_proxy = self.dev_root();
        fs::File::from(
            fdio::create_fd(
                dev_root_proxy
                    .into_channel()
                    .expect("Could not convert dev root DirectoryProxy into channel")
                    .into_zx_channel()
                    .into_handle(),
            )
            .expect("create fd of dev root"),
        )
    }

    pub fn account_manager(&self) -> AccountManagerProxy {
        self.realm_instance
            .root
            .connect_to_protocol_at_exposed_dir::<AccountManagerMarker>()
            .expect("connect to account manager")
    }

    pub async fn account_ids(&self) -> Vec<u64> {
        self.account_manager().get_account_ids().await.expect("get account ids")
    }
}

#[fasync::run_singlethreaded(test)]
async fn get_account_ids_no_partition() {
    let env = TestEnv::build().await;
    let account_ids = env.account_ids().await;
    assert_eq!(account_ids, vec![]);
}

#[fasync::run_singlethreaded(test)]
async fn get_account_ids_partition_wrong_guid() {
    let env = TestEnv::build().await;
    let unrelated_guid: Guid = Guid {
        value: [0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8, 0x9, 0xa, 0xb, 0xc, 0xd, 0xe, 0xf],
    };
    let _ramdisk = env.setup_ramdisk(unrelated_guid, ACCOUNT_LABEL).await;
    let account_manager = env
        .realm_instance
        .root
        .connect_to_protocol_at_exposed_dir::<AccountManagerMarker>()
        .expect("connect to account manager");

    let account_ids = account_manager.get_account_ids().await.expect("get account ids");
    assert_eq!(account_ids, vec![]);
}

#[fasync::run_singlethreaded(test)]
async fn get_account_ids_partition_wrong_label() {
    let env = TestEnv::build().await;
    let _ramdisk = env.setup_ramdisk(FUCHSIA_DATA_GUID, "wrong-label").await;
    let account_manager = env
        .realm_instance
        .root
        .connect_to_protocol_at_exposed_dir::<AccountManagerMarker>()
        .expect("connect to account manager");

    let account_ids = account_manager.get_account_ids().await.expect("get account ids");
    assert_eq!(account_ids, vec![]);
}

#[fasync::run_singlethreaded(test)]
async fn get_account_ids_partition_no_zxcrypt() {
    let env = TestEnv::build().await;
    let _ramdisk = env.setup_ramdisk(FUCHSIA_DATA_GUID, ACCOUNT_LABEL).await;
    let account_manager = env
        .realm_instance
        .root
        .connect_to_protocol_at_exposed_dir::<AccountManagerMarker>()
        .expect("connect to account manager");

    let account_ids = account_manager.get_account_ids().await.expect("get account ids");
    assert_eq!(account_ids, vec![]);
}

#[fasync::run_singlethreaded(test)]
async fn get_account_ids_with_zxcrypt_header() {
    let env = TestEnv::build().await;
    let ramdisk = env.setup_ramdisk(FUCHSIA_DATA_GUID, ACCOUNT_LABEL).await;
    env.format_zxcrypt(&ramdisk, ACCOUNT_LABEL).await;

    let account_manager = env
        .realm_instance
        .root
        .connect_to_protocol_at_exposed_dir::<AccountManagerMarker>()
        .expect("connect to account manager");

    let account_ids = account_manager.get_account_ids().await.expect("get account ids");
    assert_eq!(account_ids, vec![1]);
}

// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fs_test.h"

#include <fuchsia/io/llcpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/memfs/memfs.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>
#include <lib/zx/handle.h>

#include <fbl/unique_fd.h>
#include <fs-management/admin.h>
#include <fs-management/format.h>
#include <fs-management/launch.h>
#include <fs-management/mount.h>

#include "src/lib/isolated_devmgr/v2_component/fvm.h"

namespace fs_test {

// Creates a ramdisk with an optional FVM partition. Returns the ram-disk and the device path.
static zx::status<std::pair<isolated_devmgr::RamDisk, std::string>> CreateRamDisk(
    const TestFileSystemOptions& options) {
  // Create a ram-disk.
  auto ram_disk_or =
      isolated_devmgr::RamDisk::Create(options.device_block_size, options.device_block_count);
  if (ram_disk_or.is_error()) {
    return ram_disk_or.take_error();
  }

  // Create an FVM partition if requested.
  std::string device_path;
  if (options.use_fvm) {
    auto fvm_partition_or =
        isolated_devmgr::CreateFvmPartition(ram_disk_or.value().path(), options.fvm_slice_size);
    if (fvm_partition_or.is_error()) {
      return fvm_partition_or.take_error();
    }
    device_path = fvm_partition_or.value();
  } else {
    device_path = ram_disk_or.value().path();
  }

  return zx::ok(std::make_pair(std::move(ram_disk_or).value(), device_path));
}

TestFileSystemOptions TestFileSystemOptions::DefaultMinfs() {
  return TestFileSystemOptions{.description = "MinfsWithFvm",
                               .use_fvm = true,
                               .device_block_size = 512,
                               .device_block_count = 131'072,
                               .fvm_slice_size = 1'048'576,
                               .file_system = &MinfsFileSystem::SharedInstance()};
}

TestFileSystemOptions TestFileSystemOptions::DefaultMemfs() {
  return TestFileSystemOptions{.description = "Memfs",
                               .file_system = &MemfsFileSystem::SharedInstance()};
}

std::ostream& operator<<(std::ostream& out, const TestFileSystemOptions& options) {
  return out << options.description;
}

std::vector<TestFileSystemOptions> AllTestFileSystems() {
  TestFileSystemOptions minfs_with_no_fvm = TestFileSystemOptions::DefaultMinfs();
  minfs_with_no_fvm.description = "MinfsWithoutFvm";
  minfs_with_no_fvm.use_fvm = false;
  return std::vector<TestFileSystemOptions>{TestFileSystemOptions::DefaultMinfs(),
                                            minfs_with_no_fvm,
                                            TestFileSystemOptions::DefaultMemfs()};
}

zx::status<> FileSystemInstance::Mount(const std::string& device_path,
                                       const std::string& mount_path, disk_format_t format) {
  auto fd = fbl::unique_fd(open(device_path.c_str(), O_RDWR));
  if (!fd) {
    FX_LOGS(ERROR) << "Could not open device: " << device_path << ": errno=" << errno;
    return zx::error(ZX_ERR_BAD_STATE);
  }

  // fd consumed by mount. By default, mount waits until the filesystem is ready to accept commands.
  mount_options_t options = default_mount_options;
  options.register_fs = false;
  // Uncomment the following line to force an fsck at the end of every transaction (where
  // supported).
  // options.fsck_after_every_transaction = true;
  auto status = zx::make_status(
      mount(fd.release(), mount_path.c_str(), format, &options, launch_stdio_async));
  if (status.is_error()) {
    FX_LOGS(ERROR) << "Could not mount " << disk_format_string(format)
                   << " file system: " << status.status_string();
    return status;
  }
  return zx::ok();
}

zx::status<> FileSystem::Format(const std::string& device_path, disk_format_t format) {
  auto status =
      zx::make_status(mkfs(device_path.c_str(), format, launch_stdio_sync, &default_mkfs_options));
  if (status.is_error()) {
    FX_LOGS(ERROR) << "Could not format " << disk_format_string(format)
                   << " file system: " << status.status_string();
    return status;
  }
  return zx::ok();
}

class MinfsInstance : public FileSystemInstance {
 public:
  MinfsInstance(isolated_devmgr::RamDisk ram_disk, const std::string& device_path)
      : ram_disk_(std::move(ram_disk)), device_path_(device_path) {}

  zx::status<> Mount(const std::string& mount_path) override {
    return FileSystemInstance::Mount(device_path_, mount_path, DISK_FORMAT_MINFS);
  }

 private:
  isolated_devmgr::RamDisk ram_disk_;
  std::string device_path_;
};

zx::status<std::unique_ptr<FileSystemInstance>> MinfsFileSystem::Make(
    const TestFileSystemOptions& options) const {
  auto ram_disk_or = CreateRamDisk(options);
  if (ram_disk_or.is_error()) {
    return ram_disk_or.take_error();
  }
  auto [ram_disk, device_path] = std::move(ram_disk_or).value();
  zx::status<> status = FileSystem::Format(device_path, DISK_FORMAT_MINFS);
  if (status.is_error()) {
    return status.take_error();
  }
  return zx::ok(std::make_unique<MinfsInstance>(std::move(ram_disk), device_path));
}

class MemfsInstance : public FileSystemInstance {
 public:
  MemfsInstance() : loop_(&kAsyncLoopConfigNeverAttachToThread) {
    FX_CHECK(loop_.StartThread() == ZX_OK);
  }
  ~MemfsInstance() override {
    if (fs_) {
      sync_completion_t sync;
      memfs_free_filesystem(fs_, &sync);
      FX_CHECK(sync_completion_wait(&sync, zx::duration::infinite().get()) == ZX_OK);
    }
  }
  zx::status<> Format() {
    return zx::make_status(
        memfs_create_filesystem(loop_.dispatcher(), &fs_, root_.reset_and_get_address()));
  }

  zx::status<> Mount(const std::string& mount_path) override {
    auto status = zx::make_status(mount_root_handle(root_.release(), mount_path.c_str()));
    if (status.is_error())
      FX_LOGS(ERROR) << "Unable to mount: " << status.status_string();
    return status;
  }

 private:
  async::Loop loop_;
  memfs_filesystem_t* fs_ = nullptr;
  zx::channel root_;  // Only valid when not mounted.
};

zx::status<std::unique_ptr<FileSystemInstance>> MemfsFileSystem::Make(
    const TestFileSystemOptions& options) const {
  auto instance = std::make_unique<MemfsInstance>();
  zx::status<> status = instance->Format();
  if (status.is_error()) {
    return status.take_error();
  }
  return zx::ok(std::move(instance));
}

zx::status<TestFileSystem> TestFileSystem::Create(const TestFileSystemOptions& options) {
  // Make a file system.
  auto instance_or = options.file_system->Make(options);
  if (instance_or.is_error()) {
    return instance_or.take_error();
  }

  // Mount the file system.
  char mount_path_c_str[] = "/tmp/fs_test.XXXXXX";
  if (mkdtemp(mount_path_c_str) == nullptr) {
    FX_LOGS(ERROR) << "Unable to create mount point: " << errno;
    return zx::error(ZX_ERR_BAD_STATE);
  }
  std::string mount_path(mount_path_c_str);
  auto status = instance_or.value()->Mount(mount_path);
  if (status.is_error()) {
    return status.take_error();
  }
  return zx::ok(TestFileSystem(options, std::move(instance_or).value(), mount_path));
}

TestFileSystem::~TestFileSystem() {
  if (file_system_) {
    zx::status<> status = zx::make_status(umount(mount_path_.c_str()));
    if (status.is_error()) {
      FX_LOGS(WARNING) << "Failed to unmount: " << status.status_string();
    }
    rmdir(mount_path_.c_str());
  }
}

}  // namespace fs_test

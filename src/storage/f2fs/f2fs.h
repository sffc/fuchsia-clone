// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_F2FS_F2FS_H_
#define SRC_STORAGE_F2FS_F2FS_H_

// clang-format off
#ifdef __Fuchsia__
#include <zircon/device/vfs.h>

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fidl-async/cpp/bind.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <fbl/auto_lock.h>
#include <fbl/condition_variable.h>
#include <fbl/mutex.h>

#include <fidl/fuchsia.fs/cpp/wire.h>
#endif  // __Fuchsia__

#include <fcntl.h>

#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/listnode.h>
#include <zircon/types.h>

#include <lib/syslog/cpp/macros.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/zx/status.h>

#include <fbl/algorithm.h>
#include <fbl/intrusive_wavl_tree.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/macros.h>
#include <fbl/ref_ptr.h>

#include <condition_variable>

#ifdef __Fuchsia__
#include "src/lib/storage/vfs/cpp/managed_vfs.h"
#include "src/lib/storage/vfs/cpp/query_service.h"
#include "src/lib/storage/vfs/cpp/watcher.h"
#include "src/lib/storage/vfs/cpp/shared_mutex.h"
#include "src/lib/storage/vfs/cpp/service.h"
#endif  // __Fuchsia__

#include "src/lib/storage/vfs/cpp/vfs.h"
#include "src/lib/storage/vfs/cpp/vnode.h"

#include "src/storage/f2fs/f2fs_types.h"
#include "src/storage/f2fs/f2fs_lib.h"
#include "src/storage/f2fs/f2fs_layout.h"
#include "src/storage/f2fs/file_cache.h"
#include "src/storage/f2fs/f2fs_internal.h"
#include "src/storage/f2fs/namestring.h"
#include "src/storage/f2fs/bcache.h"
#include "src/storage/f2fs/vnode.h"
#include "src/storage/f2fs/dir.h"
#include "src/storage/f2fs/file.h"
#include "src/storage/f2fs/vnode_cache.h"
#include "src/storage/f2fs/node.h"
#include "src/storage/f2fs/segment.h"
#include "src/storage/f2fs/mkfs.h"
#include "src/storage/f2fs/mount.h"
#include "src/storage/f2fs/fsck.h"
#include "src/storage/f2fs/admin.h"
#include "src/storage/f2fs/dir_entry_cache.h"
// clang-format on

namespace f2fs {

enum class ServeLayout {
  // The root of the filesystem is exposed directly.
  kDataRootOnly,

  // Expose a pseudo-directory with the filesystem root located at "svc/root".
  // TODO(fxbug.dev/34531): Also expose an administration service under "svc/fuchsia.fs.Admin".
  kExportDirectory
};

zx_status_t LoadSuperblock(f2fs::Bcache *bc, Superblock *out_info);
zx_status_t LoadSuperblock(f2fs::Bcache *bc, Superblock *out_info, block_t bno);

#ifdef __Fuchsia__
zx::status<std::unique_ptr<F2fs>> CreateFsAndRoot(const MountOptions &mount_options,
                                                  async_dispatcher_t *dispatcher,
                                                  std::unique_ptr<f2fs::Bcache> bcache,
                                                  fidl::ServerEnd<fuchsia_io::Directory> root,
                                                  fit::closure on_unmount,
                                                  ServeLayout serve_layout);

using SyncCallback = fs::Vnode::SyncCallback;
#else   // __Fuchsia__
zx::status<std::unique_ptr<F2fs>> CreateFsAndRoot(const MountOptions &mount_options,
                                                  std::unique_ptr<f2fs::Bcache> bcache);
#endif  // __Fuchsia__

#ifdef __Fuchsia__
class F2fs : public fs::ManagedVfs {
#else   // __Fuchsia__
class F2fs : public fs::Vfs {
#endif  // __Fuchsia__
 public:
  // Not copyable or moveable
  F2fs(const F2fs &) = delete;
  F2fs &operator=(const F2fs &) = delete;
  F2fs(F2fs &&) = delete;
  F2fs &operator=(F2fs &&) = delete;

#ifdef __Fuchsia__
  explicit F2fs(async_dispatcher_t *dispatcher, std::unique_ptr<f2fs::Bcache> bc,
                std::unique_ptr<Superblock> sb, const MountOptions &mount_options);
#else   // __Fuchsia__
  explicit F2fs(std::unique_ptr<f2fs::Bcache> bc, std::unique_ptr<Superblock> sb,
                const MountOptions &mount_options);
#endif  // __Fuchsia__

  ~F2fs() override;

#ifdef __Fuchsia__
  [[nodiscard]] static zx_status_t Create(async_dispatcher_t *dispatcher,
                                          std::unique_ptr<f2fs::Bcache> bc,
                                          const MountOptions &options, std::unique_ptr<F2fs> *out);

  void SetUnmountCallback(fit::closure closure) { on_unmount_ = std::move(closure); }
  void Shutdown(fs::FuchsiaVfs::ShutdownCallback cb) final;
  void OnNoConnections() final;

  void SetQueryService(fbl::RefPtr<fs::QueryService> svc) { query_svc_ = std::move(svc); }
  void SetAdminService(fbl::RefPtr<AdminService> svc) { admin_svc_ = std::move(svc); }

  zx::status<fs::FilesystemInfo> GetFilesystemInfo() final;
#else   // __Fuchsia__
  [[nodiscard]] static zx_status_t Create(std::unique_ptr<f2fs::Bcache> bc,
                                          const MountOptions &options, std::unique_ptr<F2fs> *out);
#endif  // __Fuchsia__

  VnodeCache &GetVCache() { return vnode_cache_; }
  inline zx_status_t InsertVnode(VnodeF2fs *vn) { return vnode_cache_.Add(vn); }
  inline void EvictVnode(VnodeF2fs *vn) { __UNUSED zx_status_t status = vnode_cache_.Evict(vn); }
  inline zx_status_t LookupVnode(ino_t ino, fbl::RefPtr<VnodeF2fs> *out) {
    return vnode_cache_.Lookup(ino, out);
  }

#ifdef __Fuchsia__
  DirEntryCache &GetDirEntryCache() { return dir_entry_cache_; }
#endif  // __Fuchsia__

  void ResetBc(std::unique_ptr<f2fs::Bcache> *out = nullptr) {
    if (out == nullptr) {
      bc_.reset();
      return;
    }
    *out = std::move(bc_);
  }
  Bcache &GetBc() { return *bc_; }
  Superblock &RawSb() { return *raw_sb_; }
  SuperblockInfo &GetSuperblockInfo() { return *superblock_info_; }
  SegmentManager &GetSegmentManager() { return *segment_manager_; }
  NodeManager &GetNodeManager() { return *node_manager_; }

  // For testing Reset() and ResetBc()
  bool IsValid() const;
  void ResetRootVnode() { root_vnode_.reset(); }
  void ResetSuperblockInfo() { superblock_info_.reset(); }
  void ResetSegmentManager() {
    segment_manager_->DestroySegmentManager();
    segment_manager_.reset();
  }
  void ResetNodeManager() {
    node_manager_->DestroyNodeManager();
    node_manager_.reset();
  }

  // super.cc
  void PutSuper();
  zx_status_t SyncFs(int sync);
  zx_status_t SanityCheckRawSuper();
  zx_status_t SanityCheckCkpt();
  void InitSuperblockInfo();
  zx_status_t FillSuper();
  void ParseOptions();
  void Reset();
#if 0  // porting needed
  void InitOnce(void *foo);
  VnodeF2fs *F2fsAllocInode();
  static void F2fsICallback(rcu_head *head);
  void F2fsDestroyInode(inode *inode);
  int F2fsStatfs(dentry *dentry /*, kstatfs *buf*/);
  int F2fsShowOptions(/*seq_file *seq*/);
  VnodeF2fs *F2fsNfsGetInode(uint64_t ino, uint32_t generation);
  dentry *F2fsFhToDentry(fid *fid, int fh_len, int fh_type);
  dentry *F2fsFhToParent(fid *fid, int fh_len, int fh_type);
  dentry *F2fsMount(file_system_type *fs_type, int flags,
       const char *dev_name, void *data);
  int InitInodecache(void);
  void DestroyInodecache(void);
  int /*__init*/ initF2fsFs(void);
  void /*__exit*/ exitF2fsFs(void);
#endif

  // checkpoint.cc
  zx_status_t GrabMetaPage(pgoff_t index, fbl::RefPtr<Page> *out);
  zx_status_t GetMetaPage(pgoff_t index, fbl::RefPtr<Page> *out);
  zx_status_t F2fsWriteMetaPage(Page &page, bool is_reclaim = false);
  int64_t SyncMetaPages(PageType type, long nr_to_write);
  zx_status_t CheckOrphanSpace();
  void AddOrphanInode(VnodeF2fs *vnode);
  void AddOrphanInode(nid_t ino);
  void RemoveOrphanInode(nid_t ino);
  void RecoverOrphanInode(nid_t ino);
  int RecoverOrphanInodes();
  void WriteOrphanInodes(block_t start_blk);
  zx_status_t GetValidCheckpoint();
  zx_status_t ValidateCheckpoint(block_t cp_addr, uint64_t *version, fbl::RefPtr<Page> *out);
  uint64_t SyncDirtyDataPages();
  void BlockOperations();
  void UnblockOperations();
  void DoCheckpoint(bool is_umount);
  void WriteCheckpoint(bool blocked, bool is_umount);
  void InitOrphanInfo();
#if 0  // porting needed
  int F2fsWriteMetaPages(address_space *mapping, WritebackControl *wbc);
  int F2fsSetMetaPageDirty(Page *page);
  void SetDirtyDirPage(VnodeF2fs *vnode, Page *page);
  void RemoveDirtyDirInode(VnodeF2fs *vnode);
  int CreateCheckpointCaches();
  void DestroyCheckpointCaches();
#endif

  // recovery.cc
  bool SpaceForRollForward();
  FsyncInodeEntry *GetFsyncInode(list_node_t *head, nid_t ino);
  zx_status_t RecoverDentry(Page *ipage, VnodeF2fs *vnode);
  zx_status_t RecoverInode(VnodeF2fs *inode, Page *node_page);
  zx_status_t FindFsyncDnodes(list_node_t *head);
  void DestroyFsyncDnodes(list_node_t *head);
  void CheckIndexInPrevNodes(block_t blkaddr);
  void DoRecoverData(VnodeF2fs *inode, Page *page, block_t blkaddr);
  void RecoverData(list_node_t *head, CursegType type);
  void RecoverFsyncData();

  // block count
  void DecValidBlockCount(VnodeF2fs *vnode, block_t count);
  zx_status_t IncValidBlockCount(VnodeF2fs *vnode, block_t count);
  block_t ValidUserBlocks();
  uint32_t ValidNodeCount();
  void IncValidInodeCount();
  void DecValidInodeCount();
  uint32_t ValidInodeCount();
  loff_t MaxFileSize(unsigned bits);

  VnodeF2fs &GetNodeVnode() { return *node_vnode_; }
  VnodeF2fs &GetMetaVnode() { return *meta_vnode_; }

 private:
  std::unique_ptr<f2fs::Bcache> bc_;

  std::unique_ptr<VnodeF2fs> node_vnode_;
  std::unique_ptr<VnodeF2fs> meta_vnode_;
  fbl::RefPtr<VnodeF2fs> root_vnode_;
  fit::closure on_unmount_;
  MountOptions mount_options_;

  std::shared_ptr<Superblock> raw_sb_;
  std::unique_ptr<SuperblockInfo> superblock_info_;
  std::unique_ptr<SegmentManager> segment_manager_;
  std::unique_ptr<NodeManager> node_manager_;

  VnodeCache vnode_cache_;

#ifdef __Fuchsia__
  DirEntryCache dir_entry_cache_;

  fbl::RefPtr<fs::QueryService> query_svc_;
  fbl::RefPtr<AdminService> admin_svc_;

  zx::event fs_id_;
#endif  // __Fuchsia__
};

f2fs_hash_t DentryHash(std::string_view name);

zx_status_t FlushDirtyNodePage(F2fs *fs, Page &page);
zx_status_t FlushDirtyMetaPage(F2fs *fs, Page &page);
zx_status_t FlushDirtyDataPage(F2fs *fs, Page &page);

}  // namespace f2fs

#endif  // SRC_STORAGE_F2FS_F2FS_H_

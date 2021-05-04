// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        errors::FxfsError,
        object_handle::INVALID_OBJECT_ID,
        object_store::{
            self,
            directory::{self, ObjectDescriptor},
            transaction::{LockKey, Transaction},
            ObjectStore,
        },
        server::{
            errors::map_to_status,
            file::FxFile,
            node::{FxNode, GetResult},
            volume::FxVolume,
        },
    },
    anyhow::{bail, Error},
    async_trait::async_trait,
    either::{Left, Right},
    fidl::endpoints::ServerEnd,
    fidl_fuchsia_io::{
        self as fio, NodeAttributes, NodeMarker, MODE_TYPE_DIRECTORY, MODE_TYPE_FILE,
        OPEN_FLAG_CREATE, OPEN_FLAG_CREATE_IF_ABSENT, OPEN_FLAG_DIRECTORY, OPEN_FLAG_NOT_DIRECTORY,
    },
    fuchsia_async as fasync,
    fuchsia_zircon::Status,
    std::{
        any::Any,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
    },
    vfs::{
        common::send_on_open_with_error,
        directory::{
            connection::{io1::DerivedConnection, util::OpenDirectory},
            dirents_sink::{self, AppendResult, Sink},
            entry::{DirectoryEntry, EntryInfo},
            entry_container::{AsyncGetEntry, Directory, MutableDirectory},
            mutable::connection::io1::MutableConnection,
            traversal_position::TraversalPosition,
        },
        execution_scope::ExecutionScope,
        filesystem::Filesystem,
        path::Path,
    },
};

pub struct FxDirectory {
    // The root directory is the only directory which has no parent, and its parent can never
    // change, hence the Option can go on the outside.
    parent: Option<Mutex<Arc<FxDirectory>>>,
    directory: object_store::Directory<FxVolume>,
    is_deleted: AtomicBool,
}

impl FxDirectory {
    pub(super) fn new(
        parent: Option<Arc<FxDirectory>>,
        directory: object_store::Directory<FxVolume>,
    ) -> Self {
        Self {
            parent: parent.map(|p| Mutex::new(p)),
            directory,
            is_deleted: AtomicBool::new(false),
        }
    }

    pub(super) fn directory(&self) -> &object_store::Directory<FxVolume> {
        &self.directory
    }

    pub fn volume(&self) -> &Arc<FxVolume> {
        self.directory.owner()
    }

    pub fn store(&self) -> &ObjectStore {
        self.directory.store()
    }

    /// Acquires a transaction with the appropriate locks to unlink |name|. Returns the transaction,
    /// as well as the ID and type of the child.
    ///
    /// We always need to lock |self|, but we only need to lock the child if it's a directory,
    /// to prevent entries being added to the directory.
    pub(super) async fn acquire_transaction_for_unlink<'a>(
        self: &Arc<Self>,
        extra_keys: &[LockKey],
        name: &str,
    ) -> Result<(Transaction<'a>, u64, ObjectDescriptor), Error> {
        // Since we don't know the child object ID until we've looked up the child, we need to loop
        // until we have acquired a lock on a child whose ID is the same as it was in the last
        // iteration (or the child is a file, at which point it doesn't matter).
        //
        // Note that the returned transaction may lock more objects than is necessary (for example,
        // if the child "foo" was first a directory, then was renamed to "bar" and a file "foo" was
        // created, we might acquire a lock on both the parent and "bar").
        let store = self.store();
        let mut child_object_id = INVALID_OBJECT_ID;
        loop {
            let mut lock_keys = if child_object_id == INVALID_OBJECT_ID {
                vec![LockKey::object(store.store_object_id(), self.object_id())]
            } else {
                vec![
                    LockKey::object(store.store_object_id(), self.object_id()),
                    LockKey::object(store.store_object_id(), child_object_id),
                ]
            };
            lock_keys.extend_from_slice(extra_keys);
            let fs = store.filesystem().clone();
            let transaction = fs.new_transaction(&lock_keys).await?;

            let (object_id, object_descriptor) =
                self.directory.lookup(name).await?.ok_or(FxfsError::NotFound)?;
            match object_descriptor {
                ObjectDescriptor::File => {
                    return Ok((transaction, object_id, object_descriptor));
                }
                ObjectDescriptor::Directory => {
                    if object_id == child_object_id {
                        return Ok((transaction, object_id, object_descriptor));
                    }
                    child_object_id = object_id;
                }
                ObjectDescriptor::Volume => bail!(FxfsError::Inconsistent),
            }
        }
    }

    fn ensure_writable(&self) -> Result<(), Error> {
        if self.is_deleted.load(Ordering::Relaxed) {
            bail!(FxfsError::Deleted)
        }
        Ok(())
    }

    async fn lookup(
        self: &Arc<Self>,
        flags: u32,
        mode: u32,
        mut path: Path,
    ) -> Result<Arc<dyn FxNode>, Error> {
        let store = self.store();
        let fs = store.filesystem();
        let mut current_node = self.clone() as Arc<dyn FxNode>;
        while !path.is_empty() {
            let last_segment = path.is_single_component();
            let current_dir =
                current_node.into_any().downcast::<FxDirectory>().map_err(|_| FxfsError::NotDir)?;
            let name = path.next().unwrap();

            // Create the transaction here if we might need to create the object so that we have a
            // lock in place.
            let keys =
                [LockKey::object(store.store_object_id(), current_dir.directory.object_id())];
            let transaction_or_guard = if last_segment && flags & OPEN_FLAG_CREATE != 0 {
                Left(fs.clone().new_transaction(&keys).await?)
            } else {
                // When child objects are created, the object is created along with the directory
                // entry in the same transaction, and so we need to hold a read lock over the lookup
                // and open calls.
                Right(fs.read_lock(&keys).await)
            };

            current_node = match current_dir.directory.lookup(name).await? {
                Some((object_id, object_descriptor)) => {
                    if transaction_or_guard.is_left() && flags & OPEN_FLAG_CREATE_IF_ABSENT != 0 {
                        bail!(FxfsError::AlreadyExists);
                    }
                    if last_segment {
                        match object_descriptor {
                            ObjectDescriptor::File => {
                                if mode & MODE_TYPE_DIRECTORY > 0 || flags & OPEN_FLAG_DIRECTORY > 0
                                {
                                    bail!(FxfsError::NotDir)
                                }
                            }
                            ObjectDescriptor::Directory => {
                                if mode & MODE_TYPE_FILE > 0 || flags & OPEN_FLAG_NOT_DIRECTORY > 0
                                {
                                    bail!(FxfsError::NotFile)
                                }
                            }
                            ObjectDescriptor::Volume => bail!(FxfsError::Inconsistent),
                        }
                    }
                    self.volume()
                        .get_or_load_node(object_id, object_descriptor, Some(self.clone()))
                        .await?
                }
                None => {
                    if let Left(mut transaction) = transaction_or_guard {
                        let node = current_dir.create_child(&mut transaction, name, mode).await?;
                        if let GetResult::Placeholder(p) =
                            self.volume().cache().get_or_reserve(node.object_id()).await
                        {
                            p.commit(&node);
                            transaction.commit().await;
                        } else {
                            // We created a node, but the object ID was already used in the cache,
                            // which suggests a object ID was reused (which would either be a bug or
                            // corruption).
                            bail!(FxfsError::Inconsistent);
                        }
                        node
                    } else {
                        bail!(FxfsError::NotFound);
                    }
                }
            };
        }
        Ok(current_node)
    }

    async fn create_child(
        self: &Arc<Self>,
        transaction: &mut Transaction<'_>,
        name: &str,
        mode: u32,
    ) -> Result<Arc<dyn FxNode>, Error> {
        self.ensure_writable()?;
        if mode & MODE_TYPE_DIRECTORY != 0 {
            Ok(Arc::new(FxDirectory::new(
                Some(self.clone()),
                self.directory.create_child_dir(transaction, name).await?,
            )) as Arc<dyn FxNode>)
        } else {
            Ok(Arc::new(FxFile::new(self.directory.create_child_file(transaction, name).await?))
                as Arc<dyn FxNode>)
        }
    }

    /// Replaces |name| (which is object |object_id| of type |descriptor|) with |replacement|
    /// (which can be a different directory).  If |replacement| is unset, simply removes the object.
    ///
    /// It is the caller's responsibility to make sure that |name|, |object_id|, and
    /// |object_descriptor| all refer to the same object.
    ///
    /// If the child was a directory, returns the child node which MUST NOT be dropped until
    /// |transaction| is committed. This is necessary to ensure that the node isn't evicted from the
    /// cache before |transaction| finishes and actually unlinks the node.
    ///
    /// TODO(csuter): Separate out a commit_prepare which acquires the lock, so that we can handle
    /// situations like this more gracefully. (Alternatively, add a callback to commit.)
    #[must_use]
    pub(super) async fn replace_child<'a>(
        self: &'a Arc<Self>,
        transaction: &mut Transaction<'a>,
        name: &str,
        object_id: u64,
        descriptor: ObjectDescriptor,
        replacement: Option<(&'a Arc<FxDirectory>, &str)>,
    ) -> Result<Option<Arc<dyn FxNode>>, Error> {
        let dir = if let ObjectDescriptor::File = descriptor {
            // TODO(jfsulliv): We can immediately delete files if they have no open references, but
            // we need appropriate locking in place to be able to check the file's open count.
            None
        } else {
            // For directories, we need to set |is_deleted| on the in-memory node. If the node's
            // absent from the cache, we *must* load the node into memory, and make sure the node
            // stays in the cache until we commit the transaction, because otherwise another caller
            // could load the node from disk before we commit the transaction and would not see that
            // it's been unlinked.
            // Holding a placeholder here could deadlock, since transaction.commit() will block
            // until there are no readers, but a reader could be blocked on the cache by the
            // placeholder.
            let dir = self
                .volume()
                .get_or_load_node(object_id, descriptor, Some(self.clone()))
                .await?
                .into_any()
                .downcast::<FxDirectory>()
                .unwrap();
            dir.is_deleted.store(true, Ordering::Relaxed);
            Some(dir as Arc<dyn FxNode>)
        };
        // TODO(jfsulliv): This results in an unnecessary second lookup; we could pass in object_id
        // and descriptor here, since we've already resolved them.
        if let Some((existing_oid, descriptor)) = directory::replace_child(
            transaction,
            replacement.map(|(dir, name)| (dir.directory(), name)),
            (self.directory(), name),
        )
        .await?
        {
            let store = self.store();
            if let ObjectDescriptor::File = descriptor {
                store
                    .filesystem()
                    .object_manager()
                    .graveyard(store.store_object_id())
                    .unwrap()
                    .insert_child(
                        transaction,
                        &format!("{}", existing_oid),
                        existing_oid,
                        descriptor,
                    );
            } else {
                directory::remove(transaction, &store, existing_oid);
            }
        }
        Ok(dir)
    }

    // TODO(jfsulliv): Change the VFS to send in &Arc<Self> so we don't need this.
    async fn as_strong(&self) -> Arc<Self> {
        self.volume()
            .get_or_load_node(self.object_id(), ObjectDescriptor::Directory, self.parent())
            .await
            .expect("open_or_load_node on self failed")
            .into_any()
            .downcast::<FxDirectory>()
            .unwrap()
    }
}

impl Drop for FxDirectory {
    fn drop(&mut self) {
        self.volume().cache().remove(self.object_id());
    }
}

impl FxNode for FxDirectory {
    fn object_id(&self) -> u64 {
        self.directory.object_id()
    }
    fn parent(&self) -> Option<Arc<FxDirectory>> {
        self.parent.as_ref().map(|p| p.lock().unwrap().clone())
    }
    fn set_parent(&self, parent: Arc<FxDirectory>) {
        match &self.parent {
            Some(p) => *p.lock().unwrap() = parent,
            None => panic!("Called set_parent on root node"),
        }
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }
}

#[async_trait]
impl MutableDirectory for FxDirectory {
    async fn link(&self, _name: String, _entry: Arc<dyn DirectoryEntry>) -> Result<(), Status> {
        log::error!("link not implemented");
        Err(Status::NOT_SUPPORTED)
    }

    async fn unlink(&self, name: &str, _must_be_directory: bool) -> Result<(), Status> {
        // TODO(csuter): do something with must_be_directory.
        let this = self.as_strong().await;
        let (mut transaction, object_id, object_descriptor) =
            this.acquire_transaction_for_unlink(&[], name).await.map_err(map_to_status)?;
        // _dir must be held until after the transaction commits; see FxDirectory::replace_child.
        let _dir = this
            .replace_child(&mut transaction, name, object_id, object_descriptor, None)
            .await
            .map_err(map_to_status)?;
        transaction.commit().await;
        Ok(())
    }

    async fn set_attrs(&self, _flags: u32, _attrs: NodeAttributes) -> Result<(), Status> {
        log::error!("set_attrs not implemented");
        Err(Status::NOT_SUPPORTED)
    }

    fn get_filesystem(&self) -> &dyn Filesystem {
        self.volume().as_ref()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Sync + Send> {
        self as Arc<dyn Any + Sync + Send>
    }

    async fn sync(&self) -> Result<(), Status> {
        // TODO(csuter): Support sync on root of fxfs volume.
        Ok(())
    }
}

impl DirectoryEntry for FxDirectory {
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        flags: u32,
        mode: u32,
        path: Path,
        server_end: ServerEnd<NodeMarker>,
    ) {
        let cloned_scope = scope.clone();
        scope.spawn(async move {
            // TODO(jfsulliv): Factor this out into a visitor-pattern style method for FxNode, e.g.
            // FxNode::visit(FileFn, DirFn).
            match self.lookup(flags, mode, path).await {
                Err(e) => send_on_open_with_error(flags, server_end, map_to_status(e)),
                Ok(node) => {
                    if let Ok(dir) = node.clone().into_any().downcast::<FxDirectory>() {
                        MutableConnection::create_connection(
                            cloned_scope,
                            OpenDirectory::new(dir),
                            flags,
                            mode,
                            server_end,
                        );
                    } else if let Ok(file) = node.into_any().downcast::<FxFile>() {
                        file.clone().open(cloned_scope, flags, mode, Path::empty(), server_end);
                    } else {
                        unreachable!();
                    }
                }
            };
        });
    }

    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(fio::INO_UNKNOWN, fio::DIRENT_TYPE_DIRECTORY)
    }

    fn can_hardlink(&self) -> bool {
        false
    }
}

#[async_trait]
impl Directory for FxDirectory {
    fn get_entry(self: Arc<Self>, _name: String) -> AsyncGetEntry {
        // TODO(jfsulliv): Implement
        AsyncGetEntry::Immediate(Err(Status::NOT_FOUND))
    }

    async fn read_dirents<'a>(
        &'a self,
        pos: &'a TraversalPosition,
        mut sink: Box<dyn Sink>,
    ) -> Result<(TraversalPosition, Box<dyn dirents_sink::Sealed>), Status> {
        if let TraversalPosition::End = pos {
            return Ok((TraversalPosition::End, sink.seal()));
        } else if let TraversalPosition::Index(_) = pos {
            // The VFS should never send this to us, since we never return it here.
            return Err(Status::BAD_STATE);
        }

        let store = self.store();
        let fs = store.filesystem();
        let _read_guard =
            fs.read_lock(&[LockKey::object(store.store_object_id(), self.object_id())]).await;
        if self.is_deleted.load(Ordering::Relaxed) {
            return Ok((TraversalPosition::End, sink.seal()));
        }

        let starting_name = match pos {
            TraversalPosition::Start => {
                // Synthesize a "." entry if we're at the start of the stream.
                match sink
                    .append(&EntryInfo::new(fio::INO_UNKNOWN, fio::DIRENT_TYPE_DIRECTORY), ".")
                {
                    AppendResult::Ok(new_sink) => sink = new_sink,
                    AppendResult::Sealed(sealed) => {
                        // Note that the VFS should have yielded an error since the first entry
                        // didn't fit. This is defensive in case the VFS' behaviour changes, so that
                        // we return a reasonable value.
                        return Ok((TraversalPosition::Start, sealed));
                    }
                }
                ""
            }
            TraversalPosition::Name(name) => name,
            _ => unreachable!(),
        };

        let layer_set = self.store().tree().layer_set();
        let mut merger = layer_set.merger();
        let mut iter =
            self.directory.iter_from(&mut merger, starting_name).await.map_err(map_to_status)?;
        while let Some((name, object_id, object_descriptor)) = iter.get() {
            let entry_type = match object_descriptor {
                ObjectDescriptor::File => fio::DIRENT_TYPE_FILE,
                ObjectDescriptor::Directory => fio::DIRENT_TYPE_DIRECTORY,
                ObjectDescriptor::Volume => return Err(Status::IO_DATA_INTEGRITY),
            };
            let info = EntryInfo::new(object_id, entry_type);
            match sink.append(&info, name) {
                AppendResult::Ok(new_sink) => sink = new_sink,
                AppendResult::Sealed(sealed) => {
                    // We did *not* add the current entry to the sink (e.g. because the sink was
                    // full), so mark |name| as the next position so that it's the first entry we
                    // process on a subsequent call of read_dirents.
                    // Note that entries inserted between the previous entry and this entry before
                    // the next call to read_dirents would not be included in the results (but
                    // there's no requirement to include them anyways).
                    return Ok((TraversalPosition::Name(name.to_owned()), sealed));
                }
            }
            iter.advance().await.map_err(map_to_status)?;
        }
        Ok((TraversalPosition::End, sink.seal()))
    }

    fn register_watcher(
        self: Arc<Self>,
        _scope: ExecutionScope,
        _mask: u32,
        _channel: fasync::Channel,
    ) -> Result<(), Status> {
        // TODO(jfsulliv): Implement
        Err(Status::NOT_SUPPORTED)
    }

    fn unregister_watcher(self: Arc<Self>, _key: usize) {
        // TODO(jfsulliv): Implement
    }

    async fn get_attrs(&self) -> Result<NodeAttributes, Status> {
        Err(Status::NOT_SUPPORTED)
    }

    fn close(&self) -> Result<(), Status> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            device::DeviceHolder,
            server::testing::{
                close_dir_checked, close_file_checked, open_dir, open_dir_checked, open_file,
                open_file_checked, TestFixture,
            },
            testing::fake_device::FakeDevice,
        },
        fidl_fuchsia_io::{
            DirectoryProxy, SeekOrigin, MAX_BUF, MODE_TYPE_DIRECTORY, MODE_TYPE_FILE,
            OPEN_FLAG_CREATE, OPEN_FLAG_CREATE_IF_ABSENT, OPEN_RIGHT_READABLE, OPEN_RIGHT_WRITABLE,
        },
        fidl_fuchsia_io2::UnlinkOptions,
        files_async::{DirEntry, DirentKind},
        fuchsia_async as fasync,
        fuchsia_zircon::Status,
        io_util::{read_file_bytes, write_file_bytes},
        rand::Rng,
        std::{sync::Arc, time::Duration},
    };

    #[fasync::run_singlethreaded(test)]
    async fn test_open_root_dir() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();
        root.describe().await.expect("Describe failed");
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_create_dir_persists() {
        let mut device = DeviceHolder::new(FakeDevice::new(2048, 512));
        for i in 0..2 {
            let fixture = TestFixture::open(device, /*format=*/ i == 0).await;
            let root = fixture.root();

            let flags =
                if i == 0 { OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE } else { OPEN_RIGHT_READABLE };
            let dir = open_dir_checked(&root, flags, MODE_TYPE_DIRECTORY, "foo").await;
            close_dir_checked(dir).await;

            device = fixture.close().await;
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_open_nonexistent_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        assert_eq!(
            open_file(&root, OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "foo")
                .await
                .expect_err("Open succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::NOT_FOUND,
        );

        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_create_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let f =
            open_file_checked(&root, OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "foo")
                .await;
        close_file_checked(f).await;

        let f = open_file_checked(&root, OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "foo").await;
        close_file_checked(f).await;

        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_create_dir_nested() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let d = open_dir_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_DIRECTORY,
            "foo",
        )
        .await;
        close_dir_checked(d).await;

        let d = open_dir_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE,
            MODE_TYPE_DIRECTORY,
            "foo/bar",
        )
        .await;
        close_dir_checked(d).await;

        let d = open_dir_checked(&root, OPEN_RIGHT_READABLE, MODE_TYPE_DIRECTORY, "foo/bar").await;
        close_dir_checked(d).await;

        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_strict_create_file_fails_if_present() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let f = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_FLAG_CREATE_IF_ABSENT | OPEN_RIGHT_READABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;
        close_file_checked(f).await;

        assert_eq!(
            open_file(
                &root,
                OPEN_FLAG_CREATE | OPEN_FLAG_CREATE_IF_ABSENT | OPEN_RIGHT_READABLE,
                MODE_TYPE_FILE,
                "foo",
            )
            .await
            .expect_err("Open succeeded")
            .root_cause()
            .downcast_ref::<Status>()
            .expect("No status"),
            &Status::ALREADY_EXISTS,
        );

        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    #[ignore] // TODO(jfsulliv): Re-enable when we don't defer deleting files with 0 references.
    async fn test_unlink_file_with_no_refs_immediately_freed() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;

        // Fill up the file with a lot of data, so we can verify that the extents are freed.
        let buf = vec![0xaa as u8; 512];
        loop {
            match write_file_bytes(&file, buf.as_slice()).await {
                Ok(_) => {}
                Err(e) => {
                    if let Some(status) = e.root_cause().downcast_ref::<Status>() {
                        if status == &Status::NO_SPACE {
                            break;
                        }
                    }
                    panic!("Unexpected write error {:?}", e);
                }
            }
        }

        close_file_checked(file).await;

        root.unlink("foo").await.expect("unlink failed");

        assert_eq!(
            open_file(&root, OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "foo")
                .await
                .expect_err("Open succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::NOT_FOUND,
        );

        // Create another file so we can verify that the extents were actually freed.
        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "bar",
        )
        .await;
        let buf = vec![0xaa as u8; 8192];
        write_file_bytes(&file, buf.as_slice()).await.expect("Failed to write new file");
        close_file_checked(file).await;

        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_unlink_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;
        close_file_checked(file).await;

        Status::ok(root.unlink("foo").await.expect("FIDL call failed")).expect("unlink failed");

        assert_eq!(
            open_file(&root, OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "foo")
                .await
                .expect_err("Open succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::NOT_FOUND,
        );

        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_unlink_file_with_active_references() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;

        let buf = vec![0xaa as u8; 512];
        write_file_bytes(&file, buf.as_slice()).await.expect("write failed");

        Status::ok(root.unlink("foo").await.expect("FIDL call failed")).expect("unlink failed");

        // The child should immediately appear unlinked...
        assert_eq!(
            open_file(&root, OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "foo")
                .await
                .expect_err("Open succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::NOT_FOUND,
        );

        // But its contents should still be readable from the other handle.
        file.seek(0, SeekOrigin::Start).await.expect("seek failed");
        let rbuf = read_file_bytes(&file).await.expect("read failed");
        assert_eq!(rbuf, buf);
        close_file_checked(file).await;

        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_unlink_dir_with_children_fails() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let dir = open_dir_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_DIRECTORY,
            "foo",
        )
        .await;
        let f =
            open_file_checked(&dir, OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "bar")
                .await;
        close_file_checked(f).await;

        assert_eq!(
            Status::from_raw(root.unlink("foo").await.expect("FIDL call failed")),
            Status::NOT_EMPTY
        );

        Status::ok(dir.unlink("bar").await.expect("FIDL call failed")).expect("unlink failed");
        Status::ok(root.unlink("foo").await.expect("FIDL call failed")).expect("unlink failed");

        close_dir_checked(dir).await;

        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_unlink_dir_makes_directory_immutable() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let dir = open_dir_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_DIRECTORY,
            "foo",
        )
        .await;

        Status::ok(root.unlink("foo").await.expect("FIDL call failed")).expect("unlink failed");

        assert_eq!(
            open_file(&dir, OPEN_RIGHT_READABLE | OPEN_FLAG_CREATE, MODE_TYPE_FILE, "bar")
                .await
                .expect_err("Create file succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::ACCESS_DENIED,
        );

        close_dir_checked(dir).await;

        fixture.close().await;
    }

    #[fasync::run(10, test)]
    async fn test_unlink_directory_with_children_race() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        const PARENT: &str = "foo";
        const CHILD: &str = "bar";
        const GRANDCHILD: &str = "baz";
        open_dir_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_DIRECTORY,
            PARENT,
        )
        .await;

        let open_parent = || async {
            open_dir_checked(
                &root,
                OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                MODE_TYPE_DIRECTORY,
                PARENT,
            )
            .await
        };
        let parent = open_parent().await;

        // Each iteration proceeds as follows:
        //  - Initialize a directory foo/bar/. (This might still be around from the previous
        //    iteration, which is fine.)
        //  - In one task, try to unlink foo/bar/.
        //  - In another task, try to add a file foo/bar/baz.
        for _ in 0..100 {
            let d = open_dir_checked(
                &parent,
                OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                MODE_TYPE_DIRECTORY,
                CHILD,
            )
            .await;
            close_dir_checked(d).await;

            let parent = open_parent().await;
            let deleter = fasync::Task::spawn(async move {
                let wait_time = rand::thread_rng().gen_range(0, 5);
                fasync::Timer::new(Duration::from_millis(wait_time)).await;
                match parent
                    .unlink2(CHILD, UnlinkOptions::EMPTY)
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::from_raw)
                {
                    Ok(()) => {}
                    Err(Status::NOT_EMPTY) => {}
                    Err(e) => panic!("Unexpected status from unlink: {:?}", e),
                };
                close_dir_checked(parent).await;
            });

            let parent = open_parent().await;
            let writer = fasync::Task::spawn(async move {
                let child_or = open_dir(
                    &parent,
                    OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                    MODE_TYPE_DIRECTORY,
                    CHILD,
                )
                .await;
                if let Err(e) = &child_or {
                    // The directory was already deleted.
                    assert_eq!(
                        e.root_cause().downcast_ref::<Status>().expect("No status"),
                        &Status::NOT_FOUND
                    );
                    close_dir_checked(parent).await;
                    return;
                }
                let child = child_or.unwrap();
                child.describe().await.expect("describe failed");
                match open_file(
                    &child,
                    OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE,
                    MODE_TYPE_FILE,
                    GRANDCHILD,
                )
                .await
                {
                    Ok(grandchild) => {
                        grandchild.describe().await.expect("describe failed");
                        close_file_checked(grandchild).await;
                        // We added the child before the directory was deleted; go ahead and
                        // clean up.
                        Status::ok(child.unlink(GRANDCHILD).await.expect("FIDL call failed"))
                            .expect("unlink failed");
                    }
                    Err(e) => {
                        // The directory started to be deleted before we created a child.
                        // Make sure we get the right error.
                        assert_eq!(
                            e.root_cause().downcast_ref::<Status>().expect("No status"),
                            &Status::ACCESS_DENIED,
                        );
                    }
                };
                close_dir_checked(child).await;
                close_dir_checked(parent).await;
            });
            writer.await;
            deleter.await;
        }

        close_dir_checked(parent).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_readdir() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let open_dir = || {
            open_dir_checked(
                &root,
                OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                MODE_TYPE_DIRECTORY,
                "foo",
            )
        };
        let parent = Arc::new(open_dir().await);

        let files = ["eenie", "meenie", "minie", "moe"];
        for file in &files {
            let file =
                open_file_checked(parent.as_ref(), OPEN_FLAG_CREATE, MODE_TYPE_FILE, file).await;
            close_file_checked(file).await;
        }
        let dirs = ["fee", "fi", "fo", "fum"];
        for dir in &dirs {
            let dir =
                open_dir_checked(parent.as_ref(), OPEN_FLAG_CREATE, MODE_TYPE_DIRECTORY, dir).await;
            close_dir_checked(dir).await;
        }

        let readdir = |dir: Arc<DirectoryProxy>| async move {
            let status = dir.rewind().await.expect("FIDL call failed");
            Status::ok(status).expect("rewind failed");
            let (status, buf) = dir.read_dirents(MAX_BUF).await.expect("FIDL call failed");
            Status::ok(status).expect("read_dirents failed");
            let mut entries = vec![];
            for res in files_async::parse_dir_entries(&buf) {
                entries.push(res.expect("Failed to parse entry"));
            }
            entries
        };

        let mut expected_entries =
            vec![DirEntry { name: ".".to_owned(), kind: DirentKind::Directory }];
        expected_entries.extend(
            files.iter().map(|&name| DirEntry { name: name.to_owned(), kind: DirentKind::File }),
        );
        expected_entries.extend(
            dirs.iter()
                .map(|&name| DirEntry { name: name.to_owned(), kind: DirentKind::Directory }),
        );
        expected_entries.sort_unstable();
        assert_eq!(expected_entries, readdir(Arc::clone(&parent)).await);

        // Remove an entry.
        Status::ok(
            parent.unlink(&expected_entries.pop().unwrap().name).await.expect("FIDL call failed"),
        )
        .expect("unlink failed");

        assert_eq!(expected_entries, readdir(Arc::clone(&parent)).await);

        close_dir_checked(Arc::try_unwrap(parent).unwrap()).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_readdir_multiple_calls() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let parent = open_dir_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_DIRECTORY,
            "foo",
        )
        .await;

        let files = ["a", "b"];
        for file in &files {
            let file = open_file_checked(&parent, OPEN_FLAG_CREATE, MODE_TYPE_FILE, file).await;
            close_file_checked(file).await;
        }

        // TODO(jfsulliv): Magic number; can we get this from io.fidl?
        const DIRENT_SIZE: u64 = 10; // inode: u64, size: u8, kind: u8
        const BUFFER_SIZE: u64 = DIRENT_SIZE + 2; // Enough space for a 2-byte name.

        let parse_entries = |buf| {
            let mut entries = vec![];
            for res in files_async::parse_dir_entries(buf) {
                entries.push(res.expect("Failed to parse entry"));
            }
            entries
        };

        let expected_entries = vec![
            DirEntry { name: ".".to_owned(), kind: DirentKind::Directory },
            DirEntry { name: "a".to_owned(), kind: DirentKind::File },
        ];
        let (status, buf) = parent.read_dirents(2 * BUFFER_SIZE).await.expect("FIDL call failed");
        Status::ok(status).expect("read_dirents failed");
        assert_eq!(expected_entries, parse_entries(&buf));

        let expected_entries = vec![DirEntry { name: "b".to_owned(), kind: DirentKind::File }];
        let (status, buf) = parent.read_dirents(2 * BUFFER_SIZE).await.expect("FIDL call failed");
        Status::ok(status).expect("read_dirents failed");
        assert_eq!(expected_entries, parse_entries(&buf));

        // Subsequent calls yield nothing.
        let expected_entries: Vec<DirEntry> = vec![];
        let (status, buf) = parent.read_dirents(2 * BUFFER_SIZE).await.expect("FIDL call failed");
        Status::ok(status).expect("read_dirents failed");
        assert_eq!(expected_entries, parse_entries(&buf));

        close_dir_checked(parent).await;
        fixture.close().await;
    }
}

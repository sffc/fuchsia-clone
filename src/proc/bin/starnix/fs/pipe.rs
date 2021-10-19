// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use parking_lot::Mutex;
use std::convert::TryInto;
use std::sync::Arc;

use crate::errno;
use crate::error;
use crate::fd_impl_nonseekable;
use crate::fs::buffers::*;
use crate::fs::*;
use crate::mm::PAGE_SIZE;
use crate::signals::{signal_handling::send_checked_signal, *};
use crate::syscalls::*;
use crate::task::*;
use crate::types::*;

const ATOMIC_IO_BYTES: usize = 4096;
const PIPE_MAX_SIZE: usize = 1048576; // From pipe.go in gVisor.

fn round_up(value: usize, increment: usize) -> usize {
    (value + (increment - 1)) & !(increment - 1)
}

pub struct Pipe {
    messages: MessageQueue,

    waiters: WaitQueue,

    /// The number of open readers.
    reader_count: usize,

    /// Whether the pipe has ever had a reader.
    had_reader: bool,

    /// The number of open writers.
    writer_count: usize,

    /// Whether the pipe has ever had a writer.
    had_writer: bool,
}

impl Default for Pipe {
    fn default() -> Self {
        // The default size of a pipe is 16 pages.
        let default_pipe_capacity = (*PAGE_SIZE * 16) as usize;

        Pipe {
            messages: MessageQueue::new(default_pipe_capacity),
            waiters: WaitQueue::default(),
            reader_count: 0,
            had_reader: false,
            writer_count: 0,
            had_writer: false,
        }
    }
}

pub type PipeHandle = Arc<Mutex<Pipe>>;

impl Pipe {
    pub fn new() -> PipeHandle {
        Arc::new(Mutex::new(Pipe::default()))
    }

    pub fn open(pipe: &Arc<Mutex<Self>>, flags: OpenFlags) -> Box<dyn FileOps> {
        let mut events = FdEvents::empty();
        let mut pipe_locked = pipe.lock();
        if flags.can_read() {
            if !pipe_locked.had_reader {
                events |= FdEvents::POLLOUT;
            }
            pipe_locked.add_reader();
        }
        if flags.can_write() {
            if !pipe_locked.had_writer {
                events |= FdEvents::POLLIN;
            }
            pipe_locked.add_writer();
        }
        if events != FdEvents::empty() {
            pipe_locked.waiters.notify_events(events);
        }
        Box::new(PipeFileObject { pipe: Arc::clone(pipe) })
    }

    /// Increments the reader count for this pipe by 1.
    // TODO: This can be removed once the read/write logic has been extracted so sockets can use
    // that without the pipe semantics.
    pub fn add_reader(&mut self) {
        self.reader_count += 1;
        self.had_reader = true;
    }

    /// Increments the writer count for this pipe by 1.
    // TODO: This can be removed once the read/write logic has been extracted so sockets can use
    // that without the pipe semantics.
    pub fn add_writer(&mut self) {
        self.writer_count += 1;
        self.had_writer = true;
    }

    fn capacity(&self) -> usize {
        self.messages.capacity()
    }

    fn set_capacity(&mut self, mut requested_capacity: usize) -> Result<(), Errno> {
        if requested_capacity > PIPE_MAX_SIZE {
            return error!(EINVAL);
        }
        let page_size = *PAGE_SIZE as usize;
        if requested_capacity < page_size {
            requested_capacity = page_size;
        }
        requested_capacity = round_up(requested_capacity, page_size);
        self.messages.set_capacity(requested_capacity)
    }

    pub fn read(
        &mut self,
        task: &Task,
        user_buffers: &mut UserBufferIterator<'_>,
    ) -> Result<usize, Errno> {
        // If there isn't any data to read from the pipe, then the behavior
        // depends on whether there are any open writers. If there is an
        // open writer, then we return EAGAIN, to signal that the callers
        // should wait for the writer to write something into the pipe.
        // Otherwise, we'll fall through the rest of this function and
        // return that we have read zero bytes, which will let the caller
        // know that they're done reading the pipe.
        if self.messages.len() == 0 && (self.writer_count > 0 || !self.had_writer) {
            return error!(EAGAIN);
        }

        self.messages.read_stream(task, user_buffers).map(|info| info.bytes_read)
    }

    pub fn write(
        &mut self,
        task: &Task,
        user_buffers: &mut UserBufferIterator<'_>,
    ) -> Result<usize, Errno> {
        if !self.had_reader {
            return error!(EAGAIN);
        }

        if self.reader_count == 0 {
            send_checked_signal(task, Signal::SIGPIPE);
            return error!(EPIPE);
        }

        self.messages.write_stream(task, user_buffers, None, &mut None)
    }

    fn fcntl(
        &mut self,
        _file: &FileObject,
        _task: &Task,
        cmd: u32,
        arg: u64,
    ) -> Result<SyscallResult, Errno> {
        match cmd {
            F_GETPIPE_SZ => Ok(self.capacity().into()),
            F_SETPIPE_SZ => {
                self.set_capacity(arg as usize)?;
                Ok(self.capacity().into())
            }
            _ => error!(EINVAL),
        }
    }

    fn ioctl(
        &self,
        _file: &FileObject,
        task: &Task,
        request: u32,
        in_addr: UserAddress,
        _out_addr: UserAddress,
    ) -> Result<SyscallResult, Errno> {
        match request {
            FIONREAD => {
                let addr = UserRef::<i32>::new(in_addr);
                let value: i32 = self.messages.len().try_into().map_err(|_| errno!(EINVAL))?;
                task.mm.write_object(addr, &value)?;
                Ok(SUCCESS)
            }
            _ => default_ioctl(request),
        }
    }
}

/// Creates a new pipe between the two returned FileObjects.
///
/// The first FileObject is the read endpoint of the pipe. The second is the
/// write endpoint of the pipe. This order matches the order expected by
/// sys_pipe2().
pub fn new_pipe(kernel: &Kernel) -> Result<(FileHandle, FileHandle), Errno> {
    let fs = pipe_fs(kernel);
    let mode = FileMode::IFIFO | FileMode::from_bits(0o600);
    let node = fs.create_node(Box::new(SpecialNode), mode);
    node.info_write().blksize = ATOMIC_IO_BYTES;

    let open = |flags: OpenFlags| {
        Ok(FileObject::new_anonymous(node.open(flags)?, Arc::clone(&node), flags))
    };

    Ok((open(OpenFlags::RDONLY)?, open(OpenFlags::WRONLY)?))
}

struct PipeFs;
impl FileSystemOps for PipeFs {}
fn pipe_fs(kernel: &Kernel) -> &FileSystemHandle {
    kernel.pipe_fs.get_or_init(|| FileSystem::new(PipeFs))
}

struct PipeFileObject {
    pipe: Arc<Mutex<Pipe>>,
}

impl FileOps for PipeFileObject {
    fd_impl_nonseekable!();

    fn close(&self, file: &FileObject) {
        let mut events = FdEvents::empty();
        let mut pipe = self.pipe.lock();
        if file.flags().can_read() {
            assert!(pipe.reader_count > 0);
            pipe.reader_count -= 1;
            if pipe.reader_count == 0 {
                events |= FdEvents::POLLOUT;
            }
        }
        if file.flags().can_write() {
            assert!(pipe.writer_count > 0);
            pipe.writer_count -= 1;
            if pipe.writer_count == 0 {
                events |= FdEvents::POLLIN;
            }
        }
        if events != FdEvents::empty() {
            pipe.waiters.notify_events(events);
        }
    }

    fn read(&self, _file: &FileObject, task: &Task, data: &[UserBuffer]) -> Result<usize, Errno> {
        let mut user_buffers = UserBufferIterator::new(data);
        let mut pipe = self.pipe.lock();
        let actual = pipe.read(task, &mut user_buffers)?;
        if actual > 0 {
            pipe.waiters.notify_events(FdEvents::POLLOUT);
        }
        Ok(actual)
    }

    fn write(&self, file: &FileObject, task: &Task, data: &[UserBuffer]) -> Result<usize, Errno> {
        let requested = UserBuffer::get_total_length(data);
        let mut actual = 0;
        let mut user_buffers = UserBufferIterator::new(data);
        file.blocking_op(
            task,
            || {
                let mut pipe = self.pipe.lock();
                actual += match pipe.write(task, &mut user_buffers) {
                    Ok(chunk) => {
                        if chunk > 0 {
                            pipe.waiters.notify_events(FdEvents::POLLIN);
                        }
                        chunk
                    }
                    Err(errno) if errno == EPIPE && actual > 0 => return Ok(actual),
                    Err(errno) => return Err(errno),
                };
                if actual < requested {
                    return error!(EAGAIN);
                }
                Ok(actual)
            },
            FdEvents::POLLOUT,
            None,
        )
    }

    fn wait_async(
        &self,
        _file: &FileObject,
        waiter: &Arc<Waiter>,
        events: FdEvents,
        handler: EventHandler,
    ) {
        let mut pipe = self.pipe.lock();
        pipe.waiters.wait_async_mask(waiter, events.mask(), handler)
    }

    fn fcntl(
        &self,
        file: &FileObject,
        task: &Task,
        cmd: u32,
        arg: u64,
    ) -> Result<SyscallResult, Errno> {
        self.pipe.lock().fcntl(file, task, cmd, arg)
    }

    fn ioctl(
        &self,
        file: &FileObject,
        task: &Task,
        request: u32,
        in_addr: UserAddress,
        out_addr: UserAddress,
    ) -> Result<SyscallResult, Errno> {
        self.pipe.lock().ioctl(file, task, request, in_addr, out_addr)
    }
}

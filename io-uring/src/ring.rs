use {
    crate::slab::FixedSlab,
    io_uring::{
        cqueue, squeue,
        types::{SubmitArgs, Timespec},
        IoUring,
    },
    smallvec::{smallvec, SmallVec},
    std::{io, os::fd::RawFd, time::Duration},
};

/// An io_uring instance.
pub struct Ring<T, E: RingOp<T>> {
    ring: IoUring,
    entries: FixedSlab<E>,
    context: T,
}

impl<T, E: RingOp<T>> Ring<T, E> {
    /// Creates a new ring with the provided io_uring instance and context.
    ///
    /// The context `T` is a user defined value that will be passed to entries `E` once they
    /// complete. This value can be used to update state or perform additional actions as operations
    /// complete asynchronously.
    pub fn new(ring: IoUring, ctx: T) -> Self {
        Self {
            entries: FixedSlab::with_capacity(ring.params().cq_entries() as usize),
            ring,
            context: ctx,
        }
    }

    /// Returns a reference to the context value.
    pub fn context(&self) -> &T {
        &self.context
    }

    /// Returns a mutable reference to the context value.
    pub fn context_mut(&mut self) -> &mut T {
        &mut self.context
    }

    /// Registers in-memory fixed buffers for I/O with the kernel.
    ///
    /// # Safety
    ///
    /// Callers must ensure that the iov_base and iov_len values are valid and will be valid until
    /// buffers are unregistered or the ring destroyed, otherwise undefined behaviour may occur.
    ///
    /// See
    /// [Submitter::register_buffers](https://docs.rs/io-uring/0.6.3/io_uring/struct.Submitter.html#method.register_buffers).
    pub unsafe fn register_buffers(&self, iovecs: &[libc::iovec]) -> io::Result<()> {
        unsafe { self.ring.submitter().register_buffers(iovecs) }
    }

    /// Registers file descriptors as fixed for I/O with the kernel.
    ///
    /// Operations may then use `types::Fixed(index)` for index in `fds` to refer to the
    /// registered file descriptor.
    ///
    /// `-1` values can be used as slots for kernel managed fixed file descriptors (created by
    /// open operation).
    pub fn register_files(&self, fds: &[RawFd]) -> io::Result<()> {
        self.ring.submitter().register_files(fds)
    }

    /// Pushes an operation to the submission queue.
    ///
    /// Once completed, [RingOp::complete] will be called with the result.
    ///
    /// Note that the operation is not submitted to the kernel until [Ring::submit] is called. If
    /// the submission queue is full, submit will be called internally to make room for the new
    /// operation.
    ///
    /// See also [Ring::submit].
    pub fn push(&mut self, op: E) -> io::Result<()> {
        loop {
            self.process_completions()?;

            if !self.entries.is_full() {
                break;
            }
            // if the entries slab is full, we need to submit and poll
            // completions to make room
            self.submit_and_wait(1, None)?;
        }
        let key = self.entries.insert(op);
        let entry = self.entries.get_mut(key).unwrap().entry();
        let entry = entry.user_data(key as u64);
        // Safety: the entry is stored in self.entries and guaranteed to be valid for the lifetime
        // of the operation. E implementations must still ensure that the entry
        // remains valid until the last E::complete call.
        while unsafe { self.ring.submission().push(&entry) }.is_err() {
            self.submit()?;
            self.process_completions()?;
        }

        Ok(())
    }

    /// Submits all pending operations to the kernel.
    ///
    /// If the ring can't accept any more submissions because the completion
    /// queue is full, this will process completions and retry until the
    /// submissions are accepted.
    ///
    /// See also [Ring::process_completions].
    pub fn submit(&mut self) -> io::Result<()> {
        self.submit_and_wait(0, None).map(|_| ())
    }

    /// Submits all pending operations to the kernel and waits for completions.
    ///
    /// If no `timeout` is passed this will block until `want` completions are available. If a
    /// timeout is passed, this will block until `want` completions are available or the timeout is
    /// reached.
    ///
    /// Returns the number of completions received.
    pub fn submit_and_wait(&mut self, want: usize, timeout: Option<Duration>) -> io::Result<usize> {
        let mut args = SubmitArgs::new();
        let ts;
        if let Some(timeout) = timeout {
            ts = Timespec::from(timeout);
            args = args.timespec(&ts);
        }

        loop {
            match self.ring.submitter().submit_with_args(want, &args) {
                Ok(n) => return Ok(n),
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => return Ok(0),
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    // the completion queue is full, process completions and retry
                    self.process_completions()?;
                    continue;
                }
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => return Ok(0),
                Err(e) => return Err(e),
            }
        }
    }

    /// Processes completions from the kernel.
    ///
    /// This will process all completions currently available in the completion
    /// queue and invoke [RingOp::complete] for each completed operation.
    pub fn process_completions(&mut self) -> io::Result<()> {
        let mut completion = self.ring.completion();
        let mut new_entries = smallvec![];
        while let Some(cqe) = completion.next() {
            let completed_key = cqe.user_data() as usize;
            let entry = self.entries.get_mut(completed_key).unwrap();
            let result = entry.result(cqe.result());
            let mut comp_ctx = Completion {
                context: &mut self.context,
                new_entries,
            };
            let res = entry.complete(&mut comp_ctx, result);
            if !cqueue::more(cqe.flags()) {
                self.entries.remove(completed_key);
            }
            res?;
            new_entries = std::mem::take(&mut comp_ctx.new_entries);
            if !new_entries.is_empty() {
                completion.sync();
                drop(completion);
                for new_entry in new_entries.drain(..) {
                    self.push(new_entry)?;
                }
                completion = self.ring.completion();
            }
        }

        Ok(())
    }

    /// Drains the ring.
    ///
    /// This will submit all pending operations to the kernel and process all
    /// completions until the ring is empty.
    pub fn drain(&mut self) -> io::Result<()> {
        loop {
            self.process_completions()?;

            if self.entries.is_empty() {
                break;
            }

            match self.ring.submitter().submit_with_args(
                1,
                &SubmitArgs::new().timespec(&Timespec::from(Duration::from_millis(10))),
            ) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => {}
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }
}

/// Trait for operations that can be submitted to a [Ring].
pub trait RingOp<T> {
    fn entry(&mut self) -> squeue::Entry;
    fn complete(&mut self, ctx: &mut Completion<T, Self>, res: io::Result<i32>) -> io::Result<()>
    where
        Self: Sized;
    fn result(&self, res: i32) -> io::Result<i32> {
        if res < 0 {
            Err(io::Error::from_raw_os_error(res.wrapping_neg()))
        } else {
            Ok(res)
        }
    }
}

/// Context object passed to [RingOp::complete].
pub struct Completion<'a, T, E: RingOp<T>> {
    // Give new_entries a stack size of 2 to avoid heap allocations in the common case where only
    // one or two ops are queued from a completion handler.
    //
    // It's common to want to queue some extra work after a completion, for instance if you've
    // completed a read and want to close the file descriptor, or if you're doing chained operations
    // and want to push the next one. It's less common to want to queue many operations.
    new_entries: SmallVec<[E; 2]>,
    context: &'a mut T,
}

impl<T, E: RingOp<T>> Completion<'_, T, E> {
    /// Returns a reference to the context value stored in a [Ring].
    pub fn context(&self) -> &T {
        self.context
    }

    /// Returns a mutable reference to the context value stored in a [Ring].
    pub fn context_mut(&mut self) -> &mut T {
        self.context
    }

    /// Pushes an operation to the submission queue.
    ///
    /// This can be used to push new operations from within [RingOp::complete].
    ///
    /// See also [Ring::push].
    pub fn push(&mut self, op: E) {
        self.new_entries.push(op);
    }
}

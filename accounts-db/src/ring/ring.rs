use {
    io_uring::{
        squeue,
        types::{SubmitArgs, Timespec},
        CompletionQueue, IoUring, SubmissionQueue, Submitter,
    },
    slab::Slab,
    std::{io, time::Duration},
};

/// An io_uring instance.
pub struct Ring<T, E: RingOp<T>> {
    ring: IoUring,
    entries: Slab<E>,
    ctx: T,
}

impl<T, E: RingOp<T>> Ring<T, E> {
    /// Creates a new ring with the provided io_uring instance and context.
    ///
    /// The context `T`is a user defined value that will be passed to entries `E` once they
    /// complete.
    pub fn new(ring: IoUring, ctx: T) -> Self {
        Self {
            entries: Slab::with_capacity(ring.params().cq_entries() as usize),
            ring,
            ctx,
        }
    }

    /// Returns a reference to the context value.
    #[allow(dead_code)]
    pub fn ctx(&self) -> &T {
        &self.ctx
    }

    /// Returns a mutable reference to the context value.
    pub fn ctx_mut(&mut self) -> &mut T {
        &mut self.ctx
    }

    /// Registers in-memory fixed buffers for I/O with the kernel.
    ///
    /// See
    /// [Submitter::register_buffers](https://docs.rs/io-uring/0.6.3/io_uring/struct.Submitter.html#method.register_buffers).
    #[allow(dead_code)]
    pub unsafe fn register_buffers(&self, iovecs: &[libc::iovec]) -> io::Result<()> {
        self.ring.submitter().register_buffers(iovecs)
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
    pub unsafe fn push(&mut self, mut op: E) -> io::Result<()> {
        let entry = op.entry();
        let key = self.entries.insert(op);
        let entry = entry.user_data(key as u64);
        while self.ring.submission().push(&entry).is_err() {
            self.submit()?;
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
        loop {
            match self.ring.submit() {
                Ok(_) => {
                    self.ring.submission().sync();
                    return Ok(());
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    // the completion queue is full, process completions and retry
                    self.process_completions()?;
                }
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                    continue;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    /// Submits all pending operations to the kernel and waits for completions.
    ///
    /// If no `timeout` is passed  this will block until `want` completions are available. If a
    /// timeout is passed, this will block until `want` completions are available or the timeout is
    /// reached.
    ///
    /// Returns the number of completions received, or `None` if the timeout was reached.
    #[allow(dead_code)]
    pub fn submit_and_wait(
        &mut self,
        want: usize,
        timeout: Option<Duration>,
    ) -> io::Result<Option<usize>> {
        if let Some(timeout) = timeout {
            match self
                .ring
                .submitter()
                .submit_with_args(want, &SubmitArgs::new().timespec(&Timespec::from(timeout)))
            {
                Ok(n) => Ok(Some(n)),
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(None),
                Err(e) => Err(e),
            }
        } else {
            self.ring.submit_and_wait(want).map(Some)
        }
    }

    /// Processes completions from the kernel.
    ///
    /// This will process all completions currently available in the completion
    /// queue and invoke [RingOp::complete] for each completed operation.
    pub fn process_completions(&mut self) -> io::Result<()> {
        let (submitter, submission, mut completion) = self.ring.split();
        completion.sync();
        let mut submission = RingCtx {
            submission,
            completion,
            submitter,
            entries: &mut self.entries,
            ctx: &mut self.ctx,
        };
        submission.process_completions()
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
    fn complete(self, res: io::Result<i32>, sub_queue: &mut RingCtx<T, Self>) -> io::Result<()>
    where
        Self: Sized;
    fn result(&self, res: i32) -> io::Result<i32> {
        if res < 0 {
            Err(io::Error::from_raw_os_error(-res))
        } else {
            Ok(res)
        }
    }
}

/// Context object passed to [RingOp::complete].
pub struct RingCtx<'a, 'b, T, E: RingOp<T>> {
    submission: SubmissionQueue<'a>,
    completion: CompletionQueue<'a>,
    submitter: Submitter<'a>,
    entries: &'b mut Slab<E>,
    ctx: &'b mut T,
}

impl<'a, 'b, T, E: RingOp<T>> RingCtx<'a, 'b, T, E> {
    /// Returns a reference to the context value stored in a [Ring].
    #[allow(dead_code)]
    pub fn ctx(&self) -> &T {
        self.ctx
    }

    /// Returns a mutable reference to the context value stored in a [Ring].
    pub fn ctx_mut(&mut self) -> &mut T {
        self.ctx
    }

    /// Pushes an operation to the submission queue.
    ///
    /// This can be used to push new operations from within [RingOp::complete].
    ///
    /// See also [Ring::push].
    pub unsafe fn push(&mut self, mut op: E) -> io::Result<()> {
        let entry = op.entry();
        let key = self.entries.insert(op);
        let entry = entry.user_data(key as u64);
        while self.submission.push(&entry).is_err() {
            self.submit()?;
        }

        Ok(())
    }

    /// Submits all pending operations to the kernel.
    ///
    /// See also [Ring::submit].
    pub(crate) fn submit(&mut self) -> io::Result<()> {
        loop {
            match self.submitter.submit() {
                Ok(_) => {
                    self.submission.sync();
                    return Ok(());
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    self.process_completions()?;
                }
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                    continue;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    /// Processes completions from the kernel.
    ///
    /// See also [Ring::process_completions].
    pub fn process_completions(&mut self) -> io::Result<()> {
        loop {
            let Some(cqe) = self.completion.next() else {
                break;
            };
            let completed_key = cqe.user_data();
            let entry = self.entries.remove(completed_key as usize);
            let result = entry.result(cqe.result());
            entry.complete(result, self)?;
        }

        Ok(())
    }
}

use {
    agave_io_uring::{Completion, Ring, RingOp},
    io_uring::{opcode, squeue, types, IoUring},
    slab::Slab,
    std::{
        collections::VecDeque,
        ffi::{CString, OsString},
        fs, io,
        os::{
            fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd},
            unix::ffi::OsStringExt as _,
        },
        path::PathBuf,
    },
};

/// A fast directory remover using io_uring.
pub struct RingDirRemover {
    ring: Ring<State, Op>,
}

impl RingDirRemover {
    /// Creates a new remover.
    pub fn new() -> io::Result<RingDirRemover> {
        let ring = IoUring::builder().setup_sqpoll(1000).build(1024)?;
        // 12 is a good round number. Empirically removing unpacked snapshots, it appears to be high
        // enough to be fast but not too high to cause too much inode contention.
        ring.submitter().register_iowq_max_workers(&mut [12, 0])?;
        Ok(Self::with_ring(ring))
    }

    /// Creates a new remover, using the provided io_uring instance.
    fn with_ring(ring: IoUring) -> RingDirRemover {
        RingDirRemover {
            ring: Ring::new(ring, State { dirs: Slab::new() }),
        }
    }

    /// Removes a directory and all its contents.
    pub fn remove_dir_all(&mut self, path: impl Into<PathBuf>) -> io::Result<()> {
        self.remove(path, true)
    }

    /// Removes the contents of a directory, but not the directory itself.
    pub fn remove_dir_contents(&mut self, path: impl Into<PathBuf>) -> io::Result<()> {
        self.remove(path, false)
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn remove(&mut self, path: impl Into<PathBuf>, remove_root: bool) -> io::Result<()> {
        let path = path.into();

        let root_path = path.clone();
        let mut stack = VecDeque::new();

        // iterate over all directories recursively and unlink all entries
        stack.push_back(path);
        while let Some(dir_path) = stack.pop_front() {
            let file = std::fs::File::open(&dir_path)?;
            let fd = OwnedFd::from(file);
            let dir_fd = fd.as_raw_fd();
            let dir_key = self.ring.context_mut().dirs.insert(Directory::new(fd));

            for entry in fs::read_dir(&dir_path)? {
                let entry = entry?;
                let dir = &mut self.ring.context_mut().dirs[dir_key];

                let is_dir = entry.file_type()?.is_dir();
                if is_dir {
                    stack.push_back(entry.path());
                } else {
                    dir.unlink_started += 1;
                    let op = UnlinkOp::new(dir_key, dir_fd, entry.file_name());

                    self.ring.push(Op::Unlink(op))?;
                }
            }

            // mark the directory as having finished scanning
            let dir = &mut self.ring.context_mut().dirs[dir_key];
            dir.set_finished_scanning();

            // If the directory was empty or all unlink ops completed before we called
            // dir.set_finished_scanning(), we must close the dir fd now. Otherwise it gets
            // closed by the last UnlinkOp::complete() call.
            if dir.scanned_and_unlinked() {
                let fd = dir.fd.take();
                if let Some(fd) = fd {
                    self.ring
                        .push(Op::Close(CloseOp::new(dir_key, fd.into_raw_fd())))?;
                }
            }
        }

        // wait for all ring operations to complete
        self.ring.drain()?;

        if remove_root {
            fs::remove_dir_all(root_path)?;
        } else {
            for dir in fs::read_dir(root_path)? {
                let dir = dir?;
                if dir.file_type()?.is_dir() {
                    fs::remove_dir_all(dir.path())?;
                }
            }
        }

        Ok(())
    }
}

pub struct State {
    dirs: Slab<Directory>,
}

pub struct Directory {
    // File descriptor of the directory.
    fd: Option<OwnedFd>,
    // Keep track of how many ops are in flight
    unlink_started: usize,
    unlink_completed: usize,
    // Whether we have finished scanning the directory
    finished_scanning: bool,
}

impl Directory {
    pub fn new(fd: OwnedFd) -> Directory {
        Directory {
            fd: Some(fd),
            unlink_started: 0,
            unlink_completed: 0,
            finished_scanning: false,
        }
    }

    fn set_finished_scanning(&mut self) {
        self.finished_scanning = true;
    }

    // Returns true once we have finished scanning the directory and all unlink
    // ops have completed.
    //
    // This is used to determine whether we can close the directory fd.
    fn scanned_and_unlinked(&self) -> bool {
        self.finished_scanning && self.unlink_started == self.unlink_completed
    }
}

// Op used to unlink a file within a directory.
struct UnlinkOp {
    // directory key within state.dirs
    dir_key: usize,
    // file descriptor of the parent dir and path of the file to unlink
    dir_fd: RawFd,
    path: CString,
}

impl std::fmt::Debug for UnlinkOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnlinkOp")
            .field("dir_key", &self.dir_key)
            .field("dir_fd", &self.dir_fd)
            // Safety: the path is guaranteed to be null terminated
            .field("path", &self.path.as_c_str())
            .finish()
    }
}

impl UnlinkOp {
    fn new(dir_key: usize, dir_fd: RawFd, file_name: OsString) -> UnlinkOp {
        UnlinkOp {
            dir_key,
            dir_fd,
            path: CString::new(file_name.into_vec()).unwrap(),
        }
    }

    fn entry(&mut self) -> squeue::Entry {
        opcode::UnlinkAt::new(types::Fd(self.dir_fd), self.path.as_ptr() as _).build()
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn complete(
        &mut self,
        comp: &mut Completion<State, Op>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        let _ = res?;

        let dir = &mut comp.context_mut().dirs[self.dir_key];
        dir.unlink_completed += 1;
        if dir.scanned_and_unlinked() {
            // This was the last file to be removed, we can now close the parent
            // directory fd.
            //
            // Safety: the entry doesn't hold any pointers
            if let Some(fd) = dir.fd.take() {
                comp.push(Op::Close(CloseOp::new(self.dir_key, fd.into_raw_fd())));
            }
        }

        Ok(())
    }
}

// Op used to close the directory file descriptor once we have finished scanning and unlinking all
// entries.
#[derive(Debug)]
struct CloseOp {
    dir_key: usize,
    fd: RawFd,
}

impl CloseOp {
    fn new(dir_key: usize, fd: RawFd) -> Self {
        Self { dir_key, fd }
    }

    fn entry(&mut self) -> squeue::Entry {
        opcode::Close::new(types::Fd(self.fd)).build()
    }

    fn complete(
        &mut self,
        comp: &mut Completion<State, Op>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        let _ = res?;
        let _ = comp.context_mut().dirs.remove(self.dir_key);
        Ok(())
    }
}

#[derive(Debug)]
enum Op {
    Unlink(UnlinkOp),
    Close(CloseOp),
}

impl RingOp<State> for Op {
    fn entry(&mut self) -> squeue::Entry {
        match self {
            Op::Unlink(op) => op.entry(),
            Op::Close(op) => op.entry(),
        }
    }

    fn complete(
        &mut self,
        comp: &mut Completion<State, Self>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        match self {
            Op::Unlink(op) => op.complete(comp, res),
            Op::Close(op) => op.complete(comp, res),
        }
    }
}

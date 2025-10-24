use {std::io, thiserror::Error};

#[derive(Error, Debug)]
pub enum ResourceLimitError {
    #[error(
        "unable to increase the nofile limit to {desired} from {current}; setrlimit() error: \
         {error}"
    )]
    Nofile {
        desired: u64,
        current: u64,
        error: libc::c_int,
    },
}

#[cfg(not(unix))]
pub fn adjust_nofile_limit(_enforce_nofile_limit: bool) -> Result<(), ResourceLimitError> {
    Ok(())
}

#[cfg(unix)]
pub fn adjust_nofile_limit(enforce_nofile_limit: bool) -> Result<(), ResourceLimitError> {
    // AccountsDB and RocksDB both may have many files open so bump the limit
    // to ensure each database will be able to function properly
    //
    // This should be kept in sync with published validator instructions:
    // https://docs.anza.xyz/operations/guides/validator-start#system-tuning
    let desired_nofile = 1_000_000;

    fn get_nofile() -> libc::rlimit {
        let mut nofile = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut nofile) } != 0 {
            warn!("getrlimit(RLIMIT_NOFILE) failed");
        }
        nofile
    }

    let mut nofile = get_nofile();
    let current = nofile.rlim_cur;
    if current < desired_nofile {
        nofile.rlim_cur = desired_nofile;
        let return_value = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) };
        if return_value != 0 {
            let error = ResourceLimitError::Nofile {
                desired: desired_nofile,
                current,
                error: return_value,
            };

            if cfg!(target_os = "macos") {
                error!(
                    "{error}. On macOS you may need to run |sudo launchctl limit maxfiles \
                     {desired_nofile} {desired_nofile}| first",
                );
            } else {
                error!("{error}");
            };

            if enforce_nofile_limit {
                return Err(error);
            }
        }

        nofile = get_nofile();
    }
    info!("Maximum open file descriptors: {}", nofile.rlim_cur);
    Ok(())
}

/// Check kernel memory lock limit and increase it if necessary.
///
/// Returns `Err` when current limit is below `min_required` and cannot be increased.
#[cfg(target_os = "linux")]
fn adjust_ulimit_memlock(min_required: usize) -> io::Result<()> {
    fn get_memlock() -> libc::rlimit {
        let mut memlock = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut memlock) } != 0 {
            log::warn!("getrlimit(RLIMIT_MEMLOCK) failed");
        }
        memlock
    }

    let mut memlock = get_memlock();
    let current = memlock.rlim_cur as usize;
    if current < min_required {
        memlock.rlim_cur = min_required as u64;
        memlock.rlim_max = min_required as u64;
        if unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &memlock) } != 0 {
            log::error!(
                "Unable to increase the maximum memory lock limit to {min_required} from {current}"
            );

            if cfg!(target_os = "macos") {
                log::error!(
                    "On mac OS you may need to run |sudo launchctl limit memlock {min_required} \
                     {min_required}| first"
                );
            }
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "unable to set memory lock limit",
            ));
        }

        memlock = get_memlock();
        log::info!("Bumped maximum memory lock limit: {}", memlock.rlim_cur);
    }
    Ok(())
}

pub fn validate_memlock_limit_for_disk_io(required_size: usize) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // memory locked requirement is only necessary on linux where io_uring is used
        adjust_ulimit_memlock(required_size)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = required_size;
        Ok(())
    }
}

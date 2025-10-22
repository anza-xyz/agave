use thiserror::Error;

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

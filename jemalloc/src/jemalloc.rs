#[cfg(feature = "metrics")]
use {
    log::{Level, log_enabled, warn},
    solana_metrics::{datapoint_debug, datapoint_info},
    std::mem::{self, MaybeUninit},
};
use {
    std::{ffi::CString, fmt, os::raw::c_uint, ptr},
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum Error {
    #[error("jemalloc mallctl {name} failed: {error}")]
    Mallctl {
        name: String,
        error: jemalloc_ctl::Error,
    },

    #[error("jemalloc mallctl {name} failed with errno code {code}")]
    MallctlCommand { name: String, code: i32 },

    #[error("jemalloc decay duration {millis} ms does not fit ssize_t")]
    DecayMillisOutOfRange { millis: u64 },

    #[error("jemalloc returned invalid decay duration {millis} ms")]
    InvalidDecayMillis { millis: isize },

    #[error("jemalloc page byte count overflows usize: {pages} * {page_size}")]
    PageByteCountOverflow { pages: usize, page_size: usize },
}

// Jemalloc uses a one-byte C99 `_Bool` on supported targets.
type JemallocBool = u8;
const JEMALLOC_FALSE: JemallocBool = 0;
// jemalloc's mallctl APIs return POSIX errno values. ENOENT means the probed
// indexed node is outside the mallctl namespace.
#[cfg(feature = "metrics")]
const MALLCTL_ENOENT: i32 = 2;

#[derive(Debug, Clone, Copy, Default)]
pub struct Jemalloc;

impl Jemalloc {
    pub(crate) fn create_arena() -> Result<Arena, Error> {
        // SAFETY: `arenas.create` returns an unsigned arena index.
        unsafe { read_raw::<c_uint>("arenas.create") }.map(|id| Arena {
            id: ArenaId::from_raw(id),
        })
    }

    pub fn current_thread_arena() -> Result<ArenaId, Error> {
        // SAFETY: `thread.arena` reads an unsigned arena index.
        unsafe { read_raw::<c_uint>("thread.arena") }.map(ArenaId::from_raw)
    }

    pub fn disable_current_thread_tcache() -> Result<(), Error> {
        // SAFETY: `JemallocBool` matches jemalloc's C `bool` representation.
        unsafe { write_raw::<JemallocBool>("thread.tcache.enabled", JEMALLOC_FALSE) }
    }

    fn bind_current_thread_permanently(arena: ArenaId) -> Result<ArenaId, Error> {
        // SAFETY: `thread.arena` reads and writes an unsigned arena index.
        unsafe { update_raw::<c_uint>("thread.arena", arena.as_raw()) }.map(ArenaId::from_raw)
    }

    pub fn advance_epoch() -> Result<(), Error> {
        jemalloc_ctl::epoch::advance()
            .map(|_| ())
            .map_err(|error| Error::Mallctl {
                name: "epoch".to_string(),
                error,
            })
    }

    fn page_size() -> Result<usize, Error> {
        // SAFETY: `arenas.page` reads a `size_t`.
        unsafe { read_raw::<usize>("arenas.page") }
    }
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArenaId(c_uint);

impl ArenaId {
    pub const fn from_raw(raw: c_uint) -> Self {
        Self(raw)
    }

    pub const fn as_raw(self) -> c_uint {
        self.0
    }
}

impl fmt::Display for ArenaId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arena {
    id: ArenaId,
}

#[derive(Debug, Clone, Copy)]
enum ArenaCommand {
    Purge,
}

impl ArenaCommand {
    const fn name(self) -> &'static str {
        match self {
            Self::Purge => "purge",
        }
    }
}

impl Arena {
    pub const fn id(self) -> ArenaId {
        self.id
    }

    pub fn bind_current_thread_permanently(self) -> Result<ArenaId, Error> {
        Jemalloc::bind_current_thread_permanently(self.id)
    }

    pub(crate) fn set_dirty_decay(self, decay: Decay) -> Result<(), Error> {
        // SAFETY: `arena.<i>.dirty_decay_ms` writes an `ssize_t`.
        unsafe { write_raw::<isize>(&self.mallctl_name("dirty_decay_ms"), decay.to_mallctl()?) }
    }

    pub(crate) fn set_muzzy_decay(self, decay: Decay) -> Result<(), Error> {
        // SAFETY: `arena.<i>.muzzy_decay_ms` writes an `ssize_t`.
        unsafe { write_raw::<isize>(&self.mallctl_name("muzzy_decay_ms"), decay.to_mallctl()?) }
    }

    pub(crate) fn set_oversize_threshold(self, bytes: usize) -> Result<(), Error> {
        // SAFETY: `arena.<i>.oversize_threshold` writes a `size_t`.
        unsafe { write_raw::<usize>(&self.mallctl_name("oversize_threshold"), bytes) }
    }

    pub(crate) fn set_retain_grow_limit(self, bytes: usize) -> Result<(), Error> {
        // SAFETY: `arena.<i>.retain_grow_limit` writes a `size_t`.
        unsafe { write_raw::<usize>(&self.mallctl_name("retain_grow_limit"), bytes) }
    }

    pub fn purge(self) -> Result<(), Error> {
        self.run_command(ArenaCommand::Purge)
    }

    pub fn stats(self) -> Result<ArenaStats, Error> {
        arena_stats(self.id)
    }

    #[cfg(feature = "metrics")]
    fn extent_stats(self) -> Result<Vec<ArenaExtentStats>, Error> {
        arena_extent_stats(self.id)
    }

    #[cfg(feature = "metrics")]
    pub fn report_stats(self, metric_name: &'static str, stats: &ArenaStats) {
        datapoint_info!(
            metric_name,
            "kind" => "arena",
            "arena_id" => self.id.to_string(),
            ("mapped_bytes", stats.mapped as i64, i64),
            ("retained_bytes", stats.retained as i64, i64),
            ("active_bytes", stats.active as i64, i64),
            ("dirty_bytes", stats.dirty as i64, i64),
            ("muzzy_bytes", stats.muzzy as i64, i64),
            ("active_pages", stats.active_pages as i64, i64),
            ("dirty_pages", stats.dirty_pages as i64, i64),
            ("muzzy_pages", stats.muzzy_pages as i64, i64),
            ("dirty_decay_ms", decay_as_millis(stats.dirty_decay), i64),
            ("muzzy_decay_ms", decay_as_millis(stats.muzzy_decay), i64),
            ("dirty_purge_sweeps", stats.dirty_purges.sweeps as i64, i64),
            (
                "dirty_purge_madvise",
                stats.dirty_purges.madvise as i64,
                i64
            ),
            (
                "dirty_purged_pages",
                stats.dirty_purges.purged_pages as i64,
                i64
            ),
            ("muzzy_purge_sweeps", stats.muzzy_purges.sweeps as i64, i64),
            (
                "muzzy_purge_madvise",
                stats.muzzy_purges.madvise as i64,
                i64
            ),
            (
                "muzzy_purged_pages",
                stats.muzzy_purges.purged_pages as i64,
                i64
            ),
        );

        if !log_enabled!(Level::Debug) {
            return;
        }

        let extent_stats = match self.extent_stats() {
            Ok(stats) => stats,
            Err(error) => {
                warn!(
                    "failed to read jemalloc arena extent stats; arena_id: {}; error: {error}",
                    self.id
                );
                return;
            }
        };

        for stats in extent_stats
            .into_iter()
            .filter(|stats| stats.total_bytes() > 0)
        {
            datapoint_debug!(
                metric_name,
                "kind" => "extent",
                "arena_id" => self.id.to_string(),
                "extent_index" => stats.extent_index.to_string(),
                ("ndirty", stats.ndirty as i64, i64),
                ("nmuzzy", stats.nmuzzy as i64, i64),
                ("nretained", stats.nretained as i64, i64),
                ("dirty_bytes", stats.dirty_bytes as i64, i64),
                ("muzzy_bytes", stats.muzzy_bytes as i64, i64),
                ("retained_bytes", stats.retained_bytes as i64, i64),
                ("total_extents", stats.total_extents() as i64, i64),
                ("total_bytes", stats.total_bytes() as i64, i64),
            );
        }
    }

    fn mallctl_name(self, name: &str) -> String {
        format!("arena.{}.{}", self.id.as_raw(), name)
    }

    fn run_command(self, command: ArenaCommand) -> Result<(), Error> {
        let name = self.mallctl_name(command.name());
        let mallctl_name = mallctl_name(&name);
        // SAFETY: every command in the closed set takes no old or new value and cannot invalidate
        // live allocations or thread state.
        let code = unsafe {
            jemalloc_sys::mallctl(
                mallctl_name.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
            )
        };

        if code == 0 {
            Ok(())
        } else {
            Err(Error::MallctlCommand { name, code })
        }
    }
}

#[cfg(feature = "metrics")]
fn decay_as_millis(decay: Decay) -> i64 {
    match decay {
        Decay::Never => -1,
        Decay::Immediate => 0,
        Decay::Millis(millis) => i64::try_from(millis).unwrap_or(i64::MAX),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decay {
    Never,
    Immediate,
    Millis(u64),
}

impl Decay {
    fn to_mallctl(self) -> Result<isize, Error> {
        match self {
            Self::Never => Ok(-1),
            Self::Immediate => Ok(0),
            Self::Millis(millis) => {
                isize::try_from(millis).map_err(|_| Error::DecayMillisOutOfRange { millis })
            }
        }
    }

    fn from_mallctl(millis: isize) -> Result<Self, Error> {
        match millis {
            -1 => Ok(Self::Never),
            0 => Ok(Self::Immediate),
            millis if millis > 0 => u64::try_from(millis)
                .map(Self::Millis)
                .map_err(|_| Error::InvalidDecayMillis { millis }),
            millis => Err(Error::InvalidDecayMillis { millis }),
        }
    }
}

impl fmt::Display for Decay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Never => formatter.write_str("never"),
            Self::Immediate => formatter.write_str("immediate"),
            Self::Millis(millis) => write!(formatter, "{millis} ms"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaStats {
    pub mapped: usize,
    pub retained: usize,
    pub active: usize,
    pub dirty: usize,
    pub muzzy: usize,
    pub active_pages: usize,
    pub dirty_pages: usize,
    pub muzzy_pages: usize,
    pub dirty_decay: Decay,
    pub muzzy_decay: Decay,
    pub dirty_purges: PurgeStats,
    pub muzzy_purges: PurgeStats,
}

#[cfg(feature = "metrics")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArenaExtentStats {
    extent_index: usize,
    ndirty: usize,
    nmuzzy: usize,
    nretained: usize,
    dirty_bytes: usize,
    muzzy_bytes: usize,
    retained_bytes: usize,
}

#[cfg(feature = "metrics")]
impl ArenaExtentStats {
    pub const fn total_extents(self) -> usize {
        self.ndirty
            .saturating_add(self.nmuzzy)
            .saturating_add(self.nretained)
    }

    pub const fn total_bytes(self) -> usize {
        self.dirty_bytes
            .saturating_add(self.muzzy_bytes)
            .saturating_add(self.retained_bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeStats {
    pub sweeps: u64,
    pub madvise: u64,
    pub purged_pages: u64,
}

fn arena_stats(arena_id: ArenaId) -> Result<ArenaStats, Error> {
    let page_size = Jemalloc::page_size()?;
    // SAFETY: the page counts, mapped bytes, and retained bytes are `size_t`; decay values are
    // `ssize_t`; and purge counters are `uint64_t` in jemalloc's mallctl API.
    unsafe {
        let active_pages = read_arena_stat::<usize>(arena_id, "pactive")?;
        let dirty_pages = read_arena_stat::<usize>(arena_id, "pdirty")?;
        let muzzy_pages = read_arena_stat::<usize>(arena_id, "pmuzzy")?;

        Ok(ArenaStats {
            mapped: read_arena_stat::<usize>(arena_id, "mapped")?,
            retained: read_arena_stat::<usize>(arena_id, "retained")?,
            active: pages_to_bytes(active_pages, page_size)?,
            dirty: pages_to_bytes(dirty_pages, page_size)?,
            muzzy: pages_to_bytes(muzzy_pages, page_size)?,
            active_pages,
            dirty_pages,
            muzzy_pages,
            dirty_decay: Decay::from_mallctl(read_raw::<isize>(&format!(
                "arena.{}.dirty_decay_ms",
                arena_id.as_raw()
            ))?)?,
            muzzy_decay: Decay::from_mallctl(read_raw::<isize>(&format!(
                "arena.{}.muzzy_decay_ms",
                arena_id.as_raw()
            ))?)?,
            dirty_purges: PurgeStats {
                sweeps: read_arena_stat::<u64>(arena_id, "dirty_npurge")?,
                madvise: read_arena_stat::<u64>(arena_id, "dirty_nmadvise")?,
                purged_pages: read_arena_stat::<u64>(arena_id, "dirty_purged")?,
            },
            muzzy_purges: PurgeStats {
                sweeps: read_arena_stat::<u64>(arena_id, "muzzy_npurge")?,
                madvise: read_arena_stat::<u64>(arena_id, "muzzy_nmadvise")?,
                purged_pages: read_arena_stat::<u64>(arena_id, "muzzy_purged")?,
            },
        })
    }
}

#[cfg(feature = "metrics")]
fn arena_extent_stats(arena_id: ArenaId) -> Result<Vec<ArenaExtentStats>, Error> {
    let mut extent_stats = Vec::new();

    for extent_index in 0.. {
        let ndirty_name = format!("extents.{extent_index}.ndirty");
        // SAFETY: all `stats.arenas.<i>.extents.<j>` values read here are `size_t`.
        let Some(ndirty) = unsafe { read_arena_stat_optional::<usize>(arena_id, &ndirty_name) }?
        else {
            break;
        };

        // SAFETY: all `stats.arenas.<i>.extents.<j>` values read here are `size_t`.
        unsafe {
            extent_stats.push(ArenaExtentStats {
                extent_index,
                ndirty,
                nmuzzy: read_arena_stat::<usize>(
                    arena_id,
                    &format!("extents.{extent_index}.nmuzzy"),
                )?,
                nretained: read_arena_stat::<usize>(
                    arena_id,
                    &format!("extents.{extent_index}.nretained"),
                )?,
                dirty_bytes: read_arena_stat::<usize>(
                    arena_id,
                    &format!("extents.{extent_index}.dirty_bytes"),
                )?,
                muzzy_bytes: read_arena_stat::<usize>(
                    arena_id,
                    &format!("extents.{extent_index}.muzzy_bytes"),
                )?,
                retained_bytes: read_arena_stat::<usize>(
                    arena_id,
                    &format!("extents.{extent_index}.retained_bytes"),
                )?,
            });
        }
    }

    Ok(extent_stats)
}

fn pages_to_bytes(pages: usize, page_size: usize) -> Result<usize, Error> {
    pages
        .checked_mul(page_size)
        .ok_or(Error::PageByteCountOverflow { pages, page_size })
}

/// # Safety
///
/// `stat_name` must contain no nul bytes, and `T` must be the exact type of the named jemalloc
/// arena statistic.
unsafe fn read_arena_stat<T: Copy>(arena_id: ArenaId, stat_name: &str) -> Result<T, Error> {
    // SAFETY: the caller guarantees that `T` matches the named statistic.
    unsafe { read_raw::<T>(&format!("stats.arenas.{}.{stat_name}", arena_id.as_raw())) }
}

#[cfg(feature = "metrics")]
/// # Safety
///
/// `stat_name` must contain no nul bytes, and `T` must be the exact type of the named jemalloc
/// arena statistic.
unsafe fn read_arena_stat_optional<T: Copy>(
    arena_id: ArenaId,
    stat_name: &str,
) -> Result<Option<T>, Error> {
    // SAFETY: the caller guarantees that `T` matches the named statistic.
    unsafe { read_raw_optional::<T>(&format!("stats.arenas.{}.{stat_name}", arena_id.as_raw())) }
}

/// # Safety
///
/// `name` must contain no nul bytes. `T` must have the size and alignment of the named readable
/// jemalloc control, and every value the control can write must be valid for `T`.
unsafe fn read_raw<T: Copy>(name: &str) -> Result<T, Error> {
    let mallctl_name = mallctl_name(name);

    // SAFETY: the caller guarantees that `T` satisfies the control's representation requirements.
    unsafe { jemalloc_ctl::raw::read::<T>(mallctl_name.as_bytes_with_nul()) }.map_err(|error| {
        Error::Mallctl {
            name: name.to_string(),
            error,
        }
    })
}

#[cfg(feature = "metrics")]
/// # Safety
///
/// `name` must contain no nul bytes. `T` must have the size and alignment of the named readable
/// jemalloc control, and every value the control can write must be valid for `T`.
unsafe fn read_raw_optional<T: Copy>(name: &str) -> Result<Option<T>, Error> {
    let mallctl_name = mallctl_name(name);
    let mut value = MaybeUninit::<T>::uninit();
    let mut len = mem::size_of::<T>();

    // SAFETY: the caller guarantees that `T` satisfies the control's representation requirements.
    // `mallctl_name` is nul-terminated, and the old value buffer is valid for `len` bytes.
    let ret = unsafe {
        jemalloc_sys::mallctl(
            mallctl_name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    };

    match ret {
        0 => {
            assert_eq!(len, mem::size_of::<T>());
            // SAFETY: jemalloc returned success and initialized exactly `size_of::<T>()` bytes.
            Ok(Some(unsafe { value.assume_init() }))
        }
        MALLCTL_ENOENT => Ok(None),
        code => Err(Error::MallctlCommand {
            name: name.to_string(),
            code,
        }),
    }
}

/// # Safety
///
/// `name` must contain no nul bytes. `T` must have the size and alignment of the named writable
/// jemalloc control, and `value` must have a representation accepted by that control.
unsafe fn write_raw<T: Copy>(name: &str, value: T) -> Result<(), Error> {
    let mallctl_name = mallctl_name(name);

    // SAFETY: the caller guarantees that `T` satisfies the control's representation requirements.
    unsafe { jemalloc_ctl::raw::write::<T>(mallctl_name.as_bytes_with_nul(), value) }.map_err(
        |error| Error::Mallctl {
            name: name.to_string(),
            error,
        },
    )
}

/// # Safety
///
/// `name` must contain no nul bytes. `T` must have the size and alignment of the named control,
/// every value the control can write must be valid for `T`, and `value` must have a representation
/// accepted by that control.
unsafe fn update_raw<T: Copy>(name: &str, value: T) -> Result<T, Error> {
    let mallctl_name = mallctl_name(name);

    // SAFETY: the caller guarantees that `T` satisfies the control's representation requirements.
    unsafe { jemalloc_ctl::raw::update::<T>(mallctl_name.as_bytes_with_nul(), value) }.map_err(
        |error| Error::Mallctl {
            name: name.to_string(),
            error,
        },
    )
}

fn mallctl_name(name: &str) -> CString {
    CString::new(name).expect("mallctl name must not contain nul bytes")
}

#[cfg(test)]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[cfg(test)]
mod tests {
    use crate::jemalloc::ArenaId;

    #[test]
    fn preserves_raw_arena_id() {
        let arena_id = ArenaId::from_raw(42);

        assert_eq!(arena_id.as_raw(), 42);
        assert_eq!(arena_id.to_string(), "42");
    }
}

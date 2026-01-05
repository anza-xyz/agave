use std::{
    fs::{File, OpenOptions},
    io,
    path::PathBuf,
};

/// Open `File` coupled with its filesystem location and most useful information
///
/// The attached context for the `File` is kept minimal to make it easy to construct
/// without unnecessary kernel queries, but allowing users to:
/// * associate the file received in callbacks to the request (by its path)
/// * get the file's most useful metadata information
#[derive(Debug)]
pub struct FileInfo {
    pub file: File,
    pub path: PathBuf,
    pub size: u64,
}

impl FileInfo {
    /// Create new instance by opening a file from a given `path` and reading its metadata
    pub fn new_from_path(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let file = File::open(&path)?;
        Self::new_from_path_and_file(path, file)
    }

    /// Create new instance by opening file with R/RW mode from a given `path` and reading its metadata
    ///
    /// `is_writable` indicates whether the file should be opened in write mode. Note that file needs
    /// to exist, so even if `is_writable` is true, the file will not be created when missing.
    pub fn new_from_path_writable(path: impl Into<PathBuf>, is_writable: bool) -> io::Result<Self> {
        let path = path.into();
        let file = OpenOptions::new()
            .read(true)
            .create(false)
            .write(is_writable)
            .open(&path)?;
        Self::new_from_path_and_file(path, file)
    }

    /// Create new instance by using already open `file` and only reading its metadata
    pub fn new_from_path_and_file(path: impl Into<PathBuf>, file: File) -> io::Result<Self> {
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.into(),
            size,
            file,
        })
    }
}

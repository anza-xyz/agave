use {
    crate::{
        config::{Config, ExplicitRelease},
        stop_process::stop_process,
    },
    console::{Emoji, style},
    crossbeam_channel::unbounded,
    indicatif::{ProgressBar, ProgressStyle},
    serde::{Deserialize, Serialize},
    solana_hash::Hash,
    solana_sha256_hasher::Hasher,
    std::{
        fs::{self, File},
        io::{self, BufReader, Read},
        path::{Path, PathBuf},
        time::{Duration, Instant},
    },
    tempfile::TempDir,
    url::Url,
};

#[derive(Deserialize, Debug)]
pub struct ReleaseVersion {
    pub target: String,
    pub commit: String,
    channel: String,
}

static TRUCK: Emoji = Emoji("🚚 ", "");
static LOOKING_GLASS: Emoji = Emoji("🔍 ", "");
static BULLET: Emoji = Emoji("• ", "* ");
static SPARKLE: Emoji = Emoji("✨ ", "");
static WRAPPED_PRESENT: Emoji = Emoji("🎁 ", "");
static PACKAGE: Emoji = Emoji("📦 ", "");
static RECYCLING: Emoji = Emoji("♻️  ", "");

/// Creates a new process bar for processing that will take an unknown amount of time
fn new_spinner_progress_bar() -> ProgressBar {
    let progress_bar = ProgressBar::new(42);
    progress_bar.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {wide_msg}")
            .expect("ProgresStyle::template direct input to be correct"),
    );
    progress_bar.enable_steady_tick(Duration::from_millis(100));
    progress_bar
}

/// Pretty print a "name value"
fn println_name_value(name: &str, value: &str) {
    println!("{} {}", style(name).bold(), value);
}

/// Downloads a file at `url` to a temporary location.  If `expected_sha256` is
/// Some(_), produce an error if the SHA256 of the file contents doesn't match.
///
/// Returns a tuple consisting of:
/// * TempDir - drop this value to clean up the temporary location
/// * PathBuf - path to the downloaded file (within `TempDir`)
/// * String  - SHA256 of the release
///
fn download_to_temp(
    url: &str,
    expected_sha256: Option<&Hash>,
) -> Result<(TempDir, PathBuf, Hash), Box<dyn std::error::Error>> {
    fn sha256_file_digest<P: AsRef<Path>>(path: P) -> Result<Hash, Box<dyn std::error::Error>> {
        let input = File::open(path)?;
        let mut reader = BufReader::new(input);
        let mut hasher = Hasher::default();

        let mut buffer = [0; 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.hash(&buffer[..count]);
        }
        Ok(hasher.result())
    }

    let url = Url::parse(url).map_err(|err| format!("Unable to parse {url}: {err}"))?;

    let temp_dir = TempDir::new()?;
    let temp_file = temp_dir.path().join("download");

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(None)
        .build()?;

    let progress_bar = new_spinner_progress_bar();
    progress_bar.set_message(format!("{TRUCK}Downloading..."));

    let response = client.get(url.as_str()).send()?;
    let download_size = {
        response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|content_length| content_length.to_str().ok())
            .and_then(|content_length| content_length.parse().ok())
            .unwrap_or(0)
    };

    progress_bar.set_length(download_size);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green}{wide_msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
            )
            .expect("ProgresStyle::template direct input to be correct")
            .progress_chars("=> "),
    );
    progress_bar.set_message(format!("{TRUCK}Downloading"));

    struct DownloadProgress<R> {
        progress_bar: ProgressBar,
        response: R,
    }

    impl<R: Read> Read for DownloadProgress<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.response.read(buf).inspect(|&n| {
                self.progress_bar.inc(n as u64);
            })
        }
    }

    let mut source = DownloadProgress {
        progress_bar,
        response,
    };

    let mut file = File::create(&temp_file)?;
    std::io::copy(&mut source, &mut file)?;

    let temp_file_sha256 = sha256_file_digest(&temp_file)
        .map_err(|err| format!("Unable to hash {temp_file:?}: {err}"))?;

    if expected_sha256.is_some() && expected_sha256 != Some(&temp_file_sha256) {
        return Err(io::Error::other("Incorrect hash").into());
    }

    source.progress_bar.finish_and_clear();
    Ok((temp_dir, temp_file, temp_file_sha256))
}

/// Extracts the release archive into the specified directory
fn extract_release_archive(
    archive: &Path,
    extract_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use {bzip2::bufread::BzDecoder, tar::Archive};

    let progress_bar = new_spinner_progress_bar();
    progress_bar.set_message(format!("{PACKAGE}Extracting..."));

    if extract_dir.exists() {
        let _ = fs::remove_dir_all(extract_dir);
    }

    let tmp_extract_dir = extract_dir.with_file_name("tmp-extract");
    if tmp_extract_dir.exists() {
        let _ = fs::remove_dir_all(&tmp_extract_dir);
    }
    fs::create_dir_all(&tmp_extract_dir)?;

    let tar_bz2 = File::open(archive)?;
    let tar = BzDecoder::new(BufReader::new(tar_bz2));
    let mut release = Archive::new(tar);
    release.unpack(&tmp_extract_dir)?;

    fs::rename(&tmp_extract_dir, extract_dir)?;

    progress_bar.finish_and_clear();
    Ok(())
}

fn load_release_version(version_yml: &Path) -> Result<ReleaseVersion, String> {
    let file = File::open(version_yml)
        .map_err(|err| format!("Unable to open {version_yml:?}: {err:?}"))?;
    let version: ReleaseVersion = serde_yaml::from_reader(file)
        .map_err(|err| format!("Unable to parse {version_yml:?}: {err:?}"))?;
    Ok(version)
}

/// Reads the supported TARGET triple for the given release
fn load_release_target(release_dir: &Path) -> Result<String, String> {
    let mut version_yml = PathBuf::from(release_dir);
    version_yml.push("solana-release");
    version_yml.push("version.yml");

    let version = load_release_version(&version_yml)?;
    Ok(version.target)
}

/// Bug the user if active_release_bin_dir is not in their PATH
fn check_env_path_for_bin_dir(config: &Config) {
    use std::env;

    let bin_dir = config
        .active_release_bin_dir()
        .canonicalize()
        .unwrap_or_default();
    let found = match env::var_os("PATH") {
        Some(paths) => env::split_paths(&paths).any(|path| {
            if let Ok(path) = path.canonicalize()
                && path == bin_dir
            {
                return true;
            }
            false
        }),
        None => false,
    };

    if !found {
        println!(
            "\nPlease update your PATH environment variable to include the solana programs:\n    \
             PATH=\"{}:$PATH\"\n",
            config.active_release_bin_dir().to_str().unwrap()
        );
    }
}

/// Encodes a UTF-8 string as a null-terminated UCS-2 string in bytes
#[cfg(windows)]
pub fn string_to_winreg_bytes(s: &str) -> Vec<u8> {
    use std::{ffi::OsString, os::windows::ffi::OsStrExt};
    let v: Vec<_> = OsString::from(format!("{s}\x00")).encode_wide().collect();
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2).to_vec() }
}

// This is used to decode the value of HKCU\Environment\PATH. If that
// key is not Unicode (or not REG_SZ | REG_EXPAND_SZ) then this
// returns null.  The winreg library itself does a lossy unicode
// conversion.
#[cfg(windows)]
pub fn string_from_winreg_value(val: &winreg::RegValue) -> Option<String> {
    use {std::slice, winreg::enums::RegType};

    match val.vtype {
        RegType::REG_SZ | RegType::REG_EXPAND_SZ => {
            // Copied from winreg
            let words = unsafe {
                slice::from_raw_parts(val.bytes.as_ptr() as *const u16, val.bytes.len() / 2)
            };
            let Ok(mut s) = String::from_utf16(words) else {
                return None;
            };
            while s.ends_with('\u{0}') {
                s.pop();
            }
            Some(s)
        }
        _ => None,
    }
}
// Get the windows PATH variable out of the registry as a String. If
// this returns None then the PATH variable is not Unicode and we
// should not mess with it.
#[cfg(windows)]
fn get_windows_path_var() -> Result<Option<String>, String> {
    use winreg::{
        RegKey,
        enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
    };

    let root = RegKey::predef(HKEY_CURRENT_USER);
    let environment = root
        .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
        .map_err(|err| format!("Unable to open HKEY_CURRENT_USER\\Environment: {err}"))?;

    let reg_value = environment.get_raw_value("PATH");
    match reg_value {
        Ok(val) => {
            if let Some(s) = string_from_winreg_value(&val) {
                Ok(Some(s))
            } else {
                println!(
                    "the registry key HKEY_CURRENT_USER\\Environment\\PATH does not contain valid \
                     Unicode. Not modifying the PATH variable"
                );
                Ok(None)
            }
        }
        Err(ref e) if e.kind() == io::ErrorKind::NotFound => Ok(Some(String::new())),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(windows)]
fn add_to_path(new_path: &str) -> bool {
    use {
        std::ptr,
        winapi::{
            shared::minwindef::*,
            um::winuser::{
                HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutA, WM_SETTINGCHANGE,
            },
        },
        winreg::{
            RegKey, RegValue,
            enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RegType},
        },
    };

    let Some(old_path) =
        get_windows_path_var().unwrap_or_else(|err| panic!("Unable to get PATH: {err}"))
    else {
        return false;
    };

    if !old_path.contains(new_path) {
        let mut new_path = new_path.to_string();
        if !old_path.is_empty() {
            new_path.push(';');
            new_path.push_str(&old_path);
        }

        let root = RegKey::predef(HKEY_CURRENT_USER);
        let environment = root
            .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
            .unwrap_or_else(|err| panic!("Unable to open HKEY_CURRENT_USER\\Environment: {err}"));

        let reg_value = RegValue {
            bytes: string_to_winreg_bytes(&new_path),
            vtype: RegType::REG_EXPAND_SZ,
        };

        environment
            .set_raw_value("PATH", &reg_value)
            .unwrap_or_else(|err| panic!("Unable set HKEY_CURRENT_USER\\Environment\\PATH: {err}"));

        // Tell other processes to update their environment
        unsafe {
            SendMessageTimeoutA(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0_usize,
                c"Environment".as_ptr() as LPARAM,
                SMTO_ABORTIFHUNG,
                5000,
                ptr::null_mut(),
            );
        }
    }

    println!(
        "\n{}\n  {}\n\n{}",
        style("The HKEY_CURRENT_USER/Environment/PATH registry key has been modified to include:")
            .bold(),
        new_path,
        style(
            "Future applications will automatically have the correct environment, but you may \
             need to restart your current shell."
        )
        .bold()
    );
    true
}

#[cfg(unix)]
fn add_to_path(new_path: &str) -> bool {
    let shell_export_string = format!("\nexport PATH=\"{new_path}:$PATH\"");
    let mut modified_rcfiles = false;

    // Look for sh, bash, and zsh rc files
    let mut rcfiles = vec![dirs_next::home_dir().map(|p| p.join(".profile"))];
    if let Ok(shell) = std::env::var("SHELL")
        && shell.contains("zsh")
    {
        let zdotdir = std::env::var("ZDOTDIR")
            .ok()
            .map(PathBuf::from)
            .or_else(dirs_next::home_dir);
        let zprofile = zdotdir.map(|p| p.join(".zprofile"));
        rcfiles.push(zprofile);
    }

    if let Some(bash_profile) = dirs_next::home_dir().map(|p| p.join(".bash_profile")) {
        // Only update .bash_profile if it exists because creating .bash_profile
        // will cause .profile to not be read
        if bash_profile.exists() {
            rcfiles.push(Some(bash_profile));
        }
    }
    let rcfiles = rcfiles.into_iter().filter_map(|f| f.filter(|f| f.exists()));

    // For each rc file, append a PATH entry if not already present
    for rcfile in rcfiles {
        if !rcfile.exists() {
            continue;
        }

        fn read_file(path: &Path) -> io::Result<String> {
            let mut file = fs::OpenOptions::new().read(true).open(path)?;
            let mut contents = String::new();
            io::Read::read_to_string(&mut file, &mut contents)?;
            Ok(contents)
        }

        match read_file(&rcfile) {
            Err(err) => {
                println!("Unable to read {rcfile:?}: {err}");
            }
            Ok(contents) => {
                if !contents.contains(&shell_export_string) {
                    println!(
                        "Adding {} to {}",
                        style(&shell_export_string).italic(),
                        style(rcfile.to_str().unwrap()).bold()
                    );

                    fn append_file(dest: &Path, line: &str) -> io::Result<()> {
                        use std::io::Write;
                        let mut dest_file = fs::OpenOptions::new()
                            .append(true)
                            .create(true)
                            .open(dest)?;

                        writeln!(&mut dest_file, "{line}")?;

                        dest_file.sync_data()?;

                        Ok(())
                    }
                    append_file(&rcfile, &shell_export_string).unwrap_or_else(|err| {
                        println!("Unable to append to {rcfile:?}: {err}");
                    });
                    modified_rcfiles = true;
                }
            }
        }
    }

    if modified_rcfiles {
        println!(
            "\n{}\n  {}\n",
            style(
                "Close and reopen your terminal to apply the PATH changes or run the following in \
                 your existing shell:"
            )
            .bold()
            .blue(),
            shell_export_string
        );
    }

    modified_rcfiles
}

pub fn init(
    config_file: &str,
    data_dir: &str,
    no_modify_path: bool,
    explicit_release: ExplicitRelease,
) -> Result<(), String> {
    let config = Config::new(data_dir, explicit_release);
    if let Ok(current_config) = Config::load(config_file) {
        // Write new config file only if different, so that running |agave-install init|
        // repeatedly doesn't unnecessarily re-download
        if current_config != config {
            config.save(config_file)?;
        }
    } else {
        config.save(config_file)?;
    }

    init_or_update(config_file, true, false)?;

    let path_modified = if !no_modify_path {
        add_to_path(config.active_release_bin_dir().to_str().unwrap())
    } else {
        false
    };

    if !path_modified && !no_modify_path {
        check_env_path_for_bin_dir(&config);
    }
    Ok(())
}

fn github_release_download_url(release_semver: &str) -> String {
    format!(
        "https://github.com/anza-xyz/agave/releases/download/v{}/solana-release-{}.tar.bz2",
        release_semver,
        crate::build_env::TARGET
    )
}

fn release_channel_download_url(release_channel: &str) -> String {
    format!(
        "https://release.anza.xyz/{}/solana-release-{}.tar.bz2",
        release_channel,
        crate::build_env::TARGET
    )
}

fn release_channel_version_url(release_channel: &str) -> String {
    format!(
        "https://release.anza.xyz/{}/solana-release-{}.yml",
        release_channel,
        crate::build_env::TARGET
    )
}

pub fn info(config_file: &str, local_info_only: bool, eval: bool) -> Result<(), String> {
    let config = Config::load(config_file)?;

    if eval {
        println!(
            "SOLANA_INSTALL_ACTIVE_RELEASE={}",
            config.active_release_dir().to_str().unwrap_or("")
        );
        let channel = match &config.explicit_release {
            ExplicitRelease::Semver(semver) => semver,
            ExplicitRelease::Channel(channel) => channel,
        };
        println!("SOLANA_INSTALL_ACTIVE_CHANNEL={channel}",);
        return Ok(());
    }

    println_name_value("Configuration:", config_file);
    println_name_value(
        "Active release directory:",
        config.active_release_dir().to_str().unwrap_or("?"),
    );

    fn print_release_version(config: &Config) {
        if let Ok(release_version) =
            load_release_version(&config.active_release_dir().join("version.yml"))
        {
            println_name_value(
                &format!("{BULLET}Release commit:"),
                &release_version.commit[0..7],
            );
        }
    }

    match &config.explicit_release {
        ExplicitRelease::Semver(release_semver) => {
            println_name_value(&format!("{BULLET}Release version:"), release_semver);
            println_name_value(
                &format!("{BULLET}Release URL:"),
                &github_release_download_url(release_semver),
            );
        }
        ExplicitRelease::Channel(release_channel) => {
            println_name_value(&format!("{BULLET}Release channel:"), release_channel);
            println_name_value(
                &format!("{BULLET}Release URL:"),
                &release_channel_download_url(release_channel),
            );
        }
    }
    print_release_version(&config);

    if local_info_only {
        Ok(())
    } else {
        update(config_file, true).map(|_| ())
    }
}

#[cfg(windows)]
fn symlink_dir<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dst: Q) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
}
#[cfg(not(windows))]
fn symlink_dir<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dst: Q) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

pub fn gc(config_file: &str) -> Result<(), String> {
    let config = Config::load(config_file)?;

    let entries = fs::read_dir(&config.releases_dir)
        .map_err(|err| format!("Unable to read {}: {}", config.releases_dir.display(), err))?;

    let mut releases = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .metadata()
                .ok()
                .map(|metadata| (entry.path(), metadata))
        })
        .filter_map(|(release_path, metadata)| {
            if metadata.is_dir() {
                Some((release_path, metadata))
            } else {
                None
            }
        })
        .filter_map(|(release_path, metadata)| {
            metadata
                .modified()
                .ok()
                .map(|modified_time| (release_path, modified_time))
        })
        .collect::<Vec<_>>();
    releases.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // order by newest releases

    const MAX_CACHE_LEN: usize = 5;
    if releases.len() > MAX_CACHE_LEN {
        let old_releases = releases.split_off(MAX_CACHE_LEN);

        if !old_releases.is_empty() {
            let progress_bar = new_spinner_progress_bar();
            progress_bar.set_length(old_releases.len() as u64);
            progress_bar.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green}{wide_msg} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
                    .expect("ProgresStyle::template direct input to be correct")
                    .progress_chars("=> "),
            );
            progress_bar.set_message(format!("{RECYCLING}Removing old releases"));
            for (release, _modified_type) in old_releases {
                progress_bar.inc(1);
                let _ = fs::remove_dir_all(release);
            }
            progress_bar.finish_and_clear();
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GithubRelease {
    pub tag_name: String,
    pub prerelease: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GithubError {
    pub message: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GithubReleases(Vec<GithubRelease>);

fn semver_of(string: &str) -> Result<semver::Version, String> {
    if string.starts_with('v') {
        semver::Version::parse(string.split_at(1).1)
    } else {
        semver::Version::parse(string)
    }
    .map_err(|err| err.to_string())
}

fn check_for_newer_github_release(
    current_release_semver: &str,
    semver_update_type: SemverUpdateType,
    prerelease_allowed: bool,
) -> Result<Option<String>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("agave-install")
        .build()
        .map_err(|err| err.to_string())?;

    // If we want a fixed version, we don't need to stress the API to check whether it exists
    if semver_update_type == SemverUpdateType::Fixed {
        let download_url = github_release_download_url(current_release_semver);
        let response = client
            .head(download_url.as_str())
            .send()
            .map_err(|err| err.to_string())?;

        if response.status() == reqwest::StatusCode::OK {
            return Ok(Some(current_release_semver.to_string()));
        }
    }

    let version_filter = semver::VersionReq::parse(&format!(
        "{}{}",
        match semver_update_type {
            SemverUpdateType::Fixed => "=",
            SemverUpdateType::Patch => "~",
            SemverUpdateType::_Minor => "^",
        },
        current_release_semver
    ))
    .ok();

    let mut page = 1;
    const PER_PAGE: usize = 100;
    let mut all_releases = vec![];
    let mut releases = vec![];

    while page == 1 || releases.len() == PER_PAGE {
        let url = reqwest::Url::parse_with_params(
            "https://api.github.com/repos/anza-xyz/agave/releases",
            &[
                ("per_page", &format!("{PER_PAGE}")),
                ("page", &format!("{page}")),
            ],
        )
        .unwrap();
        let request = client.get(url).build().map_err(|err| err.to_string())?;
        let response = client.execute(request).map_err(|err| err.to_string())?;

        if response.status() == reqwest::StatusCode::OK {
            releases = response
                .json::<GithubReleases>()
                .map_err(|err| err.to_string())?
                .0
                .into_iter()
                .filter_map(
                    |GithubRelease {
                         tag_name,
                         prerelease,
                     }| {
                        if let Ok(version) = semver_of(&tag_name)
                            && (prerelease_allowed || !prerelease)
                            && version_filter
                                .as_ref()
                                .is_none_or(|version_filter| version_filter.matches(&version))
                        {
                            return Some(version);
                        }
                        None
                    },
                )
                .collect::<Vec<_>>();
            all_releases.extend_from_slice(&releases);
            page += 1;
        } else {
            return Err(response
                .json::<GithubError>()
                .map_err(|err| err.to_string())?
                .message);
        }
    }

    all_releases.sort();
    Ok(all_releases.pop().map(|r| r.to_string()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum SemverUpdateType {
    Fixed,
    Patch,
    _Minor,
}

pub fn update(config_file: &str, check_only: bool) -> Result<bool, String> {
    init_or_update(config_file, false, check_only)
}

pub fn init_or_update(config_file: &str, is_init: bool, check_only: bool) -> Result<bool, String> {
    let mut config = Config::load(config_file)?;

    let (updated_version, download_url_and_sha256, release_dir) = match &config.explicit_release {
        ExplicitRelease::Semver(current_release_semver) => {
            let release_dir = config.release_dir(current_release_semver);
            if is_init && release_dir.exists() {
                (current_release_semver.to_owned(), None, release_dir)
            } else {
                let progress_bar = new_spinner_progress_bar();
                progress_bar.set_message(format!("{LOOKING_GLASS}Checking for updates..."));

                let semver_update_type = if is_init {
                    SemverUpdateType::Fixed
                } else {
                    SemverUpdateType::Patch
                };
                let github_release = check_for_newer_github_release(
                    current_release_semver,
                    semver_update_type,
                    is_init,
                )?;

                progress_bar.finish_and_clear();

                match github_release {
                    None => {
                        return Err(format!("Unknown release: {current_release_semver}"));
                    }
                    Some(release_semver) => {
                        if release_semver == *current_release_semver
                            && let Ok(active_release_version) = load_release_version(
                                &config.active_release_dir().join("version.yml"),
                            )
                            && format!("v{current_release_semver}")
                                == active_release_version.channel
                        {
                            println!(
                                "Install is up to date. {release_semver} is the latest compatible \
                                 release"
                            );
                            return Ok(false);
                        }
                        config.explicit_release = ExplicitRelease::Semver(release_semver.clone());

                        let release_dir = config.release_dir(&release_semver);
                        let download_url_and_sha256 = if release_dir.exists() {
                            // Release already present in the cache
                            None
                        } else {
                            Some((github_release_download_url(&release_semver), None))
                        };
                        (release_semver, download_url_and_sha256, release_dir)
                    }
                }
            }
        }
        ExplicitRelease::Channel(release_channel) => {
            let version_url = release_channel_version_url(release_channel);

            let (_temp_dir, temp_file, _temp_archive_sha256) = download_to_temp(&version_url, None)
                .map_err(|err| format!("Unable to download {version_url}: {err}"))?;

            let update_release_version = load_release_version(&temp_file)?;

            let release_id = format!("{}-{}", release_channel, update_release_version.commit);
            let release_dir = config.release_dir(&release_id);
            let current_release_version_yml =
                release_dir.join("solana-release").join("version.yml");

            let download_url = release_channel_download_url(release_channel);

            if !current_release_version_yml.exists() {
                (
                    format!(
                        "{} commit {}",
                        release_channel,
                        &update_release_version.commit[0..7]
                    ),
                    Some((download_url, None)),
                    release_dir,
                )
            } else {
                let current_release_version = load_release_version(&current_release_version_yml)?;
                if update_release_version.commit == current_release_version.commit {
                    if let Ok(active_release_version) =
                        load_release_version(&config.active_release_dir().join("version.yml"))
                        && current_release_version.commit == active_release_version.commit
                    {
                        // Same version, no update required
                        println!(
                            "Install is up to date. {} is the latest commit for {}",
                            &active_release_version.commit[0..7],
                            release_channel
                        );
                        return Ok(false);
                    }

                    // Release already present in the cache
                    (
                        format!(
                            "{} commit {}",
                            release_channel,
                            &update_release_version.commit[0..7]
                        ),
                        None,
                        release_dir,
                    )
                } else {
                    (
                        format!(
                            "{} (from {})",
                            &update_release_version.commit[0..7],
                            &current_release_version.commit[0..7],
                        ),
                        Some((download_url, None)),
                        release_dir,
                    )
                }
            }
        }
    };

    if check_only {
        println!(
            "  {}{}",
            WRAPPED_PRESENT,
            style(format!("Update available: {updated_version}")).bold()
        );
        return Ok(true);
    }

    if let Some((download_url, archive_sha256)) = download_url_and_sha256 {
        let (_temp_dir, temp_archive, _temp_archive_sha256) =
            download_to_temp(&download_url, archive_sha256.as_ref())
                .map_err(|err| format!("Unable to download {download_url}: {err}"))?;
        extract_release_archive(&temp_archive, &release_dir).map_err(|err| {
            format!("Unable to extract {temp_archive:?} to {release_dir:?}: {err}")
        })?;
    }

    let release_target = load_release_target(&release_dir)
        .map_err(|err| format!("Unable to load release target from {release_dir:?}: {err}"))?;

    if release_target != crate::build_env::TARGET {
        return Err(format!("Incompatible update target: {release_target}"));
    }

    // Trigger an update to the modification time for `release_dir`
    {
        let path = &release_dir.join(".touch");
        let _ = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path);
        let _ = fs::remove_file(path);
    }

    let _ = fs::remove_dir_all(config.active_release_dir());
    symlink_dir(
        release_dir.join("solana-release"),
        config.active_release_dir(),
    )
    .map_err(|err| match err.raw_os_error() {
        #[cfg(windows)]
        Some(os_err) if os_err == winapi::shared::winerror::ERROR_PRIVILEGE_NOT_HELD as i32 => {
            "You need to run this command with administrator privileges.".to_string()
        }
        _ => format!(
            "Unable to symlink {:?} to {:?}: {}",
            release_dir,
            config.active_release_dir(),
            err
        ),
    })?;

    config.save(config_file)?;
    gc(config_file)?;

    if is_init {
        println!(
            "  {}{}",
            SPARKLE,
            style(format!("{updated_version} initialized")).bold()
        );
    } else {
        println!(
            "  {}{}",
            SPARKLE,
            style(format!("Update successful to {updated_version}")).bold()
        );
    }
    Ok(true)
}

pub fn run(
    config_file: &str,
    program_name: &str,
    program_arguments: Vec<&str>,
) -> Result<(), String> {
    let config = Config::load(config_file)?;

    let mut full_program_path = config.active_release_bin_dir().join(program_name);
    if cfg!(windows) && full_program_path.extension().is_none() {
        full_program_path.set_extension("exe");
    }

    if !full_program_path.exists() {
        return Err(format!(
            "{} does not exist",
            full_program_path.to_str().unwrap()
        ));
    }

    let mut child_option: Option<std::process::Child> = None;
    let mut now = Instant::now();

    let (signal_sender, signal_receiver) = unbounded();
    ctrlc::set_handler(move || {
        let _ = signal_sender.send(());
    })
    .expect("Error setting Ctrl-C handler");

    loop {
        child_option = match child_option {
            Some(mut child) => match child.try_wait() {
                Ok(Some(status)) => {
                    println_name_value(
                        &format!("{program_name} exited with:"),
                        &status.to_string(),
                    );
                    None
                }
                Ok(None) => Some(child),
                Err(err) => {
                    eprintln!("Error attempting to wait for program to exit: {err}");
                    None
                }
            },
            None => {
                match std::process::Command::new(&full_program_path)
                    .args(&program_arguments)
                    .spawn()
                {
                    Ok(child) => Some(child),
                    Err(err) => {
                        eprintln!("Failed to spawn {program_name}: {err:?}");
                        None
                    }
                }
            }
        };

        if now.elapsed().as_secs() > config.update_poll_secs {
            match update(config_file, false) {
                Ok(true) => {
                    // Update successful, kill current process so it will be restart
                    if let Some(ref mut child) = child_option {
                        stop_process(child).unwrap_or_else(|err| {
                            eprintln!("Failed to stop child: {err:?}");
                        });
                    }
                }
                Ok(false) => {} // No update available
                Err(err) => {
                    eprintln!("Failed to apply update: {err:?}");
                }
            };
            now = Instant::now();
        }

        if let Ok(()) = signal_receiver.recv_timeout(Duration::from_secs(1)) {
            // Handle SIGTERM...
            if let Some(ref mut child) = child_option {
                stop_process(child).unwrap_or_else(|err| {
                    eprintln!("Failed to stop child: {err:?}");
                });
            }
            std::process::exit(0);
        }
    }
}

pub fn list(config_file: &str) -> Result<(), String> {
    let config = Config::load(config_file)?;

    let entries = fs::read_dir(&config.releases_dir).map_err(|err| {
        format!(
            "Failed to read install directory, double check that your configuration file is \
             correct: {err}"
        )
    })?;

    let current_version =
        load_release_version(&config.active_release_dir().join("version.yml"))?.channel;

    for entry in entries {
        match entry {
            Ok(entry) => {
                let dir_name = entry.file_name();
                let current = if current_version.contains(dir_name.to_string_lossy().as_ref()) {
                    " (current)"
                } else {
                    ""
                };
                println!("{}{}", dir_name.to_string_lossy(), current);
            }
            Err(err) => {
                eprintln!("error listing installed versions: {err:?}");
            }
        };
    }
    Ok(())
}

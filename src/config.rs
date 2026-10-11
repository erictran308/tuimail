use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

use crate::text;

/// Settings read from the environment, or, in development builds, from
/// `.env` in the source folder ([`load_dotenv`]).
pub struct Config {
    /// Where your accounts, the mail cache (`tuimail.db`) and
    /// `settings.toml` are kept.
    pub data_dir: PathBuf,
}

impl Config {
    pub fn load() -> Result<Self> {
        let data_dir = make_private_dir(&data_dir()?)?;
        Ok(Self { data_dir })
    }
}

/// Makes the data folder, or checks the one that's there, so only you can
/// reach it, and gives its path with links resolved. In this order, so
/// nothing is created or changed somewhere unsafe: the folders above it may
/// be changed only by you or the system ([`private_place`]); it's then
/// made, mode 0700, or found to be a real folder (not a link) that's yours;
/// then set to 0700 again, and on macOS cleared of access rules inherited
/// from above, which would let others in whatever the mode says.
#[cfg(unix)]
fn make_private_dir(dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let shown = shown(dir);
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        bail!("{shown} isn't a folder tuimail can use");
    };
    std::fs::create_dir_all(parent).with_context(|| format!("cannot create {shown}"))?;
    let parent = std::fs::canonicalize(parent).with_context(|| format!("cannot read {shown}"))?;
    private_place(&parent)?;
    let dir = parent.join(name);
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(e).with_context(|| format!("cannot create {shown}"));
        }
        _ => {}
    }
    let meta = std::fs::symlink_metadata(&dir).with_context(|| format!("cannot read {shown}"))?;
    if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != euid() {
        bail!("{shown} isn't a folder of yours, so it's no place for your data");
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot protect {shown}"))?;
    #[cfg(target_os = "macos")]
    clear_acls(&dir);
    Ok(dir)
}

/// On Windows, a folder set with `TM_DATA_DIR` must be in your own profile,
/// whose permissions are yours alone; tuimail doesn't set Windows ACLs.
#[cfg(not(unix))]
fn make_private_dir(dir: &Path) -> Result<PathBuf> {
    let shown = shown(dir);
    if var("TM_DATA_DIR").is_some() {
        let mine = ["USERPROFILE", "LOCALAPPDATA"]
            .iter()
            .filter_map(std::env::var_os)
            .any(|base| dir.starts_with(base));
        if !mine {
            bail!("TM_DATA_DIR must be a folder in your user profile ({shown} isn't)");
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {shown}"))?;
    Ok(dir.to_path_buf())
}

/// Removes access rules (ACLs) from `dir` and everything in it: a folder
/// made inside one with an inherited rule for everyone gets that rule too,
/// and mode 0700 doesn't override it. `/bin/chmod -N` by its full path.
#[cfg(target_os = "macos")]
fn clear_acls(dir: &Path) {
    let _ = std::process::Command::new("/bin/chmod")
        .args(["-R", "-N"])
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The user tuimail runs as.
#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: geteuid can't fail and touches no memory.
    unsafe { libc::geteuid() }
}

/// `TM_DATA_DIR`, else the platform's place for app data: `~/Library/Application
/// Support/tuimail` on macOS, `~/.local/share/tuimail` on Linux, `%LOCALAPPDATA%\tuimail`
/// on Windows. The same wherever the command is run from.
pub fn data_dir() -> Result<PathBuf> {
    data_dir_from(var("TM_DATA_DIR"))
}

fn data_dir_from(set: Option<String>) -> Result<PathBuf> {
    if let Some(dir) = set {
        let dir = PathBuf::from(dir);
        // On Windows even creating a folder on another machine hands that
        // server your login hash. A relative path would depend on where
        // tuimail is started: a cloned folder could hand you its own data.
        let elsewhere = maybe_remote(&dir) || !dir.is_absolute();
        if elsewhere {
            bail!(
                "TM_DATA_DIR must be a full path to a folder on this computer \
                 (with a drive letter on Windows)"
            );
        }
        return Ok(dir);
    }
    let base = dirs::data_local_dir().context("no home directory found; set TM_DATA_DIR")?;
    Ok(base.join("tuimail"))
}

/// Whether `path` names a file on another machine: `\\server\share\…` (or
/// `//server/share/…`) on Windows. Looking at such a path connects to that
/// server and hands it the user's Windows login hash.
pub fn on_another_machine(path: &Path) -> bool {
    matches!(
        path.as_os_str().as_encoded_bytes(),
        [b'/' | b'\\', b'/' | b'\\', ..]
    )
}

/// Whether `path` may not be on this computer's own drives: on Windows, any
/// path that starts with a separator, since besides `\\server\share` the NT
/// spellings (`\??\UNC\server\…`) reach the network too, and only a drive
/// letter is surely local. Elsewhere, [`on_another_machine`].
pub fn maybe_remote(path: &Path) -> bool {
    on_another_machine(path) || (cfg!(windows) && starts_with_separator(path))
}

fn starts_with_separator(path: &Path) -> bool {
    matches!(path.as_os_str().as_encoded_bytes(), [b'/' | b'\\', ..])
}

/// A path as it can be printed: it may come from the environment.
pub fn shown(path: &Path) -> String {
    text::clean(&path.display().to_string())
}

/// Checks that only you or the system can change `dir` and the folders
/// above it: otherwise another account could rename the data folder away
/// and put its own in its place. Folders anyone may write to, like `/tmp`,
/// are fine when only an entry's owner can move it (the sticky bit) and the
/// system owns them. Owners are compared with the user tuimail runs as,
/// not with the folder's own owner.
#[cfg(unix)]
fn private_place(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let uid = euid();
    // SAFETY: getgid can't fail and touches no memory.
    let gid = unsafe { libc::getgid() };
    let unsafe_place = |what: &Path| {
        anyhow::anyhow!(
            "{} can be changed by other users, so it's no place for your data; \
             set TM_DATA_DIR somewhere in your home folder",
            shown(what)
        )
    };
    for folder in std::fs::canonicalize(dir)?.ancestors() {
        let meta = std::fs::metadata(folder)?;
        let mode = meta.mode();
        // Your own group is only yours on Linux (user private groups); on
        // macOS every account is in `staff`.
        let group_is_others = cfg!(target_os = "macos") || meta.gid() != gid;
        let others_write = mode & 0o002 != 0 || (mode & 0o020 != 0 && group_is_others);
        let sticky = mode & 0o1000 != 0;
        let owner_ok = meta.uid() == uid || meta.uid() == 0;
        if !owner_ok || (others_write && !(sticky && meta.uid() == 0)) {
            return Err(unsafe_place(folder));
        }
    }
    Ok(())
}

/// `TM_*` settings from `.env` in the source folder, for development.
static DOTENV: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Reads `.env` from tuimail's own source folder (where it was built), not
/// the working directory, and keeps only its `TM_*` keys, without touching
/// the process environment. `TM_DATA_DIR` picks where your mail and
/// passwords' settings are kept, so a `.env` in whatever folder tuimail is
/// started from must never count. Only development builds read it.
pub fn load_dotenv() {
    if !cfg!(debug_assertions) {
        let _ = DOTENV.set(HashMap::new());
        return;
    }
    let vars = dotenvy::from_path_iter(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|(key, _)| key.starts_with("TM_"))
        .collect();
    let _ = DOTENV.set(vars);
}

/// A variable's trimmed value from the environment, else from `.env`, or
/// `None` if it's unset or blank.
pub fn var(name: &str) -> Option<String> {
    let value = std::env::var(name)
        .ok()
        .or_else(|| DOTENV.get()?.get(name).cloned())?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_data_folder_on_another_machine_is_refused() {
        for dir in ["//evil.example/s/tt", "\\\\evil.example\\s\\tt"] {
            assert!(data_dir_from(Some(dir.into())).is_err(), "{dir}");
        }
        assert!(
            data_dir_from(Some("./.tuimail".into())).is_err(),
            "relative"
        );
        assert!(data_dir_from(Some(".tuimail".into())).is_err(), "relative");
    }

    #[test]
    fn nt_spellings_of_a_share_count_as_maybe_remote_on_windows() {
        for path in [
            "\\??\\UNC\\evil.example\\s\\subscriptions.csv",
            "\\\\?\\UNC\\evil.example\\s\\x.csv",
            "\\\\evil.example\\s\\x.csv",
            "/??/UNC/evil.example/s/x.csv",
        ] {
            assert!(starts_with_separator(Path::new(path)), "{path}");
        }
        assert!(!starts_with_separator(Path::new("C:\\Users\\me\\x.csv")));
        assert!(!on_another_machine(Path::new("\\??\\UNC\\h\\s\\x.csv")));
        assert_eq!(
            maybe_remote(Path::new("\\??\\UNC\\h\\s\\x.csv")),
            cfg!(windows),
            "refused on Windows, a plain (odd) local path elsewhere"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_data_folder_others_could_swap_out_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid can't fail and touches no memory.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let parent = std::env::temp_dir().join(format!("tuimail-place-{}", std::process::id()));
        let dir = parent.join("tt");
        std::fs::create_dir_all(&dir).unwrap();
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        mode(&parent, 0o777);
        assert!(private_place(&dir).is_err(), "anyone could rename it");
        mode(&parent, 0o1777);
        assert!(private_place(&dir).is_err(), "sticky, but not the system's");
        mode(&parent, 0o755);
        private_place(&dir).unwrap();
        // A data folder that's someone else's (the system's here), however
        // safe the folders above it.
        assert!(make_private_dir(Path::new("/usr/share")).is_err());
        let made = make_private_dir(&parent.join("made")).unwrap();
        let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_of(&made), 0o700);
        // A link in its place is refused, not followed.
        let target = parent.join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, parent.join("link")).unwrap();
        assert!(make_private_dir(&parent.join("link")).is_err());
        assert_eq!(mode_of(&target), 0o755, "not changed through the link");
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn paths_are_printed_without_control_characters() {
        assert_eq!(shown(Path::new("/tmp/a\u{1b}]0;x\u{7}b")), "/tmp/a]0;xb");
    }
}

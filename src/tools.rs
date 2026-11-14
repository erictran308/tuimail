//! The programs tuimail starts: the browser or app that opens a link or an
//! attachment, and your editor. A program is never named bare: it's found
//! as an absolute path in `PATH`'s absolute folders, so one planted in the
//! folder tuimail was started from can't run. And saving attachments, each
//! marked as downloaded from the internet.

#[cfg(unix)]
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::config;

/// Folders looked in after `PATH`, where package managers put programs
/// that a GUI-started shell may not have on its `PATH`.
#[cfg(unix)]
const EXTRA_DIRS: [&str; 4] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/run/current-system/sw/bin",
];
#[cfg(not(unix))]
const EXTRA_DIRS: [&str; 0] = [];

/// An absolute path to an existing file that this user can run.
fn usable(path: &Path) -> bool {
    if !path.is_absolute() || config::on_another_machine(path) {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    meta.is_file()
}

/// `name` in the absolute folders on `PATH`, then in [`EXTRA_DIRS`]; a
/// full path is taken as it is if it can run.
pub fn on_path(name: &str) -> Option<PathBuf> {
    let given = Path::new(name);
    if given.is_absolute() {
        return usable(given).then(|| given.to_path_buf());
    }
    if name.contains(['/', '\\']) {
        return None;
    }
    let exe = if cfg!(windows) && !name.ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(EXTRA_DIRS.iter().map(PathBuf::from))
        .filter(|dir| dir.is_absolute() && !config::on_another_machine(dir))
        .map(|dir| dir.join(&exe))
        .find(|candidate| usable(candidate))
}

/// `PATH` with only its absolute, local folders; `None` when nothing is
/// left (an empty `PATH` would mean the working directory). tuimail's own
/// `PATH` is set to this at start, so a program that a dependency or a
/// child (xdg-open) starts by name can't come from the folder tuimail was
/// started in.
#[cfg(unix)]
pub fn absolute_path_entries(path: Option<OsString>) -> Option<OsString> {
    let kept: Vec<PathBuf> = std::env::split_paths(&path?)
        .filter(|dir| dir.is_absolute() && !config::on_another_machine(dir))
        .collect();
    if kept.is_empty() {
        return None;
    }
    std::env::join_paths(kept).ok()
}

/// Opens a link or a file in the program the system picks: `open` on
/// macOS, ShellExecute on Windows (never `cmd /C start`, which would run
/// what follows a `&` in a link), `xdg-open` found as a full path
/// elsewhere, in its own process group and the home folder.
pub fn open_externally(target: &str) -> std::io::Result<()> {
    if cfg!(target_os = "macos") {
        // `open` waits for LaunchServices, which can take a moment.
        let target = target.to_string();
        std::thread::spawn(move || open::that_detached(target));
        return Ok(());
    }
    if cfg!(windows) {
        return open::that_detached(target);
    }
    let opener = on_path("xdg-open").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "xdg-open isn't installed")
    })?;
    let mut command = std::process::Command::new(opener);
    command
        .arg(target)
        .current_dir(dirs::home_dir().unwrap_or_else(|| "/".into()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// An attachment opener with the file as one literal, absolute argument.
/// Like the system opener, it runs from home, not beside untrusted files.
pub fn attachment_command(argv: &[String], path: &Path) -> Result<Command> {
    let name = argv
        .first()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("the attachment opener's command is empty"))?;
    let program = on_path(name).ok_or_else(|| {
        anyhow!(
            "attachment opener {} wasn't found; check its command in settings.toml",
            crate::text::one_line(name, 80)
        )
    })?;
    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .arg(path)
        .current_dir(dirs::home_dir().unwrap_or_else(|| "/".into()));
    Ok(command)
}

/// Foreground viewers inherit the released terminal. Other viewers get
/// no terminal streams and run detached, just like the system opener.
pub fn open_attachment(mut command: Command, terminal: bool) -> Result<()> {
    if terminal {
        let status = foreground_status(&mut command).context("cannot run attachment opener")?;
        if !status.success() {
            bail!("attachment opener ended with {status}");
        }
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().context("cannot run attachment opener")?;
        std::thread::spawn(move || child.wait());
    }
    Ok(())
}

/// Ignore terminal interrupts in tuimail, not in the program using its
/// terminal. Serialize this process-wide signal change, including in tests.
fn foreground_status(command: &mut Command) -> std::io::Result<std::process::ExitStatus> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    #[cfg(unix)]
    let _signals = ForegroundSignals::new(command)?;
    #[cfg(windows)]
    let _signals = ForegroundSignals::new()?;
    command.status()
}

#[cfg(unix)]
struct ForegroundSignals([libc::sigaction; 2]);

#[cfg(unix)]
impl ForegroundSignals {
    fn new(command: &mut Command) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        // SAFETY: these are initialized sigactions and valid signal numbers.
        let signals = unsafe {
            let mut previous: [libc::sigaction; 2] = std::mem::zeroed();
            for (i, signal) in [libc::SIGINT, libc::SIGQUIT].into_iter().enumerate() {
                if libc::sigaction(signal, std::ptr::null(), &mut previous[i]) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let signals = Self(previous);
            let mut ignored: libc::sigaction = std::mem::zeroed();
            ignored.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut ignored.sa_mask);
            for signal in [libc::SIGINT, libc::SIGQUIT] {
                if libc::sigaction(signal, &ignored, std::ptr::null_mut()) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            signals
        };
        // SAFETY: after fork, only async-signal-safe operations run here.
        // Ignored dispositions survive exec, so reset them in the child.
        unsafe {
            command.pre_exec(|| {
                let mut default: libc::sigaction = std::mem::zeroed();
                default.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut default.sa_mask);
                for signal in [libc::SIGINT, libc::SIGQUIT] {
                    if libc::sigaction(signal, &default, std::ptr::null_mut()) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(signals)
    }
}

#[cfg(unix)]
impl Drop for ForegroundSignals {
    fn drop(&mut self) {
        for (signal, previous) in [libc::SIGINT, libc::SIGQUIT].into_iter().zip(&self.0) {
            // SAFETY: restores the complete action read before the handoff.
            unsafe { libc::sigaction(signal, previous, std::ptr::null_mut()) };
        }
    }
}

#[cfg(windows)]
struct ForegroundSignals;

#[cfg(windows)]
unsafe extern "system" fn foreground_control(control: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    i32::from(matches!(control, CTRL_C_EVENT | CTRL_BREAK_EVENT))
}

#[cfg(windows)]
impl ForegroundSignals {
    fn new() -> std::io::Result<Self> {
        // A handler, unlike the inherited ignore flag, leaves the child
        // interruptible. Only this process handles its Ctrl-C and Ctrl-Break.
        let installed = unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(foreground_control), 1)
        };
        if installed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for ForegroundSignals {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(foreground_control), 0);
        }
    }
}

/// File types opened without asking: pictures, sound, video, PDF and plain
/// text, which their apps show without running anything in them. Office
/// files (macros, linked content), spreadsheets' CSV (formulas), archives
/// (what's inside), calendars and contacts (they're imported) ask first.
const SAFE_TO_OPEN: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "bmp", "tif", "tiff", "mp4",
    "m4v", "mov", "mkv", "webm", "mp3", "m4a", "aac", "ogg", "opus", "wav", "flac", "pdf", "txt",
];

pub fn safe_to_open(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SAFE_TO_OPEN.contains(&e.to_ascii_lowercase().as_str()))
}

/// Saves `bytes` as `name` in `dir`, under a name not taken yet ("x (1).pdf"),
/// readable only by you, marked as from the internet. Returns the path.
pub fn save_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    if !dir.is_absolute() || config::maybe_remote(dir) {
        bail!("{} isn't a folder on this computer", config::shown(dir));
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create {}", config::shown(dir)))?;
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 0..1000 {
        let candidate = if n == 0 {
            dir.join(name)
        } else {
            dir.join(format!("{stem} ({n}){ext}"))
        };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&candidate) {
            Ok(mut file) => {
                file.write_all(bytes)
                    .with_context(|| format!("cannot write {}", config::shown(&candidate)))?;
                mark_downloaded(&candidate);
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("cannot write {}", config::shown(&candidate)));
            }
        }
    }
    bail!("too many files called {name} already")
}

/// Marks a downloaded file as coming from the internet, as browsers do, so
/// the system's own checks apply when it's opened: Gatekeeper on macOS,
/// SmartScreen and Office's Protected View on Windows. Best effort.
pub fn mark_downloaded(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = std::process::Command::new("/usr/bin/xattr")
            .args([
                "-w",
                "com.apple.quarantine",
                &format!("0081;{now:x};tuimail;"),
            ])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let _ = std::fs::write(stream, "[ZoneTransfer]\r\nZoneId=3\r\n");
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = path;
}

/// The editor to write in: the `editor` setting, else `$VISUAL`, else
/// `$EDITOR` (words split on spaces), else vi; its program found as a full
/// path.
pub fn editor(setting: &[String]) -> Option<Vec<String>> {
    let words: Vec<String> = if !setting.is_empty() {
        setting.to_vec()
    } else {
        ["VISUAL", "EDITOR"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.trim().is_empty())
            .map(|v| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| vec!["vi".into()])
    };
    let program = on_path(words.first()?)?;
    let mut argv = vec![program.to_string_lossy().into_owned()];
    argv.extend(words[1..].iter().cloned());
    Some(argv)
}

/// Runs the editor on `text` in a file of its own in `dir` (yours alone),
/// waits for it, and returns what it saved. The terminal must be given to
/// it first.
pub fn edit(argv: &[String], dir: &Path, text: &str) -> Result<String> {
    std::fs::create_dir_all(dir)?;
    yours_alone(dir)?;
    let name = random_name()?;
    let path = dir.join(format!("message-{name}.txt"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&path)?.write_all(text.as_bytes())?;
    let result = (|| {
        // In the private folder, not wherever tuimail was started: an
        // editor may read settings from the folder it runs in.
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).arg(&path).current_dir(dir);
        let status = foreground_status(&mut command)
            .with_context(|| format!("cannot run {}", crate::text::one_line(&argv[0], 80)))?;
        if !status.success() {
            bail!("the editor ended with {status}; the text wasn't changed");
        }
        read_back(&path)
    })();
    let _ = std::fs::remove_file(&path);
    result
}

/// 64 random bits as hex, for a file or folder name no one can guess.
fn random_name() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut random = [0u8; 8];
    ring::rand::SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| anyhow!("the system's random numbers failed"))?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

/// Checks that `dir` is a real folder that's yours and closed to others
/// (made so if it can be): one someone else made first, in a shared place,
/// could have the editor's file swapped or read.
fn yours_alone(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let shown = config::shown(dir);
        let meta = std::fs::symlink_metadata(dir)?;
        if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != config::euid() {
            bail!("{shown} isn't a folder of yours alone, so the editor isn't run there");
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot protect {shown}"))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// A new folder of the user's alone in the system's temporary folder,
/// named so no one can make it first: where the demo, which has no data
/// folder, gives the editor its file.
pub fn private_temp_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("tuimail-{}", random_name()?));
    #[cfg(unix)]
    let builder = {
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    builder
        .create(&dir)
        .with_context(|| format!("cannot create {}", config::shown(&dir)))?;
    Ok(dir)
}

/// What the editor saved, if it's still a plain file of a sane size: read
/// without following a link put in its place or waiting on a pipe.
fn read_back(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("what the editor left isn't a plain file");
    }
    const MAX: u64 = 16 << 20;
    if meta.len() > MAX {
        bail!("the text is too long");
    }
    let mut text = String::new();
    file.take(MAX + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX {
        bail!("the text is too long");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_found_only_by_full_paths() {
        assert_eq!(on_path("./sh"), None);
        assert_eq!(on_path("bin/sh"), None);
        #[cfg(unix)]
        {
            assert!(on_path("sh").unwrap().is_absolute());
            assert_eq!(on_path("/bin/sh").as_deref(), Some(Path::new("/bin/sh")));
            assert_eq!(
                absolute_path_entries(Some("/usr/bin::.:bin:/bin".into())),
                Some("/usr/bin:/bin".into())
            );
            assert_eq!(absolute_path_entries(Some(".:x".into())), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn attachment_openers_receive_literal_arguments_and_report_foreground_failure() {
        let dir = private_temp_dir().unwrap();
        let path = dir.join("report $(echo unsafe); two.PDF");
        std::fs::write(&path, b"attachment bytes\n").unwrap();
        let literal = "--title=$(echo unsafe); *";
        for terminal in [true, false] {
            let output = dir.join(if terminal { "foreground" } else { "detached" });
            let done = output.with_extension("done");
            let argv = vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat \"$2\" > \"$0\"; printf '%s' \"$1\" >> \"$0\"; mv \"$0\" \"$0.done\"; exit 7"
                    .into(),
                output.to_string_lossy().into_owned(),
                literal.into(),
            ];
            let result = open_attachment(attachment_command(&argv, &path).unwrap(), terminal);
            assert_eq!(
                result.is_err(),
                terminal,
                "only foreground exits are awaited"
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(
                std::fs::read_to_string(&done).unwrap(),
                format!("attachment bytes\n{literal}")
            );
        }
        assert!(attachment_command(&[], &path).is_err());
        assert!(attachment_command(&["./untrusted-viewer".into()], &path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn attachment_viewer_interrupts_do_not_interrupt_tuimail() {
        const CHILD: &str = "TUIMAIL_TEST_FOREGROUND_INTERRUPT";
        if std::env::var_os(CHILD).is_some() {
            let argv = ["/bin/sh".into(), "-c".into(), "kill -INT 0".into()];
            let command = attachment_command(&argv, Path::new("/attachment.pdf")).unwrap();
            assert!(open_attachment(command, true).is_err());
            return;
        }
        // An isolated test process group: its viewer signals that whole
        // group, as a cooked terminal does, without ever signalling cargo.
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "tools::tests::attachment_viewer_interrupts_do_not_interrupt_tuimail",
            ])
            .env(CHILD, "1");
        std::os::unix::process::CommandExt::process_group(&mut child, 0);
        assert!(child.status().unwrap().success());
    }

    #[test]
    fn saved_files_never_overwrite_and_only_risky_types_ask() {
        let dir = std::env::temp_dir().join(format!("tuimail-save-{}", std::process::id()));
        let one = save_new(&dir, "report.pdf", b"one").unwrap();
        let two = save_new(&dir, "report.pdf", b"two").unwrap();
        assert_eq!(one.file_name().unwrap(), "report.pdf");
        assert_eq!(two.file_name().unwrap(), "report (1).pdf");
        assert_eq!(std::fs::read(&one).unwrap(), b"one");
        assert!(save_new(Path::new("relative"), "x", b"").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(safe_to_open("photo.JPG"));
        assert!(!safe_to_open("invoice.pdf.exe"));
        assert!(!safe_to_open("macro.docm"));
        for asks in [
            "sheet.csv",
            "letter.docx",
            "files.zip",
            "invite.ics",
            "notes.md",
        ] {
            assert!(!safe_to_open(asks), "{asks}");
        }
        assert!(!safe_to_open("noext"));
    }

    #[cfg(unix)]
    #[test]
    fn the_editor_edits_a_private_file_that_is_then_removed() {
        let dir = std::env::temp_dir().join(format!("tuimail-edit-{}", std::process::id()));
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            "printf ' and more' >> \"$0\"".into(),
        ];
        let text = edit(&argv, &dir, "start").unwrap();
        assert_eq!(text, "start and more");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "removed");
        let failing = vec!["/bin/sh".to_string(), "-c".into(), "exit 1".into()];
        assert!(edit(&failing, &dir, "x").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_editor_works_only_in_a_folder_of_yours_on_a_plain_file() {
        let sh = |script: &str| vec!["/bin/sh".to_string(), "-c".into(), script.into()];
        let base = std::env::temp_dir().join(format!("tuimail-edit2-{}", std::process::id()));
        let dir = base.join("drafts");
        std::fs::create_dir_all(&dir).unwrap();
        // What the editor leaves at the path must be a plain file: a pipe
        // is refused at once, a link isn't followed.
        let (tx, rx) = std::sync::mpsc::channel();
        let (argv, d) = (sh("rm \"$0\"; mkfifo \"$0\""), dir.clone());
        std::thread::spawn(move || tx.send(edit(&argv, &d, "x").is_err()));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(true));
        assert!(edit(&sh("rm \"$0\"; ln -s /dev/null \"$0\""), &dir, "x").is_err());
        // A folder through a link isn't used.
        std::os::unix::fs::symlink(&dir, base.join("link")).unwrap();
        assert!(edit(&sh("true"), &base.join("link"), "x").is_err());
        // The demo's folder: new, the user's alone.
        use std::os::unix::fs::PermissionsExt;
        let private = private_temp_dir().unwrap();
        let mode = std::fs::metadata(&private).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_ne!(private, private_temp_dir().unwrap(), "a new one each time");
        std::fs::remove_dir(&private).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }
}

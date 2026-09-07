use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{SavedLogin, read_login};

static CANCEL: AtomicBool = AtomicBool::new(false);

const AUTH_OVERRIDES: [&str; 7] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

// Electron Desktop also uses claude.exe. Its Windows version resource says
// "Claude"; the native CLI's says "Claude Code". Keep Desktop running.
#[cfg(windows)]
const CLI_PROCESS_COUNT_SCRIPT: &str = r"@(Get-Process | Where-Object { $_.Path -eq $env:CODEXBAR_CLAUDE_EXE -or $_.Path -like '*\.local\share\claude\versions\*' -or ($_.ProcessName -eq 'claude' -and $_.MainModule.FileVersionInfo.ProductName -ne 'Claude') }).Count";

pub fn cancel_login() {
    CANCEL.store(true, Ordering::SeqCst);
}

pub fn begin_login() {
    CANCEL.store(false, Ordering::SeqCst);
}

pub fn executable() -> io::Result<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let native = home.join(if cfg!(windows) {
            ".local/bin/claude.exe"
        } else {
            ".local/bin/claude"
        });
        if native.is_file() {
            return Ok(native);
        }
    }
    let path = which::which("claude").map_err(|_| {
        io::Error::other("Claude Code was not found. Install the Claude Code CLI, then try again.")
    })?;
    if cfg!(windows)
        && path
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("exe"))
    {
        return Err(io::Error::other(
            "Account sign-in requires the native Claude Code executable. Install Claude Code using its native Windows installer.",
        ));
    }
    Ok(path)
}

fn command(executable: &Path, dir: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env("CLAUDE_CONFIG_DIR", dir)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The login must use browser-based subscription auth, not inherited API auth.
    for key in AUTH_OVERRIDES {
        command.env_remove(key);
    }
    command.env_remove("CLAUDECODE");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

struct LoginDirectory {
    root: PathBuf,
    path: PathBuf,
}

impl Drop for LoginDirectory {
    fn drop(&mut self) {
        // Delete only the UUID directory we created, never a caller-supplied path.
        if let (Ok(root), Ok(path)) = (self.root.canonicalize(), self.path.canonicalize())
            && path.parent() == Some(root.as_path())
        {
            let _cleanup = std::fs::remove_dir_all(path);
        }
    }
}

pub fn login() -> io::Result<SavedLogin> {
    let exe = executable()?;
    let root = dirs::config_dir()
        .ok_or_else(|| io::Error::other("Configuration directory not found."))?
        .join("CodexBar/claude-accounts/logins");
    std::fs::create_dir_all(&root)?;
    let dir = LoginDirectory {
        path: root.join(uuid::Uuid::new_v4().to_string()),
        root,
    };
    std::fs::create_dir(&dir.path)?;
    let mut child = command(&exe, &dir.path)
        .args(["auth", "login", "--claudeai"])
        .spawn()?;
    wait_for_login(&mut child, &dir.path, &CANCEL, Duration::from_secs(300))
}

fn wait_for_login(
    child: &mut std::process::Child,
    dir: &Path,
    cancel: &AtomicBool,
    timeout: Duration,
) -> io::Result<SavedLogin> {
    let start = Instant::now();
    loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(e) => {
                let _kill = child.kill();
                let _reap = child.wait();
                return Err(e);
            }
        };
        if let Some(status) = status {
            if !status.success() {
                return Err(io::Error::other(
                    "Claude sign-in did not complete. Try again and finish sign-in in your browser.",
                ));
            }
            return read_login(dir, &dir.join(".claude.json"))?.ok_or_else(|| {
                io::Error::other(
                    "Claude Code did not save a subscription login. Please sign in again.",
                )
            });
        }
        let cancelled = cancel.load(Ordering::SeqCst);
        if cancelled || start.elapsed() > timeout {
            let _kill = child.kill();
            let _reap = child.wait();
            return Err(io::Error::other(if cancelled {
                "Claude sign-in cancelled."
            } else {
                "Claude sign-in timed out. Please try again."
            }));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn require_subscription_environment(
    get: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> io::Result<()> {
    for key in AUTH_OVERRIDES {
        if get(key).is_some_and(|v| !v.is_empty()) {
            return Err(io::Error::other(format!(
                "{key} overrides Claude subscription login. Unset it and restart Win-CodexBar before switching accounts."
            )));
        }
    }
    Ok(())
}

/// Existing CLI processes retain credentials in memory and may rotate them.
/// Require them to exit; account management never terminates user tasks.
pub fn require_cli_closed() -> io::Result<()> {
    require_subscription_environment(|key| std::env::var_os(key))?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let exe = executable()?.canonicalize()?;
        let output = Command::new("powershell.exe")
            .env(
                "CODEXBAR_CLAUDE_EXE",
                exe.to_string_lossy().trim_start_matches(r"\\?\"),
            )
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                CLI_PROCESS_COUNT_SCRIPT,
            ])
            .creation_flags(0x0800_0000)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "Could not check whether Claude Code is running.",
            ));
        }
        let count: usize = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| io::Error::other("Could not check whether Claude Code is running."))?;
        if count > 0 {
            return Err(io::Error::other(
                "Close your running Claude Code CLI sessions, then switch accounts and reopen Claude Code.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_override_blocks_switching_and_is_removed_from_login_children() {
        let descriptor = "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR";
        let error = require_subscription_environment(|key| {
            (key == descriptor).then(|| std::ffi::OsString::from("3"))
        })
        .unwrap_err();
        assert!(error.to_string().contains(descriptor));
        assert!(require_subscription_environment(|_| None).is_ok());
        assert!(require_subscription_environment(|_| Some(std::ffi::OsString::new())).is_ok());
        let command = command(Path::new("claude.exe"), Path::new("isolated"));
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == descriptor && value.is_none())
        );
    }

    #[cfg(windows)]
    #[test]
    fn process_guard_distinguishes_native_cli_from_store_desktop() {
        use std::os::windows::process::CommandExt;
        for (product, expected) in [("Claude", "0"), ("Claude Code", "1")] {
            let script = format!(
                "function Get-Process {{ [pscustomobject]@{{ ProcessName='claude'; Path='C:\\fixture\\claude.exe'; MainModule=[pscustomobject]@{{FileVersionInfo=[pscustomobject]@{{ProductName='{product}'}}}} }} }}; {CLI_PROCESS_COUNT_SCRIPT}"
            );
            let output = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .env("CODEXBAR_CLAUDE_EXE", "C:\\different\\claude.exe")
                .creation_flags(0x0800_0000)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
    }

    fn child(dir: &Path, sleep: bool, exit: u32) -> std::process::Child {
        #[cfg(windows)]
        let mut process = command(Path::new("powershell.exe"), dir);
        #[cfg(windows)]
        process.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "{}exit {exit}",
                if sleep {
                    "Start-Sleep -Seconds 30; "
                } else {
                    ""
                }
            ),
        ]);
        #[cfg(not(windows))]
        let mut process = command(Path::new("sh"), dir);
        #[cfg(not(windows))]
        process.args([
            "-c",
            &format!("{}exit {exit}", if sleep { "exec sleep 30; " } else { "" }),
        ]);
        process.spawn().unwrap()
    }

    #[test]
    fn cancelled_and_timed_out_logins_reap_child() {
        let dir = tempfile::tempdir().unwrap();
        for cancelled in [true, false] {
            let mut child = child(dir.path(), true, 0);
            let result = wait_for_login(
                &mut child,
                dir.path(),
                &AtomicBool::new(cancelled),
                Duration::from_millis(10),
            );
            assert!(result.is_err());
            assert!(child.try_wait().unwrap().is_some());
        }
    }

    #[test]
    fn successful_login_reads_only_the_isolated_directory_and_failed_exit_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"isolated","refreshToken":"refresh"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join(".claude.json"), r#"{"oauthAccount":{"accountUuid":"test","organizationUuid":"org","emailAddress":"test@example.com"}}"#).unwrap();
        let login = wait_for_login(
            &mut child(dir.path(), false, 0),
            dir.path(),
            &AtomicBool::new(false),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(login.id().unwrap(), "test:org");
        assert!(
            wait_for_login(
                &mut child(dir.path(), false, 1),
                dir.path(),
                &AtomicBool::new(false),
                Duration::from_secs(10)
            )
            .is_err()
        );
    }

    #[test]
    fn temporary_login_cleanup_cannot_remove_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("logins");
        let path = root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("secret.json"), "secret").unwrap();
        drop(LoginDirectory {
            root: root.clone(),
            path: path.clone(),
        });
        assert!(!path.exists());
        assert!(root.exists());
        drop(LoginDirectory {
            root: root.clone(),
            path: root.clone(),
        });
        assert!(root.exists());
    }
}

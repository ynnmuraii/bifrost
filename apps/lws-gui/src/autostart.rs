//! Per-user login entry, always referring to the executable the user is running.
use std::io;

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn reg() -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new("reg.exe");
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    command
}

/// Whether the operating system has this user's Bifrost login entry.
pub fn enabled() -> bool {
    #[cfg(windows)]
    {
        reg()
            .args(["query", RUN_KEY, "/v", "Bifrost"])
            .output()
            .is_ok_and(|output| output.status.success())
    }
    #[cfg(not(windows))]
    {
        entry().is_ok_and(|path| path.is_file())
    }
}

/// Enable login startup using the current executable's absolute path, or remove it.
pub fn set_enabled(enable: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        if !enable && !enabled() {
            return Ok(());
        }
        let mut command = reg();
        if enable {
            let exe = std::env::current_exe()?;
            command.args(["add", RUN_KEY, "/v", "Bifrost", "/t", "REG_SZ", "/d"]);
            command.arg(format!("\"{}\"", exe.display())).arg("/f");
        } else {
            command.args(["delete", RUN_KEY, "/v", "Bifrost", "/f"]);
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&output.stderr)));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let path = entry()?;
        if enable {
            std::fs::create_dir_all(path.parent().expect("autostart entry has a parent"))?;
            std::fs::write(path, desktop_entry(&login_executable()?))
        } else {
            match std::fs::remove_file(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            }
        }
    }
}

#[cfg(not(windows))]
fn entry() -> io::Result<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(base.join("autostart/bifrost.desktop"))
}

#[cfg(not(windows))]
fn desktop_entry(exe: &std::path::Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Bifrost\nExec=\"{}\"\nIcon=bifrost\nTerminal=false\nStartupWMClass=bifrost\n",
        exec_argument(exe)
    )
}

/// `exe` as the value of an `Exec` argument, escaped as the Desktop Entry Specification
/// requires (and as `packaging/install.sh` writes it).
///
/// Two rules apply, quoted rule last when writing: inside a double-quoted `Exec` argument a
/// literal `"`, `` ` ``, `$` and `\` carry a preceding backslash, and the whole value is then
/// a *string*, where a backslash is itself written as `\\`. So a literal backslash becomes
/// four characters (`\\\\`) and a literal `"` becomes `\\"`. `%` is doubled on top: a single
/// `%` would otherwise start a field code (`%f`, `%U`), and `%%` is a literal percent.
///
/// A path with no special character is written as it is, spaces included: the surrounding
/// double quotes are what makes the whole path one argument.
#[cfg(not(windows))]
fn exec_argument(exe: &std::path::Path) -> String {
    let mut escaped = String::new();
    for c in exe.to_string_lossy().chars() {
        match c {
            '%' => escaped.push_str("%%"),
            '\\' => escaped.push_str("\\\\\\\\"),
            '"' | '$' | '`' => {
                escaped.push_str("\\\\");
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

/// The executable the login entry names: the running binary with symlinks and `..` resolved,
/// falling back to the path as it was when it cannot be canonicalized.
#[cfg(not(windows))]
fn login_executable() -> io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(canonical_or_raw(&exe))
}

/// `path` with symlinks and `..` resolved; `path` itself when the filesystem refuses (a path
/// inside a mount that has gone away, or one the user cannot traverse).
#[cfg(not(windows))]
fn canonical_or_raw(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{canonical_or_raw, desktop_entry, exec_argument};
    use std::path::Path;

    #[test]
    fn login_entry_quotes_the_actual_executable() {
        let text = super::desktop_entry(Path::new("/home/user/My Apps/100%/bifrost"));
        assert!(text.contains("Exec=\"/home/user/My Apps/100%%/bifrost\"\n"));
        assert!(text.contains("\nStartupWMClass=bifrost\n"), "{text}");
        assert!(text.ends_with('\n'));
    }

    /// `packaging/install.sh`'s `sed` line, ported: the two spellings have to agree, because
    /// the same program is started by both entries and a divergence is a path one of them
    /// cannot find.
    fn installer_escaped(value: &str) -> String {
        let mut out = String::new();
        for c in value.chars() {
            match c {
                '\\' => out.push_str("\\\\\\\\"),
                '"' => out.push_str("\\\\\""),
                '$' => out.push_str("\\\\$"),
                '`' => out.push_str("\\\\`"),
                '%' => out.push_str("%%"),
                _ => out.push(c),
            }
        }
        out
    }

    #[test]
    fn the_login_entry_and_the_installer_escape_a_path_the_same_way() {
        for path in [
            "/home/user/My Apps/bifrost",
            "/opt/a$b/bifrost",
            "/opt/a\"b/bifrost",
            "/opt/a`b/bifrost",
            "/opt/a\\b/bifrost",
            "/opt/100%/bifrost",
            "/home/Пользователь/Мои программы/bifrost",
            "/opt/space and $dollar\"quote`tick\\slash%/bifrost",
        ] {
            let entry = desktop_entry(Path::new(path));
            assert!(
                entry.contains(&format!("Exec=\"{}\"\n", installer_escaped(path))),
                "{path:?} escapes differently from install.sh: {entry}"
            );
            assert_eq!(exec_argument(Path::new(path)), installer_escaped(path));
        }
    }

    #[test]
    fn a_path_that_cannot_be_canonicalized_is_kept_as_written() {
        let missing = Path::new("/nonexistent/bifrost-\u{0}unreachable/bifrost");
        assert_eq!(canonical_or_raw(missing), missing);

        let real = std::env::temp_dir();
        let canonical = canonical_or_raw(&real);
        assert_eq!(canonical, std::fs::canonicalize(&real).unwrap());
        assert!(canonical.is_absolute());
    }
}

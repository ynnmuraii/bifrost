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
            std::fs::write(path, desktop_entry(&std::env::current_exe()?))
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
    format!("[Desktop Entry]\nType=Application\nName=Bifrost\nExec=\"{escaped}\"\nIcon=bifrost\nTerminal=false\n")
}

#[cfg(all(test, not(windows)))]
mod tests {
    #[test]
    fn login_entry_quotes_the_actual_executable() {
        let text = super::desktop_entry(std::path::Path::new("/home/user/My Apps/100%/bifrost"));
        assert!(text.contains("Exec=\"/home/user/My Apps/100%%/bifrost\"\n"));
    }
}

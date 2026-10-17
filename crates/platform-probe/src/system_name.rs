//! Short OS names for non-authoritative workspace labels.

/// Reads the operating system's own product description; never host or hardware data.
pub fn detect() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/etc/os-release")
            .map(|contents| parse_os_release(&contents))
            .unwrap_or_else(|_| "Linux".into())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("reg.exe")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            ])
            .creation_flags(0x0800_0000)
            .output();
        let text = output
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        windows_from_registry(&text)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        std::env::consts::OS.to_owned()
    }
}

/// The name Windows gives itself, read out of the `reg query` dump of
/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`.
///
/// Only the three values matter; the dump also carries `CurrentBuildNumber`, whose line
/// must not be mistaken for `CurrentBuild`, which is why a line only counts when its
/// value follows the `REG_SZ` type marker.
pub fn windows_from_registry(text: &str) -> String {
    let value = |name: &str| {
        text.lines()
            .find_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix(name)?.trim_start();
                rest.strip_prefix("REG_SZ").map(str::trim)
            })
            .unwrap_or_default()
    };
    windows_name(
        value("ProductName"),
        value("DisplayVersion"),
        value("CurrentBuild").parse().unwrap_or_default(),
    )
}

/// Parses the small quoted assignments of os-release without interpreting shell code.
pub fn parse_os_release(contents: &str) -> String {
    let value = |key: &str| {
        contents
            .lines()
            .find_map(|line| {
                let (name, value) = line.trim().split_once('=')?;
                if name != key {
                    return None;
                }
                let value = value.trim();
                Some(
                    value
                        .strip_prefix('"')
                        .and_then(|value| value.strip_suffix('"'))
                        .or_else(|| {
                            value
                                .strip_prefix('\'')
                                .and_then(|value| value.strip_suffix('\''))
                        })
                        .unwrap_or(value)
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\"),
                )
            })
            .unwrap_or_default()
    };
    let pretty = value("PRETTY_NAME");
    let name = value("NAME");
    let version = value("VERSION_ID");
    let raw = if pretty.is_empty() {
        format!(
            "{} {}",
            if name.is_empty() { "Linux" } else { &name },
            version
        )
    } else {
        pretty
    };
    // Marketing support suffixes and distro codenames add length, not identity.
    let raw = raw.split(" (").next().unwrap_or(&raw).replace(" LTS", "");
    let words: Vec<_> = raw
        .split_whitespace()
        .map(|word| {
            let parts: Vec<_> = word.split('.').collect();
            if parts.len() > 2
                && parts
                    .iter()
                    .all(|part| part.chars().all(|ch| ch.is_ascii_digit()))
            {
                parts[..2].join(".")
            } else {
                word.to_owned()
            }
        })
        .collect();
    words.join(" ")
}

/// Windows 11 retains a Windows 10 ProductName in the registry on some systems.
pub fn windows_name(product: &str, version: &str, build: u32) -> String {
    let product = if product.is_empty() {
        "Windows"
    } else {
        product
    };
    let product = if build >= 22000 {
        product.replace("Windows 10", "Windows 11")
    } else {
        product.to_owned()
    };
    let product = product
        .replace(" Pro", "")
        .replace(" Home", "")
        .replace(" Enterprise", "")
        .replace(" Education", "");
    if version.is_empty() {
        product
    } else {
        format!("{product} {version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distro_samples_and_missing_fields() {
        for (input, expected) in [
            ("PRETTY_NAME=\"Ubuntu 24.04.4 LTS\"", "Ubuntu 24.04"),
            ("PRETTY_NAME=\"CachyOS\"", "CachyOS"),
            ("NAME=\"Arch Linux\"", "Arch Linux"),
            (
                "PRETTY_NAME=\"Fedora Linux 44 (Workstation Edition)\"",
                "Fedora Linux 44",
            ),
            ("NAME=Ubuntu\nVERSION_ID='24.04'", "Ubuntu 24.04"),
            ("PRETTY_NAME='Pop!_OS 22.04 LTS'", "Pop!_OS 22.04"),
            // A release whose PRETTY_NAME is missing falls back to NAME and VERSION_ID, and
            // an os-release without even those is still a name, never an empty label.
            (
                "NAME=\"Debian GNU/Linux\"\nVERSION=\"12 (bookworm)\"",
                "Debian GNU/Linux",
            ),
            (
                "NAME=\"Alpine Linux\"\nVERSION_ID=3.20",
                "Alpine Linux 3.20",
            ),
            ("# a comment\n", "Linux"),
            ("", "Linux"),
            (
                "PRETTY_NAME=\"Ubuntu 24.04.4 LTS\"\nNAME=Ubuntu",
                "Ubuntu 24.04",
            ),
        ] {
            assert_eq!(parse_os_release(input), expected);
        }
    }

    #[test]
    fn escaped_values_stay_literal_and_a_value_never_swallows_the_next_line() {
        // os-release values are quoted shell assignments, not shell code: an escape stays an
        // escape, and one line never swallows the next.
        assert_eq!(
            parse_os_release("PRETTY_NAME=\"openSUSE Leap 15.6\"\nID=opensuse-leap"),
            "openSUSE Leap 15.6"
        );
        assert_eq!(
            parse_os_release("PRETTY_NAME=\"A \\\"quoted\\\" name\"\nNAME=Other"),
            "A \"quoted\" name"
        );
    }
    #[test]
    fn windows_build_boundary_corrects_the_product_name() {
        assert_eq!(
            windows_name("Windows 10 Pro", "22H2", 19045),
            "Windows 10 22H2"
        );
        assert_eq!(
            windows_name("Windows 10 Pro", "24H2", 22000),
            "Windows 11 24H2"
        );
        assert_eq!(windows_name("Windows 11 Home", "", 26100), "Windows 11");
    }

    #[test]
    fn the_registry_dump_is_read_as_windows_names_itself() {
        // The real dump of a Windows 11 25H2 machine: ProductName still says Windows 10,
        // and `CurrentBuildNumber` must not answer for `CurrentBuild`.
        let dump = "HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\r\n\
    CurrentBuild    REG_SZ    26200\r\n\
    CurrentBuildNumber    REG_SZ    26200\r\n\
    CurrentVersion    REG_SZ    6.3\r\n\
    DisplayVersion    REG_SZ    25H2\r\n\
    ProductName    REG_SZ    Windows 10 Pro\r\n\
    ReleaseId    REG_SZ    2009\r\n\
    HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Winlogon\r\n";
        assert_eq!(windows_from_registry(dump), "Windows 11 25H2");
        // A dump Windows refused to produce, or a key without the values, is still a name.
        assert_eq!(windows_from_registry(""), "Windows");
        assert_eq!(
            windows_from_registry("    ProductName    REG_SZ    Windows 11 Home    \n"),
            "Windows 11"
        );
    }
}

//! The global path flags every role of `bifrost` takes: `--root`, `--config-dir`,
//! `--data-dir`, `--state-dir`, `--runtime-dir`.
//!
//! One parser, because three roles start from the same command line and must agree on
//! what it names: the window parses its own `argv`, the tray is started with the window's
//! arguments forwarded as they were, and the core is started by the window with the flags
//! [`CliPaths::flags`] prints. Before this module the tray accepted `--root=/path`, the
//! window and the core only `--root /path`, so `bifrost --root=/path` put the tray's IPC
//! endpoint under `/path` and the window's under the default one — two processes, two
//! different directories, one silent failure.
//!
//! Both spellings are accepted everywhere now, `--flag value` and `--flag=value`, and the
//! last occurrence of a flag wins (a forwarded set never repeats one, so this only decides
//! what a hand-typed duplicate means).
//!
//! What an argument that is *not* a path flag means stays the role's own business: the
//! window and the tray ignore it, the core reports an unknown option (see
//! [`CliPaths::take`], which hands back the index of the next argument instead of deciding).

use std::path::PathBuf;

use crate::paths::{default_paths, Paths};

/// The global flags, in the order [`CliPaths::flags`] forwards them.
pub const PATH_FLAGS: [&str; 5] = [
    "--root",
    "--config-dir",
    "--data-dir",
    "--state-dir",
    "--runtime-dir",
];

/// The global path options of one command line, exactly as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliPaths {
    /// `--root DIR`: everything the other flags do not name goes under `DIR`.
    pub root: Option<PathBuf>,
    /// `--config-dir DIR`.
    pub config_dir: Option<PathBuf>,
    /// `--data-dir DIR`.
    pub data_dir: Option<PathBuf>,
    /// `--state-dir DIR`.
    pub state_dir: Option<PathBuf>,
    /// `--runtime-dir DIR`, which has no default on every platform.
    pub runtime_dir: Option<PathBuf>,
}

/// What [`CliPaths::take`] found at one argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// The argument is not one of [`PATH_FLAGS`]; the caller decides what it means.
    No,
    /// A flag and its value were read; `usize` is the index of the next unread argument.
    Yes(usize),
    /// The named flag is present and has no value.
    MissingValue(&'static str),
}

impl CliPaths {
    /// Reads every global flag in `args`, ignoring the arguments that are not one.
    ///
    /// The window and the tray use this: an argument they do not know is not a reason to
    /// refuse to start (`--updated`, a desktop environment's own argument).
    pub fn parse<T: AsRef<str>>(args: impl IntoIterator<Item = T>) -> Self {
        let args: Vec<String> = args
            .into_iter()
            .map(|argument| argument.as_ref().to_owned())
            .collect();
        let mut paths = CliPaths::default();
        let mut index = 0;
        while index < args.len() {
            match paths.take(&args, index) {
                Taken::Yes(next) => index = next,
                Taken::No | Taken::MissingValue(_) => index += 1,
            }
        }
        paths
    }

    /// Reads the global flag at `args[index]`, if that is what it is.
    ///
    /// The core walks its command line with this so that its own rules — `--help`, an
    /// unknown option is an error, the first other argument is the command — stay its own.
    pub fn take(&mut self, args: &[String], index: usize) -> Taken {
        let Some(argument) = args.get(index) else {
            return Taken::No;
        };
        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (argument.as_str(), None),
        };
        let (slot, name) = match flag {
            "--root" => (&mut self.root, "--root"),
            "--config-dir" => (&mut self.config_dir, "--config-dir"),
            "--data-dir" => (&mut self.data_dir, "--data-dir"),
            "--state-dir" => (&mut self.state_dir, "--state-dir"),
            "--runtime-dir" => (&mut self.runtime_dir, "--runtime-dir"),
            _ => return Taken::No,
        };
        match inline {
            Some(value) => {
                *slot = Some(PathBuf::from(value));
                Taken::Yes(index + 1)
            }
            None => match args.get(index + 1) {
                Some(value) => {
                    *slot = Some(PathBuf::from(value));
                    Taken::Yes(index + 2)
                }
                None => Taken::MissingValue(name),
            },
        }
    }

    /// These options as the separated `--flag value` pairs a child process is started with.
    pub fn flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        for (name, value) in [
            ("--root", &self.root),
            ("--config-dir", &self.config_dir),
            ("--data-dir", &self.data_dir),
            ("--state-dir", &self.state_dir),
            ("--runtime-dir", &self.runtime_dir),
        ] {
            if let Some(value) = value {
                flags.push(name.to_owned());
                flags.push(value.display().to_string());
            }
        }
        flags
    }

    /// The documented defaults, with every flag on top: an explicit directory wins, then
    /// the same name under `--root`, then the platform default.
    pub fn resolve(&self) -> Result<Paths, String> {
        let defaults = default_paths().map_err(|error| error.to_string())?;
        let under_root = |name: &str| self.root.as_ref().map(|root| root.join(name));
        Ok(Paths::new(
            self.config_dir
                .clone()
                .or_else(|| under_root("config"))
                .unwrap_or_else(|| defaults.config_dir().to_path_buf()),
            self.data_dir
                .clone()
                .or_else(|| under_root("data"))
                .unwrap_or_else(|| defaults.data_dir().to_path_buf()),
            self.state_dir
                .clone()
                .or_else(|| under_root("state"))
                .unwrap_or_else(|| defaults.state_dir().to_path_buf()),
            self.runtime_dir
                .clone()
                .or_else(|| under_root("run"))
                .or_else(|| defaults.runtime_dir().map(std::path::Path::to_path_buf)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_that_looks_like_a_flag_is_still_the_value() {
        // The separated form cannot tell a value from a flag, and must not try: `--root
        // --data-dir` names a directory called `--data-dir`, exactly as every other
        // command-line parser reads it.
        let mut paths = CliPaths::default();
        assert_eq!(
            paths.take(&["--root".to_owned(), "--data-dir".to_owned()], 0),
            Taken::Yes(2)
        );
        assert_eq!(paths.root, Some(PathBuf::from("--data-dir")));
        assert_eq!(paths.data_dir, None);
    }
}

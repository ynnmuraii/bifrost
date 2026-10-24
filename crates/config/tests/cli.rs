//! The one parser behind the global path flags of every role.

use config::{CliPaths, Taken};
use std::path::PathBuf;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn both_forms_name_the_same_directories() {
    let separated = CliPaths::parse(args(&[
        "--root",
        "/work",
        "--config-dir",
        "/work/config",
        "--data-dir",
        "/work/data",
        "--state-dir",
        "/work/state",
        "--runtime-dir",
        "/work/run",
    ]));
    let inline = CliPaths::parse(args(&[
        "--root=/work",
        "--config-dir=/work/config",
        "--data-dir=/work/data",
        "--state-dir=/work/state",
        "--runtime-dir=/work/run",
    ]));
    assert_eq!(separated, inline);
    assert_eq!(separated.root, Some(PathBuf::from("/work")));
    assert_eq!(separated.config_dir, Some(PathBuf::from("/work/config")));
    assert_eq!(separated.data_dir, Some(PathBuf::from("/work/data")));
    assert_eq!(separated.state_dir, Some(PathBuf::from("/work/state")));
    assert_eq!(separated.runtime_dir, Some(PathBuf::from("/work/run")));

    // A path with spaces and an `=`, the reason the inline form has to split on the
    // first `=` only.
    let awkward = CliPaths::parse(args(&["--root=/home/My Work/a=b"]));
    assert_eq!(awkward.root, Some(PathBuf::from("/home/My Work/a=b")));
}

#[test]
fn a_flag_without_a_value_is_reported_and_left_unset() {
    assert!(matches!(
        CliPaths::default().take(&args(&["--root"]), 0),
        Taken::MissingValue("--root")
    ));
    // GUI and tray ignore it rather than inventing a directory.
    let ignored = CliPaths::parse(args(&["--root"]));
    assert_eq!(ignored, CliPaths::default());
    // `--root=` is not missing: the value is the empty path, as written.
    let empty = CliPaths::parse(args(&["--root="]));
    assert_eq!(empty.root, Some(PathBuf::from("")));
}

#[test]
fn the_last_occurrence_wins() {
    let parsed = CliPaths::parse(args(&["--root", "/first", "--root=/second"]));
    assert_eq!(parsed.root, Some(PathBuf::from("/second")));
}

#[test]
fn arguments_that_are_not_path_flags_are_left_alone() {
    assert!(matches!(
        CliPaths::default().take(&args(&["run"]), 0),
        Taken::No
    ));
    assert!(matches!(
        CliPaths::default().take(&args(&["--rootx=/y"]), 0),
        Taken::No
    ));
    assert!(matches!(
        CliPaths::default().take(&args(&["--help"]), 0),
        Taken::No
    ));
    // The value of a `--root` is consumed even when it looks like a flag: skipping it is
    // the role's business, not the parser's.
    let mut paths = CliPaths::default();
    assert!(matches!(
        paths.take(&args(&["--root", "--data-dir", "/d"]), 0),
        Taken::Yes(2)
    ));
    assert_eq!(paths.root, Some(PathBuf::from("--data-dir")));
}

#[test]
fn forwarded_flags_parse_back_to_the_same_options() {
    let parsed = CliPaths::parse(args(&[
        "--root=/work",
        "--data-dir",
        "/elsewhere",
        "gui",
        "--updated",
    ]));
    let forwarded = parsed.flags();
    assert_eq!(
        forwarded,
        args(&["--root", "/work", "--data-dir", "/elsewhere"])
    );
    // What the window forwards to the tray/core names the same directories there.
    assert_eq!(CliPaths::parse(&forwarded), parsed);
    assert_eq!(CliPaths::parse(&forwarded).resolve(), parsed.resolve());
}

#[test]
fn a_root_places_every_directory_under_it() {
    let paths = CliPaths::parse(args(&["--root=/work"]))
        .resolve()
        .expect("the defaults resolve");
    assert_eq!(paths.config_dir(), PathBuf::from("/work/config").as_path());
    assert_eq!(paths.data_dir(), PathBuf::from("/work/data").as_path());
    assert_eq!(paths.state_dir(), PathBuf::from("/work/state").as_path());
    assert_eq!(
        paths.runtime_dir(),
        Some(PathBuf::from("/work/run").as_path())
    );

    // An explicit directory beats the root.
    let explicit = CliPaths::parse(args(&["--root=/work", "--config-dir=/etc/bifrost"]))
        .resolve()
        .expect("the defaults resolve");
    assert_eq!(
        explicit.config_dir(),
        PathBuf::from("/etc/bifrost").as_path()
    );
    assert_eq!(explicit.data_dir(), PathBuf::from("/work/data").as_path());
}

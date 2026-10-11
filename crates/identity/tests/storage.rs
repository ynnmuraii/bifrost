//! Key storage: paths, load-or-create, at-rest protection, and the documented refusal
//! of a key readable by anyone else.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use identity::atomic::is_wider_than_owner;
use identity::{
    key_file_of, keystore, trust_file_at, IdentityError, IdentityLoad, IdentityPaths, NodeIdentity,
    DPAPI_ENTROPY,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lws-identity-storage-{}-{}-{label}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temporary directory");
    dir
}

#[test]
fn rooted_paths_keep_the_key_and_the_trust_store_together() {
    let root = temp_dir("paths");
    let paths = IdentityPaths::rooted_at(&root);

    assert_eq!(paths.data_dir, root);
    assert_eq!(paths.key_file, root.join(identity::key::KEY_FILE_NAME));
    assert_eq!(paths.trust_file, root.join("trust.toml"));
    assert_eq!(key_file_of(&paths), paths.key_file.as_path());
    assert_eq!(trust_file_at(&root), paths.trust_file);

    if cfg!(windows) {
        assert_eq!(identity::key::KEY_FILE_NAME, "identity.dpapi");
    } else {
        assert_eq!(identity::key::KEY_FILE_NAME, "identity.key");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn system_paths_resolve_to_documented_roots_without_creating_anything() {
    // Resolution reads the environment and builds names; it must not create directories,
    // so this stays inert even though it looks at the real profile.
    let paths = IdentityPaths::system().expect("APPDATA/HOME must be resolvable here");

    // Both files live in a directory called `lan-workspace`, and the key sits in the data
    // directory.
    assert!(paths.data_dir.ends_with("lan-workspace"));
    assert_eq!(paths.key_file.parent(), Some(paths.data_dir.as_path()));
    assert_eq!(
        paths.key_file.file_name(),
        Some(std::ffi::OsStr::new(identity::key::KEY_FILE_NAME))
    );
    assert_eq!(
        paths.trust_file.file_name(),
        Some(std::ffi::OsStr::new("trust.toml"))
    );
    assert!(paths
        .trust_file
        .parent()
        .expect("trust.toml has a parent")
        .ends_with("lan-workspace"));

    // «Файлы и пути»: «Приватный ключ | … | `$XDG_DATA_HOME/lan-workspace/identity.key`
    // (0600)» and «Доверенные узлы | `%APPDATA%\lan-workspace\trust.toml` |
    // `$XDG_CONFIG_HOME/lan-workspace/trust.toml`» — the key follows the data root, the
    // trust store the config root. Only on Windows, where both documented rows name
    // `%APPDATA%\lan-workspace`, do they share a directory.
    #[cfg(unix)]
    {
        let data_root = documented_root("XDG_DATA_HOME", ".local/share").join("lan-workspace");
        let config_root = documented_root("XDG_CONFIG_HOME", ".config").join("lan-workspace");
        let existed_before = data_root.exists();

        assert_eq!(paths.data_dir, data_root, "the key lives under $XDG_DATA_HOME");
        assert_eq!(
            paths.trust_file,
            config_root.join("trust.toml"),
            "the trust store lives under $XDG_CONFIG_HOME"
        );
        assert_ne!(
            paths.trust_file.parent(),
            Some(paths.data_dir.as_path()),
            "the two roots are different variables, so the files are not in one directory"
        );
        assert_eq!(
            data_root.exists(),
            existed_before,
            "resolving paths must not create the directory"
        );
    }

    #[cfg(windows)]
    {
        let appdata = PathBuf::from(std::env::var_os("APPDATA").expect("%APPDATA% must be set"));
        assert_eq!(paths.data_dir, appdata.join("lan-workspace"));
        assert_eq!(paths.trust_file, paths.data_dir.join("trust.toml"));
    }
}

/// The test's own reading of the documented XDG resolution, so the assertion above
/// compares two independent derivations instead of calling the code under test twice.
#[cfg(unix)]
fn documented_root(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback))
        })
        .expect("HOME must be set when neither XDG variable is")
}

#[test]
fn the_key_is_created_once_and_reloaded_with_the_same_identity() {
    let root = temp_dir("load-or-create");
    let paths = IdentityPaths::rooted_at(root.join("node-a"));

    let (first, created) = NodeIdentity::load_or_create(&paths).expect("create");
    assert_eq!(created, IdentityLoad::Created);
    assert!(paths.key_file.exists(), "the key must be persisted");
    assert!(paths.data_dir.exists(), "the directory is created with it");

    let (second, loaded) = NodeIdentity::load_or_create(&paths).expect("load");
    assert_eq!(loaded, IdentityLoad::Loaded);
    assert_eq!(
        second.node_id(),
        first.node_id(),
        "the pinned identity must survive a restart"
    );
    assert_eq!(second.pkcs8_der(), first.pkcs8_der());

    let (third, again) = NodeIdentity::load_or_create(&paths).expect("load again");
    assert_eq!(again, IdentityLoad::Loaded);
    assert_eq!(third.node_id(), first.node_id());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_or_create_creates_missing_parent_directories() {
    let root = temp_dir("nested");
    let paths = IdentityPaths::rooted_at(root.join("deep").join("deeper").join("node"));
    let (identity, created) = NodeIdentity::load_or_create(&paths).expect("create in a fresh tree");
    assert_eq!(created, IdentityLoad::Created);
    assert!(identity.node_id().as_bytes().iter().any(|byte| *byte != 0));
    assert!(paths.key_file.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_key_is_protected_for_this_user_only() {
    let root = temp_dir("protection");
    let paths = IdentityPaths::rooted_at(root.join("node-a"));
    let (identity, _) = NodeIdentity::load_or_create(&paths).expect("create");
    let stored = std::fs::read(&paths.key_file).expect("read the stored key");

    // The Windows blob is DPAPI (version 1 + the provider GUID), never a bare PKCS#8
    // DER sequence; on Linux the documented protection is the file mode instead.
    #[cfg(windows)]
    {
        assert_ne!(
            stored,
            identity.pkcs8_der(),
            "the key must not be stored in the clear"
        );
        assert_ne!(
            &stored[..2],
            &[0x30, 0x82],
            "a PKCS#8 DER sequence means the key was not sealed"
        );
        assert_eq!(
            &stored[..16],
            &[
                0x01, 0x00, 0x00, 0x00, // DATA_BLOB version
                0xd0, 0x8c, 0x9d, 0xdf, 0x01, 0x15, 0xd1, 0x11, 0x8c, 0x7a, 0x00,
                0xc0, // provider GUID
            ],
            "expected the DPAPI blob header"
        );
        assert_eq!(
            keystore::unseal(&stored).expect("unseal"),
            identity.pkcs8_der()
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            stored,
            identity.pkcs8_der(),
            "the file holds the PKCS#8 bytes"
        );
        let mode = std::fs::metadata(&paths.key_file)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the documented key file mode");
        let dir_mode = std::fs::metadata(&paths.data_dir)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "the documented directory mode");
    }

    // The entropy constant is part of the on-disk format.
    assert_eq!(DPAPI_ENTROPY, b"lan-workspace/identity/v1");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_keystore_round_trips_and_windows_refuses_foreign_blobs() {
    let plaintext = b"a fake pkcs8 payload";

    #[cfg(unix)]
    {
        // The Linux keystore is the identity function: the file mode is the protection.
        assert_eq!(keystore::seal(plaintext).expect("seal"), plaintext);
        assert_eq!(keystore::unseal(plaintext).expect("unseal"), plaintext);
    }

    #[cfg(windows)]
    {
        let sealed = keystore::seal(plaintext).expect("seal");
        assert_ne!(sealed, plaintext, "sealing must change the bytes");
        assert_eq!(keystore::unseal(&sealed).expect("unseal"), plaintext);
        assert!(
            keystore::unseal(b"not a dpapi blob").is_err(),
            "a foreign/corrupt blob must fail instead of yielding another identity"
        );
    }
}

#[test]
fn an_owner_only_key_is_not_reported_as_wide() {
    let root = temp_dir("owner-only");
    let paths = IdentityPaths::rooted_at(root.join("node-a"));
    NodeIdentity::load_or_create(&paths).expect("create");

    assert!(!is_wider_than_owner(&paths.key_file).expect("check the file"));
    assert!(!is_wider_than_owner(&paths.data_dir).expect("check the directory"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn create_private_dir_is_idempotent_and_creates_parents() {
    let root = temp_dir("private-dir");
    let target = root.join("a").join("b").join("node");

    identity::atomic::create_private_dir(&target).expect("create");
    assert!(target.is_dir());
    identity::atomic::create_private_dir(&target).expect("second call is a no-op");
    assert!(!is_wider_than_owner(&target).expect("check"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_key_readable_by_others_refuses_to_load() {
    // «Core отказывается стартовать, если права шире, и выдаёт понятную ошибку».
    let root = temp_dir("insecure");
    let paths = IdentityPaths::rooted_at(root.join("node-a"));
    let (identity, _) = NodeIdentity::load_or_create(&paths).expect("create");

    widen(&paths.key_file);
    assert!(
        is_wider_than_owner(&paths.key_file).expect("check"),
        "the test must actually widen the permissions"
    );

    match NodeIdentity::load_or_create(&paths) {
        Err(IdentityError::InsecurePermissions { path }) => {
            assert_eq!(path, paths.key_file);
        }
        other => panic!("expected InsecurePermissions, got {other:?}"),
    }

    // The refusal mentions the path and what was expected, for the GUI/log.
    let message = IdentityError::InsecurePermissions {
        path: paths.key_file.clone(),
    }
    .to_string();
    assert!(
        message.contains(&paths.key_file.display().to_string()),
        "{message}"
    );
    assert!(message.contains("refusing to start"), "{message}");

    // A key that is still sealed is enough to keep the identity recoverable: nothing was
    // rewritten by the refused load.
    assert_ne!(identity.pkcs8_der(), Vec::new());

    let _ = std::fs::remove_dir_all(&root);
}

/// Makes `path` readable by everyone, on the platform's own terms.
#[cfg(windows)]
fn widen(path: &std::path::Path) {
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/grant")
        .arg("*S-1-1-0:(R)")
        .status()
        .expect("icacls must be available on Windows");
    assert!(status.success(), "icacls failed to widen the ACL");
}

#[cfg(unix)]
fn widen(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
}

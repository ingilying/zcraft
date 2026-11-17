use std::{
    fs,
    io::ErrorKind,
    process::Command,
    sync::{Arc, Barrier},
    time::Duration,
};
use zcraft::config::Config;

#[test]
fn missing_file_generates_roundtrippable_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zcraft.toml");
    let config = Config::load_or_create(&path).unwrap();
    assert_eq!(config, Config::default());
    let contents = fs::read_to_string(&path).unwrap();
    assert!(contents.contains("io_timeout_seconds = 10"));
    assert_eq!(Config::load_or_create(&path).unwrap(), config);
    assert_eq!(fs::read_to_string(path).unwrap(), contents);
}

#[test]
fn partial_config_preserves_comments_and_uses_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("custom.toml");
    let contents =
        "# keep this comment\nmotd = '自定义服务器'\nmax_players = 42\nio_timeout_seconds = 5\n";
    fs::write(&path, contents).unwrap();
    let config = Config::load_or_create(&path).unwrap();
    assert_eq!(config.motd, "自定义服务器");
    assert_eq!(config.max_players, 42);
    assert_eq!(config.max_connections, 1024);
    assert_eq!(config.io_timeout, Duration::from_secs(5));
    assert_eq!(fs::read_to_string(path).unwrap(), contents);
}

#[test]
fn invalid_files_are_reported_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.toml");
    for contents in [
        "broken = [",
        "max_conections = 10",
        "max_connections = 0",
        "max_connections = -1",
        "io_timeout_seconds = 0",
        "io_timeout_seconds = 86401",
        "io_timeout_seconds = 1.5",
        "max_players = 'twenty'",
        "bind = 'not an address'",
    ] {
        fs::write(&path, contents).unwrap();
        let error = Config::load_or_create(&path).unwrap_err();
        assert!(error.to_string().contains("invalid.toml"));
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }
}

#[test]
fn io_errors_do_not_trigger_default_fallback() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Config::load_or_create(dir.path()).is_err());
    let missing_parent = dir.path().join("absent/zcraft.toml");
    assert_eq!(
        Config::load_or_create(&missing_parent).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert!(!missing_parent.exists());
}

#[test]
fn overrides_are_validated_without_changing_file_or_partial_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zcraft.toml");
    let mut config = Config::load_or_create(&path).unwrap();
    let contents = fs::read_to_string(&path).unwrap();
    config
        .apply_overrides(Some("[::1]:25566".into()), Some("override".into()))
        .unwrap();
    assert_eq!(config.bind, "[::1]:25566");
    assert_eq!(config.motd, "override");
    assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    let before = config.clone();
    assert!(
        config
            .apply_overrides(Some("invalid".into()), Some("changed".into()))
            .is_err()
    );
    assert_eq!(config, before);
}

#[test]
fn excessive_connection_limit_is_rejected_before_semaphore_creation() {
    let config = Config {
        max_connections: usize::MAX,
        ..Config::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn concurrent_first_loads_publish_a_complete_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zcraft.toml");
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Config::load_or_create(path).unwrap()
            })
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap(), Config::default());
    }
    assert_eq!(Config::load_or_create(path).unwrap(), Config::default());
}

#[test]
fn binary_creates_default_config_before_validating_environment_override() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alternate.toml");
    // An invalid bind makes startup exit before opening a listener.
    let output = Command::new(env!("CARGO_BIN_EXE_zcraft"))
        .current_dir(dir.path())
        .env("ZCRAFT_CONFIG", &path)
        .env("ZCRAFT_BIND", "invalid")
        .env_remove("ZCRAFT_MOTD")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bind must be an IP:port"));
    assert_eq!(Config::load_or_create(path).unwrap(), Config::default());
    assert!(!dir.path().join("zcraft.toml").exists());
}

#[cfg(unix)]
#[test]
fn dangling_symlink_is_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zcraft.toml");
    std::os::unix::fs::symlink("missing.toml", &path).unwrap();
    assert!(Config::load_or_create(&path).is_err());
    assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
}

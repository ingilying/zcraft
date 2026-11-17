use std::{
    fs,
    io::ErrorKind,
    net::TcpListener,
    process::Command,
    sync::{Arc, Barrier},
};
use zcraft::eula::require_acceptance;

#[test]
fn first_start_creates_refusal_and_requires_manual_consent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("eula.txt");
    let error = require_acceptance(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("eula=true"));
    let original = fs::read_to_string(&path).unwrap();
    assert!(original.contains("\neula=false\n"));
    assert!(original.contains("https://www.minecraft.net/eula"));
    assert!(require_acceptance(&path).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn explicit_acceptance_is_required_and_existing_files_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("eula.txt");
    for contents in [
        "eula=true",
        "# comment\neula=true\n",
        "\u{feff}! comment\r\n eula = true \r\n",
    ] {
        fs::write(&path, contents).unwrap();
        require_acceptance(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }
    for contents in [
        "",
        "# eula=true\n",
        "eula=false",
        "eula=TRUE",
        "eula=1",
        "eula=",
        "eula=true\neula=false",
        "eula=true\neula=true",
        "other=true",
        "eula=true\ninvalid",
        "eula=true # comment",
    ] {
        fs::write(&path, contents).unwrap();
        assert!(require_acceptance(&path).is_err(), "{contents:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }
}

#[test]
fn read_errors_and_invalid_utf8_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    assert!(require_acceptance(dir.path()).is_err());
    let path = dir.path().join("eula.txt");
    fs::write(&path, [0xff]).unwrap();
    assert!(require_acceptance(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), [0xff]);
    assert!(require_acceptance(dir.path().join("missing/eula.txt")).is_err());
}

#[test]
fn concurrent_creation_never_accepts_or_publishes_partial_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("eula.txt");
    let barrier = Arc::new(Barrier::new(8));
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                assert_eq!(
                    require_acceptance(path).unwrap_err().kind(),
                    ErrorKind::PermissionDenied
                );
            })
        })
        .collect();
    for task in tasks {
        task.join().unwrap();
    }
    assert!(fs::read_to_string(path).unwrap().ends_with("eula=false\n"));
}

#[test]
fn binary_checks_eula_before_binding_and_only_proceeds_after_consent() {
    let dir = tempfile::tempdir().unwrap();
    // Occupy a port: reaching bind must return AddrInUse, not hang.
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = occupied.local_addr().unwrap().to_string();
    let bind_error = TcpListener::bind(&bind).unwrap_err().to_string();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_zcraft"))
            .current_dir(dir.path())
            .env_remove("ZCRAFT_CONFIG")
            .env_remove("ZCRAFT_MOTD")
            .env("ZCRAFT_BIND", &bind)
            .env("RUST_LOG", "error")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    };

    let output = run();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("ERROR zcraft:"), "{stderr}");
    assert!(stderr.contains(
        "EULA file eula.txt: EULA not accepted. Read https://www.minecraft.net/eula, then set eula=true only if you agree"
    ), "{stderr}");
    assert!(!stderr.contains("Custom {"), "{stderr}");
    assert!(!stderr.contains("PermissionDenied"), "{stderr}");
    assert!(dir.path().join("zcraft.toml").exists());

    let eula = dir.path().join("eula.txt");
    fs::write(&eula, "eula=true\n").unwrap();
    let output = run();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ERROR zcraft:"), "{stderr}");
    assert!(stderr.contains(&bind_error), "{stderr}");
    assert_eq!(fs::read_to_string(&eula).unwrap(), "eula=true\n");

    // Consent is checked on every restart.
    fs::write(eula, "eula=false\n").unwrap();
    let output = run();
    assert!(String::from_utf8_lossy(&output.stderr).contains("EULA not accepted"));
}

#[cfg(unix)]
#[test]
fn dangling_symlink_is_not_replaced_or_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("eula.txt");
    std::os::unix::fs::symlink("missing.txt", &path).unwrap();
    assert!(require_acceptance(&path).is_err());
    assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
}

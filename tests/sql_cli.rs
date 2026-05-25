use std::io::Write;
use std::process::Command;

#[test]
fn script_mode_returns_nonzero_on_error() {
    let path = std::env::temp_dir().join(format!(
        "toydb-cli-error-{}-{}.sql",
        std::process::id(),
        unique_suffix()
    ));
    {
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "SELECT no_such_column;").unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_toydb"))
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(&path).ok();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fatal:"), "{stderr}");
}

#[test]
fn db_flag_requires_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_toydb"))
        .arg("--db")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--db requires a path"), "{stderr}");
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

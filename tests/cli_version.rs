use std::process::Command;

#[test]
fn version_reports_compiled_provenance_without_starting_service() {
    let missing_state = std::env::temp_dir()
        .join(format!("dbgjs-version-no-service-{}", std::process::id()))
        .join("service.json");
    for argument in ["--version", "-V", "version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_dbgjs"))
            .args(["--json", argument])
            .env("DBGJS_SERVICE_STATE", &missing_state)
            .env("DBGJS_SERVICE_EXE", "nonexistent-dbgjs-service")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        let commit = env!("DBGJS_BUILD_GIT_COMMIT");
        assert_eq!(
            value["gitCommit"].as_str(),
            (commit != "unknown").then_some(commit)
        );
        assert_eq!(
            value["gitDirty"].as_bool(),
            env!("DBGJS_BUILD_GIT_DIRTY").parse::<bool>().ok()
        );
        assert!(!missing_state.exists());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_dbgjs"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(env!("CARGO_PKG_VERSION")));
    assert!(text.contains(env!("DBGJS_BUILD_GIT_COMMIT")));
    assert_eq!(
        text.contains(", dirty)"),
        env!("DBGJS_BUILD_GIT_DIRTY") == "true"
    );
}

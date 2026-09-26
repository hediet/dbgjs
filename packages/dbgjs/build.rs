use std::{env, path::Path, process::Command};

fn git(arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
        )
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn main() {
    let provenance = (|| {
        let commit = git(&["rev-parse", "HEAD"])?;
        let status = git(&["status", "--porcelain", "--untracked-files=no"])?;
        // Rebuild after edits, staging, commits, and branch switches, including worktrees.
        for name in ["HEAD", "index", "packed-refs", "logs/HEAD"] {
            let path = git(&["rev-parse", "--git-path", name])?;
            println!("cargo:rerun-if-changed={}", path.trim());
        }
        let reference = git(&["rev-parse", "--symbolic-full-name", "HEAD"])?;
        if reference.trim().starts_with("refs/") {
            let path = git(&["rev-parse", "--git-path", reference.trim()])?;
            println!("cargo:rerun-if-changed={}", path.trim());
        }
        for path in git(&["ls-files", "-z"])?
            .split('\0')
            .filter(|path| !path.is_empty())
        {
            // Submodule directories can contain ignored build outputs.
            if !Path::new(path).is_dir() {
                println!("cargo:rerun-if-changed=../../{path}");
            }
        }
        Ok::<_, String>((commit.trim().to_owned(), !status.trim().is_empty()))
    })();
    println!("cargo:rerun-if-changed=build.rs");
    let (commit, dirty) = match provenance {
        Ok((commit, dirty)) => (commit, dirty.to_string()),
        Err(error) => {
            println!("cargo:warning=Git build provenance unavailable: {error}");
            ("unknown".to_owned(), "unknown".to_owned())
        }
    };
    println!("cargo:rustc-env=DBGJS_BUILD_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=DBGJS_BUILD_GIT_DIRTY={dirty}");
}

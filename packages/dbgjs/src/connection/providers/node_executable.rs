//! Locates the Node.js executable that runs dbgjs's helper scripts.
//!
//! Services started by launchd, systemd, or a hub daemon often inherit a minimal `PATH`
//! without version-manager installs. After the explicit override and `PATH`, the search
//! therefore checks common install locations before giving up.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const NODE_OVERRIDE_VARIABLE: &str = "DBGJS_NODE";

#[derive(Debug, thiserror::Error)]
#[error(
    "Node.js was not found on PATH or in common install locations (nvm, fnm, Volta, mise, asdf, \
     Homebrew, /usr/local/bin, Program Files); add the directory containing node to the PATH of \
     the dbgjs service, or set DBGJS_NODE to the node executable"
)]
pub struct NodeExecutableNotFound;

/// Resolves `DBGJS_NODE`, then `node` on `PATH`, then the newest version-manager install, then
/// version-manager shims and system install locations.
pub fn resolve_node_executable() -> Result<PathBuf, NodeExecutableNotFound> {
    NodeSearch {
        platform: Platform::current(),
        variable: |name: &str| std::env::var_os(name),
    }
    .resolve()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Windows,
    Unix,
}

impl Platform {
    fn current() -> Self {
        if cfg!(windows) { Self::Windows } else { Self::Unix }
    }

    fn executable_name(self) -> &'static str {
        match self {
            Self::Windows => "node.exe",
            Self::Unix => "node",
        }
    }
}

struct NodeSearch<F: Fn(&str) -> Option<OsString>> {
    platform: Platform,
    variable: F,
}

impl<F: Fn(&str) -> Option<OsString>> NodeSearch<F> {
    fn resolve(&self) -> Result<PathBuf, NodeExecutableNotFound> {
        if let Some(executable) = self.non_empty(NODE_OVERRIDE_VARIABLE) {
            return Ok(PathBuf::from(executable));
        }
        self.path_candidates()
            .into_iter()
            .chain(self.versioned_install_candidates())
            .chain(self.fixed_install_candidates())
            .find(|candidate| is_executable(candidate))
            .ok_or(NodeExecutableNotFound)
    }

    fn path_candidates(&self) -> Vec<PathBuf> {
        self.non_empty("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join(self.platform.executable_name()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Installs managed by nvm, nvm-windows, fnm, mise, and asdf, newest version first.
    fn versioned_install_candidates(&self) -> Vec<PathBuf> {
        let unix_bin = Path::new("bin").join("node");
        let fnm_executable = match self.platform {
            Platform::Windows => Path::new("installation").join("node.exe"),
            Platform::Unix => Path::new("installation").join(&unix_bin),
        };
        let mut layouts: Vec<(PathBuf, PathBuf)> = self
            .fnm_directories()
            .into_iter()
            .map(|directory| (directory.join("node-versions"), fnm_executable.clone()))
            .collect();
        match self.platform {
            Platform::Windows => {
                let nvm_home = self
                    .non_empty("NVM_HOME")
                    .map(PathBuf::from)
                    .or_else(|| self.directory("APPDATA", &["nvm"]));
                layouts.extend(nvm_home.map(|home| (home, PathBuf::from("node.exe"))));
                layouts.extend(
                    self.directory("LOCALAPPDATA", &["mise"])
                        .map(|mise| (mise.join("installs").join("node"), PathBuf::from("node.exe"))),
                );
            }
            Platform::Unix => {
                let nvm = self
                    .non_empty("NVM_DIR")
                    .map(PathBuf::from)
                    .or_else(|| self.home_directory(&[".nvm"]));
                layouts.extend(nvm.map(|nvm| (nvm.join("versions").join("node"), unix_bin.clone())));
                layouts.extend(
                    self.mise_directory()
                        .map(|mise| (mise.join("installs").join("node"), unix_bin.clone())),
                );
                layouts.extend(
                    self.asdf_directory()
                        .map(|asdf| (asdf.join("installs").join("nodejs"), unix_bin.clone())),
                );
            }
        }
        let mut installs = layouts
            .iter()
            .flat_map(|(versions, executable)| version_directories(versions, executable))
            .collect::<Vec<_>>();
        installs.sort_by(|(left, _), (right, _)| right.cmp(left));
        installs.into_iter().map(|(_, executable)| executable).collect()
    }

    fn fnm_directories(&self) -> Vec<PathBuf> {
        if let Some(directory) = self.non_empty("FNM_DIR") {
            return vec![PathBuf::from(directory)];
        }
        match self.platform {
            Platform::Windows => self.directory("APPDATA", &["fnm"]).into_iter().collect(),
            Platform::Unix => [
                self.directory("XDG_DATA_HOME", &["fnm"]),
                self.home_directory(&[".local", "share", "fnm"]),
                self.home_directory(&["Library", "Application Support", "fnm"]),
                self.home_directory(&[".fnm"]),
            ]
            .into_iter()
            .flatten()
            .collect(),
        }
    }

    fn mise_directory(&self) -> Option<PathBuf> {
        self.non_empty("MISE_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| self.directory("XDG_DATA_HOME", &["mise"]))
            .or_else(|| self.home_directory(&[".local", "share", "mise"]))
    }

    fn asdf_directory(&self) -> Option<PathBuf> {
        self.non_empty("ASDF_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| self.home_directory(&[".asdf"]))
    }

    /// Version-manager shims and package-manager or system installs, in preference order.
    fn fixed_install_candidates(&self) -> Vec<PathBuf> {
        match self.platform {
            Platform::Windows => [
                self.non_empty("VOLTA_HOME")
                    .map(|volta| PathBuf::from(volta).join("bin").join("node.exe")),
                self.directory("LOCALAPPDATA", &["Volta", "bin", "node.exe"]),
                self.directory("ProgramFiles", &["Volta", "node.exe"]),
                self.non_empty("NVM_SYMLINK")
                    .map(|symlink| PathBuf::from(symlink).join("node.exe")),
                self.home_directory(&["scoop", "apps", "nodejs", "current", "node.exe"]),
                self.home_directory(&["scoop", "apps", "nodejs-lts", "current", "node.exe"]),
                self.directory("ProgramFiles", &["nodejs", "node.exe"]),
                self.directory("ProgramFiles(x86)", &["nodejs", "node.exe"]),
            ]
            .into_iter()
            .flatten()
            .collect(),
            Platform::Unix => {
                let volta = self
                    .non_empty("VOLTA_HOME")
                    .map(PathBuf::from)
                    .or_else(|| self.home_directory(&[".volta"]));
                [
                    volta.map(|volta| volta.join("bin").join("node")),
                    self.mise_directory().map(|mise| mise.join("shims").join("node")),
                    self.asdf_directory().map(|asdf| asdf.join("shims").join("node")),
                ]
                .into_iter()
                .flatten()
                .chain(
                    [
                        "/opt/homebrew/bin/node",
                        "/usr/local/bin/node",
                        "/opt/local/bin/node",
                        "/usr/bin/node",
                        "/snap/bin/node",
                    ]
                    .map(PathBuf::from),
                )
                .collect()
            }
        }
    }

    fn home_directory(&self, relative: &[&str]) -> Option<PathBuf> {
        let home = match self.platform {
            Platform::Windows => self.non_empty("USERPROFILE").or_else(|| self.non_empty("HOME")),
            Platform::Unix => self.non_empty("HOME"),
        };
        home.map(|home| join(PathBuf::from(home), relative))
    }

    fn directory(&self, variable: &str, relative: &[&str]) -> Option<PathBuf> {
        self.non_empty(variable)
            .map(|directory| join(PathBuf::from(directory), relative))
    }

    fn non_empty(&self, variable: &str) -> Option<OsString> {
        (self.variable)(variable).filter(|value| !value.is_empty())
    }
}

fn join(base: PathBuf, components: &[&str]) -> PathBuf {
    components
        .iter()
        .fold(base, |path, component| path.join(component))
}

type NodeVersion = (u64, u64, u64);

/// Lists `<versions>/<version>/<executable>` for each subdirectory named like `v20.11.0`.
fn version_directories(versions: &Path, executable: &Path) -> Vec<(NodeVersion, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(versions) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = parse_version(entry.file_name().to_str()?)?;
            Some((version, entry.path().join(executable)))
        })
        .collect()
}

fn parse_version(name: &str) -> Option<NodeVersion> {
    let mut parts = name.strip_prefix('v').unwrap_or(name).split('.');
    let major = parts.next()?.parse().ok()?;
    let mut component = || parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    Some((major, component(), component()))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().expect("temporary directory"),
            }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.root.path().join(relative)
        }

        fn executable(&self, relative: &str) -> PathBuf {
            let path = self.path(relative);
            std::fs::create_dir_all(path.parent().expect("executable has a parent"))
                .expect("create executable parent");
            std::fs::write(&path, "").expect("write executable");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("mark executable");
            }
            path
        }

        fn resolve(
            &self,
            platform: Platform,
            variables: &[(&str, &str)],
        ) -> Result<PathBuf, NodeExecutableNotFound> {
            let variables = variables
                .iter()
                .map(|(name, value)| ((*name).to_owned(), OsString::from(self.path(value))))
                .collect::<BTreeMap<_, _>>();
            NodeSearch {
                platform,
                variable: |name: &str| variables.get(name).cloned(),
            }
            .resolve()
        }
    }

    #[test]
    fn override_wins_without_inspecting_the_file_system() {
        let fixture = Fixture::new();
        fixture.executable("bin/node");
        assert_eq!(
            fixture
                .resolve(Platform::Unix, &[("DBGJS_NODE", "custom/node"), ("PATH", "bin")])
                .unwrap(),
            fixture.path("custom/node")
        );
    }

    #[test]
    fn path_wins_over_install_locations() {
        let fixture = Fixture::new();
        let on_path = fixture.executable("bin/node");
        fixture.executable("home/.nvm/versions/node/v22.1.0/bin/node");
        assert_eq!(
            fixture
                .resolve(Platform::Unix, &[("PATH", "bin"), ("HOME", "home")])
                .unwrap(),
            on_path
        );
    }

    #[test]
    fn newest_version_manager_install_wins_across_managers() {
        let fixture = Fixture::new();
        fixture.executable("home/.nvm/versions/node/v18.20.4/bin/node");
        fixture.executable("home/.nvm/versions/node/v20.9.0/bin/node");
        let newest =
            fixture.executable("home/.local/share/fnm/node-versions/v20.11.1/installation/bin/node");
        fixture.executable("home/.volta/bin/node");
        // A version directory without an executable must not shadow older installs.
        std::fs::create_dir_all(fixture.path("home/.nvm/versions/node/v23.0.0"))
            .expect("create incomplete version");
        std::fs::create_dir_all(fixture.path("home/.local/share/fnm/node-versions/.downloads"))
            .expect("create fnm download directory");
        assert_eq!(
            fixture
                .resolve(Platform::Unix, &[("PATH", "empty"), ("HOME", "home")])
                .unwrap(),
            newest
        );
    }

    #[test]
    fn explicit_manager_directories_replace_defaults() {
        let fixture = Fixture::new();
        fixture.executable("home/.nvm/versions/node/v18.0.0/bin/node");
        let custom = fixture.executable("nvm/versions/node/v16.0.0/bin/node");
        assert_eq!(
            fixture
                .resolve(Platform::Unix, &[("HOME", "home"), ("NVM_DIR", "nvm")])
                .unwrap(),
            custom
        );
    }

    #[test]
    fn macos_fnm_and_mise_installs_are_found() {
        let fixture = Fixture::new();
        let fnm = fixture.executable(
            "home/Library/Application Support/fnm/node-versions/v22.0.0/installation/bin/node",
        );
        fixture.executable("home/.local/share/mise/installs/node/21.7.3/bin/node");
        assert_eq!(
            fixture.resolve(Platform::Unix, &[("HOME", "home")]).unwrap(),
            fnm
        );
    }

    #[test]
    fn volta_and_shims_are_used_when_no_versioned_install_exists() {
        let fixture = Fixture::new();
        fixture.executable("home/.asdf/shims/node");
        let volta = fixture.executable("home/.volta/bin/node");
        assert_eq!(
            fixture.resolve(Platform::Unix, &[("HOME", "home")]).unwrap(),
            volta
        );
    }

    #[test]
    fn windows_fnm_and_program_files_installs_are_found() {
        let fixture = Fixture::new();
        let program_files = fixture.executable("Program Files/nodejs/node.exe");
        assert_eq!(
            fixture
                .resolve(Platform::Windows, &[("ProgramFiles", "Program Files")])
                .unwrap(),
            program_files
        );
        let fnm = fixture.executable("AppData/Roaming/fnm/node-versions/v20.0.0/installation/node.exe");
        assert_eq!(
            fixture
                .resolve(
                    Platform::Windows,
                    &[("ProgramFiles", "Program Files"), ("APPDATA", "AppData/Roaming")],
                )
                .unwrap(),
            fnm
        );
    }

    #[test]
    fn missing_node_reports_how_to_fix_it() {
        let fixture = Fixture::new();
        let error = fixture
            .resolve(Platform::Windows, &[("PATH", "bin"), ("USERPROFILE", "home")])
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Node.js was not found on PATH or in common install locations (nvm, fnm, Volta, mise, \
             asdf, Homebrew, /usr/local/bin, Program Files); add the directory containing node to \
             the PATH of the dbgjs service, or set DBGJS_NODE to the node executable"
        );
    }

    #[test]
    fn versions_parse_with_and_without_prefix() {
        assert_eq!(parse_version("v20.11.1"), Some((20, 11, 1)));
        assert_eq!(parse_version("22"), Some((22, 0, 0)));
        assert_eq!(parse_version(".downloads"), None);
        assert_eq!(parse_version("lts"), None);
    }
}

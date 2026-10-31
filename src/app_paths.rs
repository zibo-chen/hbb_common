//! Desktop data paths are selected once, before any configuration or logger is opened.
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

#[derive(Debug)]
pub struct AppPaths {
    pub root: PathBuf,
    pub portable: bool,
}

static PATHS: OnceLock<AppPaths> = OnceLock::new();

pub fn get() -> Option<&'static AppPaths> {
    PATHS.get()
}

impl AppPaths {
    pub fn config(&self) -> PathBuf {
        self.root.join("config")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn ipc_namespace(&self) -> String {
        let digest =
            sodiumoxide::crypto::hash::sha256::hash(self.root.to_string_lossy().as_bytes());
        digest.0[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn absolute(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// A macOS app's portable data lives beside the bundle, never inside its signed resources.
fn application_directory(exe: &Path) -> io::Result<PathBuf> {
    for ancestor in exe.ancestors() {
        if ancestor.extension().is_some_and(|ext| ext == "app") {
            return ancestor
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| io::Error::other("Application bundle has no parent"));
        }
    }
    let parent = exe
        .parent()
        .ok_or_else(|| io::Error::other("Executable has no parent"))?;
    if parent.file_name().is_some_and(|name| name == "app") {
        return parent
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| io::Error::other("Portable app directory has no parent"));
    }
    Ok(parent.to_path_buf())
}

pub fn initialize(app_name: &str, organization: &str) -> io::Result<&'static AppPaths> {
    if let Some(paths) = get() {
        return Ok(paths);
    }
    let args: Vec<_> = std::env::args().collect();
    let explicit = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--data-dir=").map(PathBuf::from))
        .or_else(|| {
            args.windows(2)
                .find(|pair| pair[0] == "--data-dir")
                .map(|pair| PathBuf::from(&pair[1]))
        })
        .or_else(|| std::env::var_os("SUBNETDESK_DATA_DIR").map(PathBuf::from));
    let portable_root = std::env::var_os("SUBNETDESK_PORTABLE_ROOT").map(PathBuf::from);
    let app_dir = application_directory(&std::env::current_exe()?)?;
    let portable = portable_root.is_some()
        || args.iter().any(|arg| arg == "--portable")
        || app_dir.join("portable").is_file()
        || cfg!(windows);
    let import_legacy = explicit.is_none() && !portable;
    let root = if let Some(path) = explicit {
        absolute(path)?
    } else if portable {
        absolute(portable_root.unwrap_or(app_dir))?.join("userdata")
    } else {
        directories_next::ProjectDirs::from("", organization, app_name)
            .ok_or_else(|| io::Error::other("Cannot locate the current user's data directory"))?
            .data_local_dir()
            .to_path_buf()
    };
    let mut paths = AppPaths { root, portable };
    for directory in [paths.config(), paths.logs(), paths.cache()] {
        fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
    }
    paths.root = fs::canonicalize(&paths.root)?;
    if import_legacy {
        if let Some(legacy) = directories_next::ProjectDirs::from("", organization, app_name) {
            migrate_config(legacy.config_dir(), &paths.config())?;
        }
    }
    // Check writability now. Never silently redirect a read-only portable application to AppData.
    let probe = paths.root.join(format!(
        ".write-check-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    drop(file);
    fs::remove_file(probe)?;
    // Child UI/connection-manager processes must use the same profile even when
    // launched without the parent's command-line flags.
    std::env::set_var("SUBNETDESK_DATA_DIR", &paths.root);
    PATHS
        .set(paths)
        .map_err(|_| io::Error::other("Desktop paths already initialized"))?;
    get().ok_or_else(|| io::Error::other("Desktop path initialization failed"))
}

fn migrate_config(source: &Path, destination: &Path) -> io::Result<()> {
    if !source.is_dir() || source == destination {
        return Ok(());
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "toml") {
            if !target.exists() {
                fs::copy(entry.path(), target)?;
            }
        } else if kind.is_dir() && entry.file_name() == "peers" {
            fs::create_dir_all(&target)?;
            migrate_config(&entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_root_is_outside_bundle_and_runtime() {
        assert_eq!(
            application_directory(Path::new(
                "/tmp/desk/SubnetDesk.app/Contents/MacOS/SubnetDesk"
            ))
            .unwrap(),
            Path::new("/tmp/desk")
        );
        assert_eq!(
            application_directory(Path::new("/tmp/desk/app/SubnetDesk.exe")).unwrap(),
            Path::new("/tmp/desk")
        );
        assert_eq!(
            application_directory(Path::new("/tmp/desk/SubnetDesk.exe")).unwrap(),
            Path::new("/tmp/desk")
        );
    }

    #[test]
    fn runtime_assets_and_user_data_are_separate() {
        let paths = AppPaths {
            root: PathBuf::from("/tmp/desk/userdata"),
            portable: true,
        };
        assert_eq!(paths.config(), Path::new("/tmp/desk/userdata/config"));
        assert_eq!(paths.logs(), Path::new("/tmp/desk/userdata/logs"));
        assert_eq!(paths.cache(), Path::new("/tmp/desk/userdata/cache"));
    }

    #[test]
    fn migration_preserves_current_settings_and_imports_peers_only() {
        let root = std::env::temp_dir().join(format!(
            "subnetdesk-paths-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let source = root.join("legacy");
        let target = root.join("userdata/config");
        fs::create_dir_all(source.join("peers")).unwrap();
        fs::create_dir_all(source.join("log")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("SubnetDesk.toml"), "old").unwrap();
        fs::write(source.join("peers/peer.toml"), "peer").unwrap();
        fs::write(source.join("log/unrelated.toml"), "log").unwrap();
        fs::write(target.join("SubnetDesk.toml"), "current").unwrap();
        migrate_config(&source, &target).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("SubnetDesk.toml")).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(target.join("peers/peer.toml")).unwrap(),
            "peer"
        );
        assert!(!target.join("log").exists());
        assert!(source.join("SubnetDesk.toml").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ipc_namespaces_keep_portable_profiles_separate() {
        let first = AppPaths {
            root: PathBuf::from("/tmp/a/userdata"),
            portable: true,
        };
        let second = AppPaths {
            root: PathBuf::from("/tmp/b/userdata"),
            portable: true,
        };
        assert_eq!(first.ipc_namespace(), first.ipc_namespace());
        assert_ne!(first.ipc_namespace(), second.ipc_namespace());
    }
}

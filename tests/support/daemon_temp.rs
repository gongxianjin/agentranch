use std::path::{Path, PathBuf};

pub fn temp_root() -> tempfile::TempDir {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize().unwrap();
    let target = checkout.join("target");
    let base = target.join("task-2-lifecycle-tests");
    assert_outside_appdata(&base);
    for dir in [&target, &base] {
        match std::fs::create_dir(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("cannot create lifecycle test fixture parent: {error}"),
        }
        let metadata = std::fs::symlink_metadata(dir).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            assert_eq!(metadata.file_attributes() & 0x400, 0, "fixture parent is a reparse point");
        }
    }
    let root = tempfile::tempdir_in(base).unwrap();
    assert_outside_appdata(&root.path().canonicalize().unwrap());
    root
}

fn assert_outside_appdata(path: &Path) {
    let profile_appdata = std::env::var_os("USERPROFILE").map(|base| PathBuf::from(base).join("AppData"));
    let env_appdata = ["APPDATA", "LOCALAPPDATA", "LOCALAPPDATALOW"].into_iter()
        .filter_map(std::env::var_os).map(PathBuf::from);
    let path = comparable(path);
    for appdata in profile_appdata.into_iter().chain(env_appdata) {
        assert!(!path.starts_with(comparable(&appdata)), "lifecycle test fixtures must not use AppData");
    }
}

fn comparable(path: &Path) -> PathBuf {
    let spelling = path.to_string_lossy();
    #[cfg(windows)]
    {
        let spelling = spelling.replace('/', "\\");
        PathBuf::from(spelling.strip_prefix(r"\\?\").unwrap_or(&spelling).to_lowercase())
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(spelling.as_ref())
    }
}

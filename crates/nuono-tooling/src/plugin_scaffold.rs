use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MANIFEST_TEMPLATE: &str = include_str!("../templates/plugin/Cargo.toml.template");
const DEFAULT_CONFIG_TEMPLATE: &str =
    include_str!("../templates/plugin/src/default_config.rs.template");
const SOURCE_TEMPLATE: &str = include_str!("../templates/plugin/src/lib.rs.template");
const README_TEMPLATE: &str = include_str!("../templates/plugin/README.md.template");
const GITIGNORE_TEMPLATE: &str = include_str!("../templates/plugin/.gitignore.template");

#[derive(Debug, Eq, PartialEq)]
pub struct GeneratedPlugin {
    pub root: PathBuf,
    pub library_stem: String,
}

#[derive(Debug)]
pub enum ScaffoldError {
    InvalidName { name: String },
    InvalidPluginsRoot { path: PathBuf },
    Busy { path: PathBuf },
    AlreadyExists { path: PathBuf },
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for ScaffoldError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => write!(
                formatter,
                "invalid plugin package name `{name}`; use lowercase ASCII letters, digits, and interior hyphens; hub is reserved"
            ),
            Self::InvalidPluginsRoot { path } => write!(
                formatter,
                "plugins root must be a real directory, not a symlink or other file: {}",
                path.display()
            ),
            Self::Busy { path } => write!(
                formatter,
                "plugin scaffold is already in progress: {}",
                path.display()
            ),
            Self::AlreadyExists { path } => {
                write!(
                    formatter,
                    "plugin directory already exists: {}",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(formatter, "failed to scaffold {}: {source}", path.display())
            }
        }
    }
}

impl Error for ScaffoldError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidName { .. }
            | Self::InvalidPluginsRoot { .. }
            | Self::Busy { .. }
            | Self::AlreadyExists { .. } => None,
        }
    }
}

pub fn create_plugin(repository_root: &Path, name: &str) -> Result<GeneratedPlugin, ScaffoldError> {
    validate_name(name)?;

    let plugins_root = repository_root.join("plugins");
    ensure_plugins_root(&plugins_root)?;

    let lock = ScaffoldLock::acquire(&plugins_root, name)?;
    let plugins_root_identity = PluginsRootIdentity::capture(&plugins_root)?;
    let final_root = plugins_root.join(name);
    ensure_destination_absent(&final_root)?;

    let staged = StagedPlugin::materialize(&plugins_root, name)?;
    staged.publish(&final_root, &plugins_root_identity, &lock)?;

    Ok(GeneratedPlugin {
        root: final_root,
        library_stem: name.replace('-', "_"),
    })
}

fn validate_name(name: &str) -> Result<(), ScaffoldError> {
    let library_stem = name.replace('-', "_");
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && !name.as_bytes().windows(2).any(|pair| pair == b"--")
        && !is_rust_reserved_identifier(&library_stem)
        && !nuono_domain::deployment::is_reserved_plugin_name(name);

    if valid {
        Ok(())
    } else {
        Err(ScaffoldError::InvalidName {
            name: name.to_string(),
        })
    }
}

fn is_rust_reserved_identifier(identifier: &str) -> bool {
    matches!(
        identifier,
        "Self"
            | "abstract"
            | "as"
            | "async"
            | "await"
            | "become"
            | "box"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "do"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "final"
            | "fn"
            | "for"
            | "gen"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "macro"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "override"
            | "priv"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "try"
            | "type"
            | "typeof"
            | "union"
            | "unsafe"
            | "unsized"
            | "use"
            | "virtual"
            | "where"
            | "while"
            | "yield"
    )
}

fn ensure_plugins_root(plugins_root: &Path) -> Result<(), ScaffoldError> {
    match fs::symlink_metadata(plugins_root) {
        Ok(metadata) => validate_plugins_root(plugins_root, &metadata),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            match fs::create_dir(plugins_root) {
                Ok(()) => Ok(()),
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata =
                        fs::symlink_metadata(plugins_root).map_err(|source| ScaffoldError::Io {
                            path: plugins_root.to_path_buf(),
                            source,
                        })?;
                    validate_plugins_root(plugins_root, &metadata)
                }
                Err(source) => Err(ScaffoldError::Io {
                    path: plugins_root.to_path_buf(),
                    source,
                }),
            }
        }
        Err(source) => Err(ScaffoldError::Io {
            path: plugins_root.to_path_buf(),
            source,
        }),
    }
}

fn validate_plugins_root(
    plugins_root: &Path,
    metadata: &fs::Metadata,
) -> Result<(), ScaffoldError> {
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(ScaffoldError::InvalidPluginsRoot {
            path: plugins_root.to_path_buf(),
        })
    }
}

struct ScaffoldLock {
    path: PathBuf,
    identity: PlatformFileIdentity,
    // 在 Drop 检查锁文件路径前保持文件句柄打开，防止原 inode/文件索引被系统回收复用作他用。
    file: fs::File,
}

impl ScaffoldLock {
    fn acquire(plugins_root: &Path, name: &str) -> Result<Self, ScaffoldError> {
        let path = plugins_root.join(format!(".{name}.scaffold.lock"));
        let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ScaffoldError::Busy { path });
            }
            Err(source) => return Err(ScaffoldError::Io { path, source }),
        };

        let metadata = file.metadata().map_err(|source| ScaffoldError::Io {
            path: path.clone(),
            source,
        })?;
        let identity = PlatformFileIdentity::capture_open(&file, &metadata);

        let mut lock = Self {
            path,
            identity,
            file,
        };

        if let Err(source) = writeln!(lock.file, "{}", std::process::id()) {
            return Err(ScaffoldError::Io {
                path: lock.path.clone(),
                source,
            });
        }

        Ok(lock)
    }
}

impl Drop for ScaffoldLock {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_file() && self.identity.safe_to_unlink(&self.path, &metadata) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PluginsRootIdentity {
    canonical_path: PathBuf,
    platform_identity: PlatformFileIdentity,
}

impl PluginsRootIdentity {
    fn capture(plugins_root: &Path) -> Result<Self, ScaffoldError> {
        let metadata = fs::symlink_metadata(plugins_root).map_err(|source| ScaffoldError::Io {
            path: plugins_root.to_path_buf(),
            source,
        })?;
        validate_plugins_root(plugins_root, &metadata)?;

        let canonical_path =
            fs::canonicalize(plugins_root).map_err(|source| ScaffoldError::Io {
                path: plugins_root.to_path_buf(),
                source,
            })?;

        Ok(Self {
            canonical_path,
            platform_identity: PlatformFileIdentity::capture(plugins_root, &metadata),
        })
    }

    fn revalidate(&self, plugins_root: &Path) -> Result<(), ScaffoldError> {
        let metadata = fs::symlink_metadata(plugins_root).map_err(|source| ScaffoldError::Io {
            path: plugins_root.to_path_buf(),
            source,
        })?;
        validate_plugins_root(plugins_root, &metadata)?;
        let canonical_path =
            fs::canonicalize(plugins_root).map_err(|source| ScaffoldError::Io {
                path: plugins_root.to_path_buf(),
                source,
            })?;

        if canonical_path == self.canonical_path
            && self.platform_identity.matches(plugins_root, &metadata)
        {
            Ok(())
        } else {
            Err(ScaffoldError::InvalidPluginsRoot {
                path: plugins_root.to_path_buf(),
            })
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Eq, PartialEq)]
struct PlatformFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl PlatformFileIdentity {
    /// 路径身份：`symlink_metadata` 得到的元数据已对应打开时的对象。
    fn capture(_path: &Path, metadata: &fs::Metadata) -> Self {
        Self::from_metadata(metadata)
    }

    /// 句柄身份：对已打开的文件取元数据，避免重新解析路径带来的竞态。
    fn capture_open(_file: &fs::File, metadata: &fs::Metadata) -> Self {
        Self::from_metadata(metadata)
    }

    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn matches(&self, path: &Path, metadata: &fs::Metadata) -> bool {
        *self == Self::capture(path, metadata)
    }

    fn safe_to_unlink(&self, path: &Path, metadata: &fs::Metadata) -> bool {
        self.matches(path, metadata)
    }
}

#[cfg(windows)]
#[derive(Debug, Eq, PartialEq)]
struct PlatformFileIdentity {
    volume_serial_number: Option<u32>,
    file_index: Option<u64>,
}

#[cfg(windows)]
impl PlatformFileIdentity {
    /// 路径身份：仅以元数据查询权限重新打开路径本身（不跟随重解析点）后读取句柄信息。
    ///
    /// `MetadataExt::volume_serial_number` / `MetadataExt::file_index` 在 stable 上仍是
    /// nightly-only 的 `windows_by_handle`，因此改用 `GetFileInformationByHandle`。
    fn capture(path: &Path, metadata: &fs::Metadata) -> Self {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };

        // 目录必须带 FILE_FLAG_BACKUP_SEMANTICS 才能打开；FILE_FLAG_OPEN_REPARSE_POINT
        // 保证拿到的是链接或联接自身，与 `symlink_metadata` 对齐；access_mode(0) 仅查询元数据。
        let opened = fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path);
        // 打不开或查询失败时返回未知身份，让 `matches` 一律判定为不匹配（保守拒绝）。
        opened.map_or_else(
            |_| Self::unknown(),
            |file| Self::capture_open(&file, metadata),
        )
    }

    /// 句柄身份：直接查询已打开句柄，不重新解析路径。
    fn capture_open(file: &fs::File, _metadata: &fs::Metadata) -> Self {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };

        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` 在调用期间保持打开，句柄有效；`info` 是独占且大小匹配的可写结构体。
        let succeeded = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
        if succeeded == 0 {
            return Self::unknown();
        }

        Self {
            volume_serial_number: Some(info.dwVolumeSerialNumber),
            file_index: Some(
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            ),
        }
    }

    fn unknown() -> Self {
        Self {
            volume_serial_number: None,
            file_index: None,
        }
    }

    fn matches(&self, path: &Path, metadata: &fs::Metadata) -> bool {
        let current = Self::capture(path, metadata);
        self.volume_serial_number.is_some() && self.file_index.is_some() && *self == current
    }

    fn safe_to_unlink(&self, path: &Path, metadata: &fs::Metadata) -> bool {
        self.matches(path, metadata)
    }
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Eq, PartialEq)]
struct PlatformFileIdentity {
    created: Option<std::time::SystemTime>,
}

#[cfg(not(any(unix, windows)))]
impl PlatformFileIdentity {
    fn capture(_path: &Path, metadata: &fs::Metadata) -> Self {
        Self::from_metadata(metadata)
    }

    fn capture_open(_file: &fs::File, metadata: &fs::Metadata) -> Self {
        Self::from_metadata(metadata)
    }

    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            created: metadata.created().ok(),
        }
    }

    fn matches(&self, path: &Path, metadata: &fs::Metadata) -> bool {
        self.created.is_some() && *self == Self::capture(path, metadata)
    }

    fn safe_to_unlink(&self, _path: &Path, _metadata: &fs::Metadata) -> bool {
        false
    }
}

struct StagedPlugin {
    root: PathBuf,
    published: bool,
}

impl StagedPlugin {
    fn materialize(plugins_root: &Path, name: &str) -> Result<Self, ScaffoldError> {
        let root = plugins_root.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).map_err(|source| ScaffoldError::Io {
            path: root.clone(),
            source,
        })?;

        let mut staged = Self {
            root,
            published: false,
        };
        staged.write(name)?;
        Ok(staged)
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.root
    }

    fn write(&mut self, name: &str) -> Result<(), ScaffoldError> {
        let source_root = self.root.join("src");
        fs::create_dir(&source_root).map_err(|source| ScaffoldError::Io {
            path: source_root.clone(),
            source,
        })?;

        let manifest_path = self.root.join("Cargo.toml");
        let manifest = MANIFEST_TEMPLATE.replace("{{package_name}}", name);
        fs::write(&manifest_path, manifest).map_err(|source| ScaffoldError::Io {
            path: manifest_path,
            source,
        })?;

        // 从宿主仓库复制 Cargo.lock 作为依赖版本的初始基线；
        // 插件首次独立构建时，Cargo 会在尽量沿用兼容锁定版本的基础上解析插件自身的依赖。
        let host_lock = self
            .root
            .parent()
            .and_then(Path::parent)
            .map(|root| root.join("Cargo.lock"));
        if let Some(host_lock) = host_lock.filter(|path| path.is_file()) {
            let plugin_lock = self.root.join("Cargo.lock");
            fs::copy(&host_lock, &plugin_lock).map_err(|source| ScaffoldError::Io {
                path: plugin_lock,
                source,
            })?;
        }

        let default_path = source_root.join("default_config.rs");
        fs::write(&default_path, DEFAULT_CONFIG_TEMPLATE).map_err(|source| ScaffoldError::Io {
            path: default_path,
            source,
        })?;
        for (filename, content) in [
            (
                "README.md",
                README_TEMPLATE.replace("{{package_name}}", name),
            ),
            (".gitignore", GITIGNORE_TEMPLATE.to_string()),
        ] {
            let path = self.root.join(filename);
            fs::write(&path, content).map_err(|source| ScaffoldError::Io { path, source })?;
        }
        let source_path = source_root.join("lib.rs");
        fs::write(
            &source_path,
            SOURCE_TEMPLATE.replace("{{package_name}}", name),
        )
        .map_err(|source| ScaffoldError::Io {
            path: source_path,
            source,
        })
    }

    fn publish(
        mut self,
        final_root: &Path,
        plugins_root_identity: &PluginsRootIdentity,
        lock: &ScaffoldLock,
    ) -> Result<(), ScaffoldError> {
        let plugins_root = final_root
            .parent()
            .expect("plugin destination must have a plugins parent");
        debug_assert_eq!(lock.path.parent(), Some(plugins_root));
        plugins_root_identity.revalidate(plugins_root)?;
        ensure_destination_absent(final_root)?;

        fs::rename(&self.root, final_root).map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                ScaffoldError::AlreadyExists {
                    path: final_root.to_path_buf(),
                }
            } else {
                ScaffoldError::Io {
                    path: final_root.to_path_buf(),
                    source,
                }
            }
        })?;
        self.published = true;
        Ok(())
    }
}

impl Drop for StagedPlugin {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn ensure_destination_absent(final_root: &Path) -> Result<(), ScaffoldError> {
    match fs::symlink_metadata(final_root) {
        Ok(_) => Err(ScaffoldError::AlreadyExists {
            path: final_root.to_path_buf(),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ScaffoldError::Io {
            path: final_root.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct TestRepository {
        root: PathBuf,
    }

    impl TestRepository {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("nuono-plugin-scaffold-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).expect("create temporary repository");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn creates_plugin_from_templates() {
        let repository = TestRepository::new();

        let generated =
            create_plugin(repository.path(), "my-plugin").expect("generate valid plugin");

        assert_eq!(generated.root, repository.path().join("plugins/my-plugin"));
        assert_eq!(generated.library_stem, "my_plugin");

        let manifest =
            fs::read_to_string(generated.root.join("Cargo.toml")).expect("read generated manifest");
        assert!(manifest.contains("name = \"my-plugin\""));
        assert!(!manifest.contains("rust-version ="));
        assert!(manifest.contains("crate-type = [\"rlib\"]"));
        assert!(manifest.contains("[workspace]"));
        assert!(manifest.contains("nuono-plugin = { path = \"../../crates/nuono-plugin\" }"));
        assert!(
            manifest.contains("nuono-plugin-test = { path = \"../../crates/nuono-plugin-test\" }")
        );
        assert!(!manifest.contains("serde ="));
        assert!(!manifest.contains("reqwest ="));

        let source =
            fs::read_to_string(generated.root.join("src/lib.rs")).expect("read generated source");
        assert!(source.contains("use nuono_plugin::prelude::*;"));
        assert!(source.contains("struct MyPlugin"));
        assert!(source.contains("greeting_uses_the_runtime_config"));
        assert!(source.contains("register_plugin!(MyPlugin);"));
        assert!(generated.root.join("README.md").is_file());
        assert!(!generated.root.join("rust-toolchain.toml").exists());

        let remaining_entries = fs::read_dir(repository.path().join("plugins"))
            .expect("read plugins directory")
            .map(|entry| entry.expect("read plugins entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(remaining_entries, [std::ffi::OsString::from("my-plugin")]);
    }

    #[test]
    fn rejects_invalid_package_names_before_mutating_repository() {
        for name in [
            "",
            ".",
            "..",
            "Bad_Name",
            "-bad",
            "bad-",
            "bad--name",
            "bad/name",
            "1-plugin",
            "async",
            "mut",
            "self",
            "hub",
        ] {
            let repository = TestRepository::new();

            let error = create_plugin(repository.path(), name).expect_err("reject invalid name");

            assert!(matches!(error, ScaffoldError::InvalidName { .. }));
            assert!(!repository.path().join("plugins").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_plugins_root_symlink_without_writing_to_its_target() {
        use std::os::unix::fs::symlink;

        let repository = TestRepository::new();
        let external = TestRepository::new();
        let plugins_root = repository.path().join("plugins");
        let sentinel = external.path().join("keep.txt");
        fs::write(&sentinel, "preserve me").expect("write external sentinel");
        symlink(external.path(), &plugins_root).expect("create plugins root symlink");

        let error =
            create_plugin(repository.path(), "my-plugin").expect_err("reject plugins root symlink");

        assert!(matches!(error, ScaffoldError::InvalidPluginsRoot { .. }));
        assert_eq!(
            fs::read_to_string(&sentinel).expect("read external sentinel"),
            "preserve me"
        );
        assert!(!external.path().join("my-plugin").exists());
        assert!(
            fs::symlink_metadata(&plugins_root)
                .expect("preserve plugins root symlink")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn rejects_plugins_root_regular_file() {
        let repository = TestRepository::new();
        let plugins_root = repository.path().join("plugins");
        fs::write(&plugins_root, "preserve me").expect("write plugins root file");

        let error = create_plugin(repository.path(), "my-plugin")
            .expect_err("reject plugins root regular file");

        assert!(matches!(error, ScaffoldError::InvalidPluginsRoot { .. }));
        assert_eq!(
            fs::read_to_string(&plugins_root).expect("read plugins root file"),
            "preserve me"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_replace_a_dangling_destination_symlink() {
        use std::os::unix::fs::symlink;

        let repository = TestRepository::new();
        let plugins_root = repository.path().join("plugins");
        let destination = plugins_root.join("my-plugin");
        fs::create_dir_all(&plugins_root).expect("create plugins directory");
        symlink("missing-plugin-target", &destination).expect("create dangling symlink");

        let error =
            create_plugin(repository.path(), "my-plugin").expect_err("refuse dangling symlink");

        assert!(matches!(error, ScaffoldError::AlreadyExists { .. }));
        let metadata = fs::symlink_metadata(&destination).expect("preserve dangling symlink");
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            fs::read_link(&destination).expect("read dangling symlink target"),
            PathBuf::from("missing-plugin-target")
        );
    }

    #[test]
    fn preexisting_lock_refuses_creation_without_modifying_the_lock() {
        let repository = TestRepository::new();
        let plugins_root = repository.path().join("plugins");
        let lock_path = plugins_root.join(".my-plugin.scaffold.lock");
        fs::create_dir_all(&plugins_root).expect("create plugins directory");
        fs::write(&lock_path, "owned by another creator").expect("write existing lock");

        let error = create_plugin(repository.path(), "my-plugin").expect_err("report busy plugin");

        assert!(matches!(error, ScaffoldError::Busy { .. }));
        assert_eq!(
            fs::read_to_string(&lock_path).expect("read existing lock"),
            "owned by another creator"
        );
        assert!(!plugins_root.join("my-plugin").exists());
        assert_eq!(
            fs::read_dir(&plugins_root)
                .expect("read plugins directory")
                .count(),
            1
        );
    }

    #[test]
    fn lock_guard_preserves_a_replacement_lock_entry() {
        let repository = TestRepository::new();
        let plugins_root = repository.path().join("plugins");
        fs::create_dir_all(&plugins_root).expect("create plugins directory");
        let lock = ScaffoldLock::acquire(&plugins_root, "my-plugin").expect("acquire lock");
        let lock_path = plugins_root.join(".my-plugin.scaffold.lock");
        fs::remove_file(&lock_path).expect("remove owned lock entry");
        fs::write(&lock_path, "replacement owned elsewhere").expect("write replacement lock");

        drop(lock);

        assert_eq!(
            fs::read_to_string(&lock_path).expect("read replacement lock"),
            "replacement owned elsewhere"
        );
    }

    #[test]
    fn final_root_is_absent_while_staged_plugin_is_materialized() {
        let repository = TestRepository::new();
        let plugins_root = repository.path().join("plugins");
        let final_root = plugins_root.join("my-plugin");
        fs::create_dir_all(&plugins_root).expect("create plugins directory");
        let lock = ScaffoldLock::acquire(&plugins_root, "my-plugin").expect("acquire lock");
        let identity = PluginsRootIdentity::capture(&plugins_root).expect("record plugins root");

        let staged =
            StagedPlugin::materialize(&plugins_root, "my-plugin").expect("materialize staging");

        assert!(!final_root.exists());
        assert_eq!(staged.path().parent(), Some(plugins_root.as_path()));
        assert!(
            staged
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".my-plugin.tmp-"))
        );
        assert!(staged.path().join("Cargo.toml").is_file());
        assert!(staged.path().join("src/lib.rs").is_file());

        staged
            .publish(&final_root, &identity, &lock)
            .expect("publish staged plugin");
        assert!(final_root.join("Cargo.toml").is_file());
        assert!(final_root.join("src/lib.rs").is_file());
    }

    #[test]
    fn publish_refuses_a_colliding_destination_and_preserves_it() {
        let repository = TestRepository::new();
        let plugins_root = repository.path().join("plugins");
        let final_root = plugins_root.join("my-plugin");
        fs::create_dir_all(&plugins_root).expect("create plugins directory");
        let lock = ScaffoldLock::acquire(&plugins_root, "my-plugin").expect("acquire lock");
        let identity = PluginsRootIdentity::capture(&plugins_root).expect("record plugins root");
        let staged =
            StagedPlugin::materialize(&plugins_root, "my-plugin").expect("materialize staging");
        let staging_root = staged.path().to_path_buf();
        fs::write(&final_root, "preserve me").expect("write colliding destination file");

        let error = staged
            .publish(&final_root, &identity, &lock)
            .expect_err("report destination collision");

        assert!(matches!(error, ScaffoldError::AlreadyExists { .. }));
        assert_eq!(
            fs::read_to_string(&final_root).expect("read collision sentinel"),
            "preserve me"
        );
        assert!(!staging_root.exists());
    }

    #[test]
    fn refuses_to_replace_an_existing_plugin_directory() {
        let repository = TestRepository::new();
        let existing = repository.path().join("plugins/my-plugin");
        fs::create_dir_all(&existing).expect("create existing plugin directory");
        fs::write(existing.join("keep.txt"), "preserve me").expect("write collision sentinel");

        let error =
            create_plugin(repository.path(), "my-plugin").expect_err("refuse plugin collision");

        assert!(matches!(error, ScaffoldError::AlreadyExists { .. }));
        assert_eq!(
            fs::read_to_string(existing.join("keep.txt")).expect("read collision sentinel"),
            "preserve me"
        );
        assert_eq!(
            fs::read_dir(&existing)
                .expect("read existing directory")
                .count(),
            1
        );
    }
}

use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// 统一路径配置
///
/// 默认路径相对于进程工作目录；嵌入方可通过 with_root 指定部署根目录。
/// 消除代码中硬编码的 "nuono/plugins" 等字符串。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paths {
    pub root: PathBuf,
    pub plugins: PathBuf,
    pub scripts: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
    pub templates: PathBuf,
    pub web_dist: PathBuf,
}

impl Paths {
    /// 基于仓库根目录的默认路径
    pub fn root() -> Self {
        let root = PathBuf::from(".");
        Self {
            plugins: root.join("plugins"),
            scripts: root.join("scripts"),
            data: root.join("data"),
            logs: root.join("logs"),
            templates: root.join("templates"),
            web_dist: root.join("nuono-hub").join("dist"),
            root,
        }
    }

    /// 基于自定义根目录的路径
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            plugins: root.join("plugins"),
            scripts: root.join("scripts"),
            data: root.join("data"),
            logs: root.join("logs"),
            templates: root.join("templates"),
            web_dist: root.join("nuono-hub").join("dist"),
            root,
        }
    }

    /// 管理界面的持久资产，与插件自己的数据目录分开。
    pub fn hub(&self) -> PathBuf {
        self.data.join("hub")
    }

    pub fn wallpapers(&self) -> PathBuf {
        self.hub().join("wallpapers")
    }

    pub fn identity_assets(&self) -> PathBuf {
        self.hub().join("identity-assets")
    }

    /// 宿主管理的 AI 角色素材，按角色稳定 ID 隔离。
    pub fn persona_assets(&self) -> PathBuf {
        self.hub().join("persona-assets")
    }

    pub fn is_reserved_plugin_name(name: &str) -> bool {
        nuono_domain::deployment::is_reserved_plugin_name(name)
    }

    /// 启动时迁移历史 Hub 资产，保留文件名和数据库中的相对引用。
    ///
    /// 先检查全部源/目标，再重命名；不合并、不覆盖已有目标。重启可继续未完成迁移。
    /// 此操作应在文件服务和插件启动之前执行，不迁移插件业务目录或数据库备份。
    pub fn migrate_legacy_hub_assets(&self) -> io::Result<usize> {
        let hub = self.hub();
        let hub_exists = directory_exists(&hub)?;
        if hub_exists && entry_exists(&hub.join("config.toml"))? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "data/hub 被已有插件配置占用，请先迁移该插件；不会覆盖插件数据",
            ));
        }
        let mut pending = Vec::new();
        for name in ["wallpapers", "identity-assets"] {
            let source = self.data.join(name);
            if !directory_exists(&source)? {
                continue;
            }
            if entry_exists(&source.join("config.toml"))? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} 包含插件配置，拒绝作为 Hub 资产迁移", source.display()),
                ));
            }
            let destination = hub.join(name);
            if entry_exists(&destination)? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Hub 资产目录冲突：{} 与 {} 同时存在，请先核对并整理；未覆盖文件",
                        source.display(),
                        destination.display()
                    ),
                ));
            }
            pending.push((source, destination));
        }
        if !pending.is_empty() {
            fs::create_dir_all(&hub)?;
        }
        for (source, destination) in &pending {
            fs::rename(source, destination)?;
        }
        Ok(pending.len())
    }

    /// 获取指定插件的数据目录
    pub fn plugin_data(&self, plugin_name: &str) -> PathBuf {
        self.data.join(plugin_name)
    }

    /// 确保数据与日志根目录仅属主可访问（unix 0700；幂等重设历史宽松安装）。
    ///
    /// 明文密钥（`runtime_secrets`、SQLite 整库）的既定安全边界是文件系统权限
    /// （见 `entity/runtime_secrets` 的注释），这里把边界真正建立起来。chmod
    /// 失败只告警不阻断：权限是纵深防御而非功能前提，在特殊文件系统上硬失败
    /// 会把实例锁死在启动。
    pub fn ensure_private_dirs(&self) {
        restrict_private_dir(&self.data);
        restrict_private_dir(&self.logs);
    }
}

/// 目录不存在则以 0700 创建（连同缺失的父目录）；已存在的真实目录幂等重设为 0700。
///
/// 通过软链接指向的目录不追着 chmod：链接目标可能是部署者与其他服务共享的位置，
/// 静默收紧会破坏别人的写入，此时只依赖部署者自己的权限设置。
#[cfg(unix)]
pub(crate) fn restrict_private_dir(path: &Path) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                tracing::debug!(path = %path.display(), "私有目录是软链接，跳过权限收紧");
                return;
            }
            if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
                tracing::warn!(path = %path.display(), %error, "收紧私有目录权限失败");
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            if let Err(error) = builder.create(path) {
                tracing::warn!(path = %path.display(), %error, "创建私有目录失败");
            }
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "检查私有目录状态失败");
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn restrict_private_dir(_path: &Path) {}

fn directory_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("资产路径必须是实际目录：{}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn entry_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

impl Default for Paths {
    fn default() -> Self {
        Self::root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("nuono-hub-paths-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn paths(&self) -> Paths {
            Paths::with_root(&self.0)
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_dirs_are_created_and_retightened_at_0700() {
        use std::os::unix::fs::PermissionsExt;
        let root = TestRoot::new();
        let paths = root.paths();
        paths.ensure_private_dirs();
        let mode = |dir: &str| fs::metadata(root.0.join(dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("data"), 0o700);
        assert_eq!(mode("logs"), 0o700);

        // 历史宽松安装：已存在的 0755 目录被幂等重设为 0700。
        fs::set_permissions(paths.data.clone(), fs::Permissions::from_mode(0o755)).unwrap();
        paths.ensure_private_dirs();
        assert_eq!(mode("data"), 0o700);

        // 软链接目录不追着 chmod：目标可能是部署者与其他服务共享的位置。
        let shared = root.0.join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.0.join("data-link");
        std::os::unix::fs::symlink(&shared, &link).unwrap();
        restrict_private_dir(&link);
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o755,
            "软链接目标不被收紧"
        );
    }

    #[test]
    fn migrates_legacy_assets_preserving_bytes_and_plugin_data() {
        let root = TestRoot::new();
        let paths = root.paths();
        for (name, content) in [
            ("wallpapers/image.png", b"wallpaper".as_slice()),
            ("identity-assets/profile/avatar.png", b"avatar".as_slice()),
            ("sample/files/report.pdf", b"report".as_slice()),
        ] {
            let file = paths.data.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }
        assert_eq!(paths.migrate_legacy_hub_assets().unwrap(), 2);
        assert_eq!(
            fs::read(paths.wallpapers().join("image.png")).unwrap(),
            b"wallpaper"
        );
        assert_eq!(
            fs::read(paths.identity_assets().join("profile/avatar.png")).unwrap(),
            b"avatar"
        );
        assert_eq!(
            fs::read(paths.plugin_data("sample").join("files/report.pdf")).unwrap(),
            b"report"
        );
        assert!(!paths.data.join("wallpapers").exists());
        assert_eq!(paths.migrate_legacy_hub_assets().unwrap(), 0);
    }

    #[test]
    fn conflicts_leave_both_sources_and_destination_intact() {
        let root = TestRoot::new();
        let paths = root.paths();
        fs::create_dir_all(paths.data.join("wallpapers")).unwrap();
        fs::create_dir_all(paths.data.join("identity-assets")).unwrap();
        fs::create_dir_all(paths.identity_assets()).unwrap();
        fs::write(paths.data.join("identity-assets/old.png"), b"old").unwrap();
        fs::write(paths.identity_assets().join("new.png"), b"new").unwrap();
        assert_eq!(
            paths.migrate_legacy_hub_assets().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(paths.data.join("wallpapers").is_dir());
        assert_eq!(
            fs::read(paths.data.join("identity-assets/old.png")).unwrap(),
            b"old"
        );
        assert_eq!(
            fs::read(paths.identity_assets().join("new.png")).unwrap(),
            b"new"
        );
    }

    #[test]
    fn plugin_named_hub_is_not_repurposed() {
        let root = TestRoot::new();
        let paths = root.paths();
        fs::create_dir_all(paths.hub()).unwrap();
        fs::write(paths.hub().join("config.toml"), b"count = 42").unwrap();
        assert!(paths.migrate_legacy_hub_assets().is_err());
        assert_eq!(
            fs::read(paths.hub().join("config.toml")).unwrap(),
            b"count = 42"
        );
        assert!(Paths::is_reserved_plugin_name("Hub"));
    }

    #[test]
    fn legacy_plugin_directory_is_not_moved_as_assets() {
        let root = TestRoot::new();
        let paths = root.paths();
        fs::create_dir_all(paths.data.join("wallpapers")).unwrap();
        fs::write(paths.data.join("wallpapers/config.toml"), b"enabled = true").unwrap();
        assert!(paths.migrate_legacy_hub_assets().is_err());
        assert!(paths.data.join("wallpapers/config.toml").exists());
        assert!(!paths.hub().exists());
    }

    #[test]
    fn fresh_deployments_do_not_create_unused_asset_directories() {
        let root = TestRoot::new();
        let paths = root.paths();
        assert_eq!(paths.migrate_legacy_hub_assets().unwrap(), 0);
        assert!(!paths.data.exists());
        assert_eq!(paths.web_dist, root.0.join("nuono-hub/dist"));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_roots_without_modifying_the_target() {
        let root = TestRoot::new();
        let paths = root.paths();
        fs::create_dir_all(&paths.data).unwrap();
        let target = root.0.join("external");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.png"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target, paths.data.join("wallpapers")).unwrap();
        assert!(paths.migrate_legacy_hub_assets().is_err());
        assert_eq!(fs::read(target.join("keep.png")).unwrap(), b"keep");
    }
}

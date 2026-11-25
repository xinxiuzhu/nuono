//! 在调用 Cargo 编译前，读取并校验插件根目录下的可选图标文件字节。
use anyhow::{Context, Result, ensure};
use std::{fmt::Write, fs, io::Read, path::Path};

use super::{ICON_FILES, MAX_ICON_BYTES};

pub struct EmbeddedIcon {
    content_type: &'static str,
    bytes: Vec<u8>,
}

impl EmbeddedIcon {
    /// 生成包含已校验图标内容的 Rust 字节字符串字面量，使 `build.rs` 和 `rustc` 后续无需再访问插件图标路径或跟随符号链接。
    pub fn registration(&self) -> String {
        let mut literal = String::with_capacity(self.bytes.len() * 4);
        for byte in &self.bytes {
            write!(&mut literal, "\\x{byte:02x}").unwrap();
        }
        format!(
            "nuono_server::register_plugin_icon(plugin.name(), {:?}, b\"{}\");",
            self.content_type, literal
        )
    }
}

pub fn load(root: &Path) -> Result<Option<EmbeddedIcon>> {
    // 测试代码中可能会传入尚未落地创建的虚拟插件路径；图标文件本身是可选的，但一旦存在则必须是通过大小和类型校验的普通文件。
    for (name, content_type) in ICON_FILES {
        let path = root.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("无法读取插件图标 {}", path.display()));
            }
        };
        ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "插件图标必须是普通文件且不能是符号链接: {}",
            path.display()
        );
        ensure!(
            metadata.len() > 0 && metadata.len() <= MAX_ICON_BYTES as u64,
            "插件图标必须非空且不能超过 512 KiB: {}",
            path.display()
        );
        let canonical_root = root.canonicalize()?;
        let canonical_path = path.canonicalize()?;
        ensure!(
            canonical_path.parent() == Some(canonical_root.as_path()),
            "插件图标必须位于 Cargo.toml 同级的插件根目录: {}",
            path.display()
        );
        let file = fs::File::open(&canonical_path)
            .with_context(|| format!("无法打开插件图标 {}", path.display()))?;
        ensure!(
            file.metadata()?.is_file(),
            "插件图标不是普通文件: {}",
            path.display()
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_ICON_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_ICON_BYTES,
            "插件图标读取时超出 512 KiB 或为空: {}",
            path.display()
        );
        // 拒绝在读取过程中被并发替换为符号链接的图标文件。
        ensure!(
            fs::symlink_metadata(&path)?.file_type().is_file()
                && path.canonicalize()? == canonical_path,
            "读取期间插件图标路径已变化: {}",
            path.display()
        );
        return Ok(Some(EmbeddedIcon {
            content_type,
            bytes,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_icon_snapshot_respects_package_root_order_and_missing_files() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/icon.svg"), b"business icon").unwrap();
        assert!(load(root.path()).unwrap().is_none());
        fs::write(root.path().join("icon.png"), b"png").unwrap();
        fs::write(root.path().join("icon.svg"), b"svg").unwrap();
        let icon = load(root.path()).unwrap().unwrap();
        assert_eq!(icon.content_type, "image/svg+xml");
        assert_eq!(icon.bytes, b"svg");
        fs::write(root.path().join("icon.svg"), b"modified").unwrap();
        assert!(icon.registration().contains("b\"\\x73\\x76\\x67\""));
    }

    #[test]
    fn plugin_icon_snapshot_rejects_oversized_and_non_file_candidates() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("icon.png"), vec![0; MAX_ICON_BYTES + 1]).unwrap();
        assert!(load(root.path()).is_err());
        fs::remove_file(root.path().join("icon.png")).unwrap();
        fs::create_dir(root.path().join("icon.svg")).unwrap();
        assert!(load(root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn plugin_icon_snapshot_rejects_symbolic_links_outside_package() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("icon.svg"), b"outside").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("icon.svg"),
            root.path().join("icon.svg"),
        )
        .unwrap();
        assert!(load(root.path()).is_err());
    }
}

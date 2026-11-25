#[path = "../nuono-tooling/build_support.rs"]
mod build_support;

fn main() {
    build_support::emit_build_metadata("BuildMetadata", "BuildDependencyMetadata");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let profile =
        build_support::build_profile::from_out_dir(&out).expect("recognized Cargo OUT_DIR layout");
    println!("cargo:rustc-env=NUONO_BUILD_PROFILE={profile}");
    println!("cargo:rerun-if-changed=build.rs");
    let root =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    println!(
        "cargo:rustc-env=NUONO_STORAGE_FINGERPRINT={}",
        fingerprint(
            &root,
            &[
                "crates/nuono-storage/migrations",
                "crates/nuono-storage/migrations_sqlite"
            ]
        )
    );
    // 独立的 #[cfg(test)] 测试模块统一放在 tests/unit 下，不混入以下生产代码根目录。
    // 不要按文件名排除：test_support 模块同样是生产构建的输入。
    println!(
        "cargo:rustc-env=NUONO_FRAMEWORK_FINGERPRINT={}",
        fingerprint(
            &root,
            &[
                "crates/nuono-server/src",
                "crates/nuono-server/build.rs",
                "crates/nuono-tooling/build_support.rs",
                "crates/nuono-tooling/build_profile.rs",
                "crates/nuono-tooling/Cargo.toml",
                "crates/nuono-tooling/build.rs",
                "crates/nuono-tooling/templates",
                "crates/nuono-core/src",
                "crates/nuono-storage/src",
                "crates/nuono-plugin/src",
                "crates/nuono-tooling/src"
            ]
        )
    );
}

fn fingerprint(root: &std::path::Path, paths: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    fn visit(root: &std::path::Path, path: &std::path::Path, hash: &mut Sha256) {
        if path == root.join("crates/nuono-tooling/src/bin") {
            return;
        }
        // 监听具体文件而非整个 tooling 目录，因为 bin 子目录改动也会刷新该目录的 mtime。
        if !path.is_dir() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        if path.is_dir() {
            let mut entries = std::fs::read_dir(path)
                .expect("fingerprint directory")
                .map(|e| e.expect("fingerprint entry").path())
                .collect::<Vec<_>>();
            entries.sort();
            for entry in entries {
                visit(root, &entry, hash);
            }
        } else if path.is_file() {
            let name = path.strip_prefix(root).unwrap().to_string_lossy();
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            let bytes = std::fs::read(path).expect("fingerprint source");
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(&bytes);
        }
    }
    let mut hash = Sha256::new();
    for path in paths {
        visit(root, &root.join(path), &mut hash);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

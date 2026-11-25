pub fn find_nuono_repository_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    start
        .ancestors()
        .find(|candidate| is_nuono_repository_root(candidate))
        .map(std::path::Path::to_path_buf)
}

fn is_nuono_repository_root(candidate: &std::path::Path) -> bool {
    let root_manifest = candidate.join("Cargo.toml");
    let server_manifest = candidate.join("crates/nuono-server/Cargo.toml");

    let Ok(root_manifest) = std::fs::read_to_string(root_manifest) else {
        return false;
    };
    let Ok(root_manifest) = toml::from_str::<toml::Value>(&root_manifest) else {
        return false;
    };
    let has_server_workspace_member = root_manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .is_some_and(|members| {
            members
                .iter()
                .any(|member| member.as_str() == Some("crates/nuono-server"))
        });
    if !has_server_workspace_member {
        return false;
    }

    let Ok(server_manifest) = std::fs::read_to_string(server_manifest) else {
        return false;
    };
    let Ok(server_manifest) = toml::from_str::<toml::Value>(&server_manifest) else {
        return false;
    };
    server_manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        == Some("nuono-server")
}

#[cfg(test)]
mod repository_root_tests {
    use super::find_nuono_repository_root;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct TestDirectory {
        root: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "nuono-repository-root-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&root).expect("create test directory");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write_nuono_markers(&self) {
            let server_dir = self.root.join("crates/nuono-server");
            fs::create_dir_all(&server_dir).expect("create server crate directory");
            fs::write(
                self.root.join("Cargo.toml"),
                "[workspace]\nmembers = [\"crates/nuono-server\"]\n",
            )
            .expect("write root manifest");
            fs::write(
                server_dir.join("Cargo.toml"),
                "[package]\nname = \"nuono-server\"\n",
            )
            .expect("write server manifest");
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn repository_root_resolves_from_root_directory() {
        let repository = TestDirectory::new();
        repository.write_nuono_markers();

        assert_eq!(
            find_nuono_repository_root(repository.path()),
            Some(repository.path().to_path_buf())
        );
    }

    #[test]
    fn repository_root_resolves_from_nested_directory() {
        let repository = TestDirectory::new();
        repository.write_nuono_markers();
        let nested = repository.path().join("crates/nuono-server/src");
        fs::create_dir_all(&nested).expect("create nested directory");

        assert_eq!(
            find_nuono_repository_root(&nested),
            Some(repository.path().to_path_buf())
        );
    }

    #[test]
    fn repository_root_rejects_unrelated_cargo_project() {
        let project = TestDirectory::new();
        let server_dir = project.path().join("crates/nuono-server");
        fs::create_dir_all(&server_dir).expect("create lookalike crate directory");
        fs::write(
            project.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/other-server\"]\n",
        )
        .expect("write unrelated root manifest");
        fs::write(
            server_dir.join("Cargo.toml"),
            "[package]\nname = \"other-server\"\n",
        )
        .expect("write unrelated server manifest");

        assert_eq!(find_nuono_repository_root(project.path()), None);
        assert!(!project.path().join("plugins").exists());
    }
}

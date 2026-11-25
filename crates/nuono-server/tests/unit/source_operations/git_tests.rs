use super::*;
use crate::source_operations::empty_catalog_item;
use std::path::PathBuf;
use std::process::Command;

struct Repository(PathBuf);
impl Repository {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("nuono-git-history-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        let repo = Self(path);
        repo.command(&["init", "--initial-branch=main"]);
        std::fs::write(
            repo.0.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        repo.commit("first");
        repo
    }
    fn command(&self, args: &[&str]) -> String {
        let mut command = Command::new("git");
        configure_command(&mut command);
        let output = command
            .args(["-C"])
            .arg(&self.0)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
    fn commit(&self, subject: &str) -> String {
        std::fs::write(self.0.join("code.txt"), subject).unwrap();
        self.command(&["add", "."]);
        self.command(&["commit", "-m", subject]);
        self.command(&["rev-parse", "HEAD"])
    }
}
impl Drop for Repository {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn history_pages_stay_on_an_exact_reference_and_tags_resolve_to_commits() {
    let repo = Repository::new();
    let first = repo.command(&["rev-parse", "HEAD"]);
    repo.command(&["tag", "-a", "v0.1", "-m", "release"]);
    repo.command(&["branch", "stable"]);
    let mut latest = first.clone();
    for n in 1..23 {
        latest = repo.commit(&format!("change {n}"));
    }
    repo.command(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/demo.git?token=private",
    ]);
    repo.command(&["update-ref", "refs/remotes/origin/main", &latest]);
    repo.command(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    repo.command(&["checkout", "--detach", &first]);
    let running = source_build::source_identity(&repo.0).unwrap();
    let first_page = local_history("demo", &repo.0, Some(&running), &HistoryQuery::default())
        .await
        .unwrap();
    assert!(first_page.available);
    assert!(first_page.branch.is_none());
    assert_eq!(first_page.source_commit.as_deref(), Some(first.as_str()));
    assert_eq!(first_page.reference.as_deref(), Some(latest.as_str()));
    assert_eq!(first_page.commits.len(), PAGE_SIZE);
    assert_eq!(first_page.next_offset, Some(20));
    assert!(!first_page.pending_restart);
    assert_eq!(
        first_page.repo_url.as_deref(),
        Some("https://example.invalid/demo.git")
    );
    assert!(
        first_page
            .refs
            .iter()
            .any(|r| r.name == "v0.1" && r.kind == "tag" && r.commit == first)
    );
    assert!(
        first_page
            .refs
            .iter()
            .any(|r| r.name == "stable" && r.kind == "branch" && r.commit == first)
    );
    let last_page = local_history(
        "demo",
        &repo.0,
        Some(&running),
        &HistoryQuery {
            reference: Some(latest.clone()),
            offset: 20,
        },
    )
    .await
    .unwrap();
    assert_eq!(last_page.commits.len(), 3);
    assert_eq!(last_page.next_offset, None);
    assert_eq!(last_page.commits.last().unwrap().commit, first);
    assert!(
        first_page
            .commits
            .iter()
            .all(|row| !row.author.is_empty() && !row.date.is_empty())
    );
    let old = local_history(
        "demo",
        &repo.0,
        Some(&running),
        &HistoryQuery {
            reference: Some(first),
            offset: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(old.commits.len(), 1);
}

#[tokio::test]
async fn source_changes_require_restart_even_when_version_and_commit_match() {
    let repo = Repository::new();
    let running = source_build::source_identity(&repo.0).unwrap();
    std::fs::write(repo.0.join("code.txt"), "unsaved local edit").unwrap();
    let view = local_history("demo", &repo.0, Some(&running), &HistoryQuery::default())
        .await
        .unwrap();
    assert!(view.pending_restart && view.dirty);
    assert_eq!(view.source_commit, running.commit);
    assert_eq!(
        view.source_version.as_deref(),
        Some(running.version.as_str())
    );
}

#[tokio::test]
async fn empty_commit_still_reports_a_different_running_git_version() {
    let repo = Repository::new();
    let running = source_build::source_identity(&repo.0).unwrap();
    repo.command(&["commit", "--allow-empty", "-m", "metadata only"]);
    let source = source_build::source_identity(&repo.0).unwrap();
    assert_eq!(source.source_hash, running.source_hash);
    let view = local_history("demo", &repo.0, Some(&running), &HistoryQuery::default())
        .await
        .unwrap();
    assert!(view.pending_restart);
    assert_ne!(view.source_commit, view.running_commit);
}

#[tokio::test]
async fn dirty_detection_preserves_changes_but_ignores_our_pin_marker() {
    let repo = Repository::new();
    assert!(!source_dirty(&repo.0).await.unwrap());
    std::fs::write(repo.0.join(".nuono-pin"), "marker").unwrap();
    assert!(!source_dirty(&repo.0).await.unwrap());
    std::fs::write(repo.0.join("untracked.txt"), "keep me").unwrap();
    assert!(source_dirty(&repo.0).await.unwrap());
    std::fs::remove_file(repo.0.join("untracked.txt")).unwrap();
    std::fs::write(repo.0.join("code.txt"), "modified").unwrap();
    assert!(source_dirty(&repo.0).await.unwrap());
}

#[tokio::test]
async fn parent_repository_and_worktree_pointer_cannot_be_managed_as_a_plugin() {
    let repo = Repository::new();
    let plugin = repo.0.join("plugins/demo");
    std::fs::create_dir_all(&plugin).unwrap();
    assert!(independent_repository(&plugin).await.is_err());
    std::fs::write(
        plugin.join(".git"),
        format!("gitdir: {}\n", repo.0.join(".git").display()),
    )
    .unwrap();
    assert!(independent_repository(&plugin).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn linked_git_directory_and_plugin_directory_are_rejected() {
    let repo = Repository::new();
    let plugin = repo.0.join("plugins/demo");
    std::fs::create_dir_all(&plugin).unwrap();
    std::os::unix::fs::symlink(repo.0.join(".git"), plugin.join(".git")).unwrap();
    assert!(independent_repository(&plugin).await.is_err());
    let link = repo.0.join("linked");
    std::os::unix::fs::symlink(&repo.0, &link).unwrap();
    assert!(independent_repository(&link).await.is_err());
}

#[tokio::test]
async fn ordinary_git_directory_cannot_share_another_repositorys_common_data() {
    let repo = Repository::new();
    let plugin = repo.0.join("plugins/shared");
    std::fs::create_dir_all(plugin.join(".git")).unwrap();
    std::fs::write(plugin.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        plugin.join(".git/commondir"),
        format!("{}\n", repo.0.join(".git").display()),
    )
    .unwrap();
    assert!(independent_repository(&plugin).await.is_err());
}

#[tokio::test]
async fn stale_expected_commit_and_reference_option_injection_are_rejected() {
    let repo = Repository::new();
    let previous = current_commit(&repo.0).await.unwrap();
    let newer = repo.commit("newer");
    assert!(expected_head(&repo.0, &previous).await.is_err());
    assert!(expected_head(&repo.0, &newer).await.is_ok());
    for malicious in [
        "--all",
        "HEAD",
        "main",
        "abcd",
        "0000000000000000000000000000000000000000",
    ] {
        assert!(verify_commit(&repo.0, malicious).await.is_err());
        assert!(
            local_history(
                "demo",
                &repo.0,
                None,
                &HistoryQuery {
                    reference: Some(malicious.into()),
                    offset: 0
                }
            )
            .await
            .is_err()
        );
    }
    assert!(
        local_history(
            "demo",
            &repo.0,
            None,
            &HistoryQuery {
                reference: None,
                offset: MAX_OFFSET + 1
            }
        )
        .await
        .is_err()
    );
}

#[test]
fn signatures_cannot_be_bypassed_by_selecting_a_git_commit() {
    let repo = "https://example.invalid/demo.git";
    let commit = "1111111111111111111111111111111111111111";
    let mut cfg = nuono_core::config::PluginStoreConfig::default();
    assert!(target_policy(&cfg, None, repo, commit).is_ok());
    let mut plugin = empty_catalog_item("demo");
    plugin.repo_url = repo.into();
    plugin.pinned_commit = Some(commit.into());
    assert!(
        target_policy(
            &cfg,
            Some(&plugin),
            "https://other.invalid/demo.git",
            commit
        )
        .is_err()
    );
    cfg.require_signature = true;
    assert!(target_policy(&cfg, None, repo, commit).is_err());
    assert!(target_policy(&cfg, Some(&plugin), repo, commit).is_err());
    let secret = hex::encode([1u8; 32]);
    let key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
    cfg.signing_public_key = Some(hex::encode(key.verifying_key().to_bytes()));
    plugin.signature =
        Some(nuono_core::service::plugin_store::sign_commit(repo, commit, &secret).unwrap());
    assert!(target_policy(&cfg, Some(&plugin), repo, commit).is_ok());
    assert!(
        target_policy(
            &cfg,
            Some(&plugin),
            repo,
            "2222222222222222222222222222222222222222"
        )
        .unwrap_err()
        .contains("签名策略")
    );
}

#[test]
fn repository_links_hide_credentials_and_queries() {
    assert_eq!(
        safe_repo_url("https://alice:password@example.invalid/demo.git"),
        None
    );
    assert_eq!(
        safe_repo_url("https://example.invalid/demo.git?token=secret"),
        Some("https://example.invalid/demo.git".into())
    );
    assert_eq!(
        safe_repo_url("ssh://alice@example.invalid/demo.git?key=secret"),
        Some("ssh://example.invalid/demo.git".into())
    );
    assert_eq!(safe_repo_url("file:///private/demo"), None);
}

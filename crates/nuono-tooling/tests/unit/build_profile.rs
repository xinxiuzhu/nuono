use super::from_out_dir;
use std::path::Path;

#[test]
fn preserves_profiles_across_cargo_layouts_targets_and_separate_build_directories() {
    for base in [
        "target",
        "target/aarch64-unknown-linux-gnu",
        "cache with spaces/build/custom-root",
    ] {
        for profile in ["debug", "release", "production"] {
            for unit in [
                "nuono-tooling-0123456789abcdef",
                "nuono-tooling/0123456789abcdef",
            ] {
                let path = format!("{base}/{profile}/build/{unit}/out");
                assert_eq!(from_out_dir(Path::new(&path)), Some(profile), "{path}");
            }
        }
    }
}

#[test]
fn rejects_unrecognized_layouts_instead_of_returning_an_unrelated_directory() {
    for path in [
        "out",
        "target/debug/deps/unit/out",
        "target/debug/build/unit/not-out",
    ] {
        assert_eq!(from_out_dir(Path::new(path)), None, "{path}");
    }
}

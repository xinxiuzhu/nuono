mod build_profile;

fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let profile = build_profile::from_out_dir(&out).expect("recognized Cargo OUT_DIR layout");
    println!("cargo:rustc-env=NUONO_BUILD_PROFILE={profile}");
    println!(
        "cargo:rustc-env=NUONO_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_profile.rs");
}

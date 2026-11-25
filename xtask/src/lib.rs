pub mod architecture;
pub mod plugin_checks;
pub mod registry;
pub mod source_startup;

pub fn repository_root() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the repository root")
}

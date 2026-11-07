//! Make sure `ui/dist` exists so `include_dir!` compiles before the UI has been built.
fn main() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(&dist);
    }
    println!("cargo:rerun-if-changed=../../ui/dist");
}

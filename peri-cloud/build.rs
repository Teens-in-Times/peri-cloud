// MSVC embeds a PDB reference even when the PDB is not distributed.
// Keep that reference independent of the builder's private directory layout.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/PDBALTPATH:%_PDB%");
    }
}

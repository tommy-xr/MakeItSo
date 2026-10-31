//! Bakes the target triple in: it names this build's release asset.
fn main() {
    println!(
        "cargo:rustc-env=MIS_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}

//! The switch for the testkit corpus test in `tests/suites.rs`: a non-empty `FL_CORPUS` sets
//! the `fl_corpus` cfg that runs it, and without one `cargo test` lists that test as ignored.
//! Only this repository needs it, so the published crate leaves both files out.

fn main() {
    println!("cargo:rerun-if-env-changed=FL_CORPUS");
    println!("cargo:rustc-check-cfg=cfg(fl_corpus)");
    if std::env::var_os("FL_CORPUS").is_some_and(|corpus| !corpus.is_empty()) {
        println!("cargo:rustc-cfg=fl_corpus");
    }
}

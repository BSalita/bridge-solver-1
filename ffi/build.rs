//! Name the shared library by where it will be found, not where it was built.
//!
//! By default a macOS dylib records its own absolute build path as its
//! install name, and a program linked against it looks there at run time --
//! a directory on whatever machine built the release. `@rpath` makes the
//! program look in its own run-path list instead, which the program sets with
//! `-Wl,-rpath,<dir>`. On Linux the equivalent is the soname.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libbridgesolver.dylib")
        }
        Ok("linux") => println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libbridgesolver.so"),
        _ => {}
    }
}

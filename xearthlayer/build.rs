fn main() {
    // intel_tex_2 links a static ISPC library that requires the C++ runtime.
    // The crate's build script doesn't emit this linkage, so we add it here.
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-lib=stdc++");

    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=c++");
}

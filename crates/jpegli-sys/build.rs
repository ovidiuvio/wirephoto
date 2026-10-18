//! Builds vendor/ (jpegli and Highway) and shim/ with CMake into static libraries and links them, with the C++
//! standard library.

fn main() {
    println!("cargo:rerun-if-changed=CMakeLists.txt");
    println!("cargo:rerun-if-changed=shim");
    println!("cargo:rerun-if-changed=vendor");
    let dst = cmake::Config::new(".")
        .profile("Release")
        .define("CMAKE_BUILD_TYPE", "Release")
        .build();
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=jpegli");
    println!("cargo:rustc-link-lib=static=hwy");
    let target = std::env::var("TARGET").unwrap();
    if target.contains("apple") {
        println!("cargo:rustc-link-lib=c++");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}

use std::env;
use std::path::PathBuf;

fn main() {
    // 链接到3FS构建产物
    let hf3fs_build_dir = env::var("HF3FS_BUILD_DIR")
        .expect("HF3FS_BUILD_DIR environment variable must be set");

    println!("cargo::rustc-link-search=native={}/src/lib/api", hf3fs_build_dir);
    println!("cargo::rustc-link-lib=hf3fs_api_shared");

    // 使用本地头文件生成绑定
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let header_path = PathBuf::from(&manifest_dir)
        .join("include")
        .join("hf3fs_usrbio.h");

    let bindings = bindgen::Builder::default()
        .header(header_path.display().to_string())
        .clang_arg("-std=c99")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

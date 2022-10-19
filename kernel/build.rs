fn main() {
    println!("cargo:rerun-if-env-changed=LOG");
    println!("cargo:rerun-if-env-changed=SMP");
    println!("cargo:rerun-if-env-changed=BOARD");
    println!("cargo:rerun-if-env-changed=USER_IMG");

    //let _arch: String = std::env::var("ARCH").unwrap();
    if let Ok(user_img) = std::env::var("USER_IMG") {
        println!("cargo:rerun-if-changed={}", user_img);
    }



    // for shorter #[cfg] check
    //let target = std::env::var("TARGET").unwrap();

    // hardcode riscv64 to cater vscode
    let target = "riscv64".to_string();

    if target.contains("riscv32") {
        println!("cargo:rustc-cfg=riscv");
        println!("cargo:rustc-cfg=riscv32");
    } else if target.contains("riscv64") {
        println!("cargo:rustc-cfg=riscv");
        println!("cargo:rustc-cfg=riscv64");
    } else if target.contains("mipsel") {
        println!("cargo:rustc-cfg=mipsel");
    } else if target.contains("aarch64") {
        println!("cargo:rustc-cfg=aarch64");
    } else if target.contains("x86_64") {
        println!("cargo:rustc-cfg=x86_64");
    }
}

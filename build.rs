fn main() {
    // macOSビルド時に、Swiftランタイムのパスをリンカフラグとして強制追加する
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-link-arg=-L/usr/lib/swift");
}

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("linux") {
        return;
    }
    let Ok(output) = std::process::Command::new("g++")
        .arg("-print-file-name=libstdc++.so")
        .output()
    else {
        return;
    };
    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim();
    if path.is_empty() || path == "libstdc++.so" {
        return;
    }
    if let Some(dir) = std::path::Path::new(path).parent() {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
}

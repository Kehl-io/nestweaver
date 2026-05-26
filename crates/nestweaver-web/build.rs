fn main() {
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package.json");
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/vite.config.ts");
    println!("cargo:rerun-if-changed=frontend/tsconfig.app.json");

    let frontend_dir = std::path::Path::new("frontend");
    let dist_dir = frontend_dir.join("dist");

    if frontend_dir.join("node_modules").exists() {
        let status = std::process::Command::new("npm")
            .args(["run", "build"])
            .current_dir(frontend_dir)
            .status();

        match status {
            Ok(s) if s.success() => {}
            _ => {
                println!(
                    "cargo:warning=Frontend build failed or npm not found, using existing dist"
                );
                ensure_dist_exists(&dist_dir, frontend_dir);
            }
        }
    } else {
        ensure_dist_exists(&dist_dir, frontend_dir);
    }
}

fn ensure_dist_exists(dist_dir: &std::path::Path, frontend_dir: &std::path::Path) {
    std::fs::create_dir_all(dist_dir).ok();
    if !dist_dir.join("index.html").exists()
        && let Ok(html) = std::fs::read_to_string(frontend_dir.join("index.html"))
    {
        std::fs::write(dist_dir.join("index.html"), html).ok();
    }
}

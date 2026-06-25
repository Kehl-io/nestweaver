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
            Ok(s) => {
                println!(
                    "cargo:warning=Frontend build exited with status {s}, using existing dist"
                );
            }
            Err(e) => {
                println!("cargo:warning=Frontend build failed ({e}), using existing dist");
            }
        }
    }

    if !dist_dir.join("assets").exists() || !dist_dir.join("index.html").exists() {
        println!("cargo:warning=frontend/dist/assets/ is missing — the UI will not work.");
        println!(
            "cargo:warning=Run: cd crates/nestweaver-web/frontend && npm install && npm run build"
        );

        // Create a minimal dist so rust-embed doesn't fail compilation,
        // but the UI will show a clear error instead of a blank screen.
        std::fs::create_dir_all(dist_dir.join("assets")).ok();
        let html = r#"<!DOCTYPE html>
<html><head><title>NestWeaver</title></head>
<body><h1>Frontend not built</h1>
<p>Run <code>cd crates/nestweaver-web/frontend &amp;&amp; npm install &amp;&amp; npm run build</code> then rebuild.</p>
</body></html>"#;
        std::fs::write(dist_dir.join("index.html"), html).ok();
    }
}

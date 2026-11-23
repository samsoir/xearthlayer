//! Build script: Tauri code generation.
//!
//! `tauri::generate_context!` embeds `xearthlayer-gui/dist` and refuses to
//! compile when it does not exist. The frontend is built by npm (`make
//! gui-frontend`), which a plain `cargo build`, `cargo test` or `cargo clippy`
//! never runs, so a placeholder page is written when the folder is missing.
//! The real build overwrites it.

use std::path::Path;

const PLACEHOLDER: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>XEarthLayer</title></head>
<body style="font-family: system-ui; padding: 2rem">
<h1>XEarthLayer</h1>
<p>The interface was not built into this binary. Run <code>make gui-frontend</code>
and rebuild, or build with <code>make build</code>.</p>
</body></html>
"#;

fn main() {
    let dist = Path::new("../dist");
    println!("cargo:rerun-if-changed=../dist");
    if !dist.join("index.html").exists() {
        std::fs::create_dir_all(dist).expect("create xearthlayer-gui/dist");
        std::fs::write(dist.join("index.html"), PLACEHOLDER).expect("write placeholder page");
    }
    tauri_build::build()
}

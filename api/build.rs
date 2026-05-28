use std::{env, fs, path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=SKIP_UI");
    println!("cargo:rerun-if-changed=../coolify-ui/package.json");
    println!("cargo:rerun-if-changed=../coolify-ui/bun.lock");
    println!("cargo:rerun-if-changed=../coolify-ui/src");
    let dist = Path::new("../coolify-ui/dist");
    if env::var("SKIP_UI").ok().as_deref() == Some("1") || dist.join("index.html").exists() {
        ensure_placeholder(dist);
        return;
    }
    if Command::new("bun").arg("--version").output().is_ok() {
        let status = Command::new("bun")
            .arg("install")
            .current_dir("../coolify-ui")
            .status()
            .expect("run bun install");
        if !status.success() {
            panic!("bun install failed");
        }
        let status = Command::new("bun")
            .arg("run")
            .arg("build")
            .current_dir("../coolify-ui")
            .status()
            .expect("run bun build");
        if !status.success() {
            panic!("bun run build failed");
        }
    } else {
        ensure_placeholder(dist);
    }
}

fn ensure_placeholder(dist: &Path) {
    fs::create_dir_all(dist).expect("create Coolify UI dist");
    let index = dist.join("index.html");
    if !index.exists() {
        fs::write(index, "<!doctype html><html><head><meta charset=\"UTF-8\"><title>Coolify v5</title></head><body><div id=\"root\">Coolify UI not built. Run bun run build in coolify-ui/.</div></body></html>").expect("write placeholder index");
    }
}

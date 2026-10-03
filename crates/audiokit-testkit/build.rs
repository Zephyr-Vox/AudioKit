//! Embeds revision and a content digest, including uncommitted workspace code.
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

fn collect(path: &Path, paths: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(path)
        .expect("source directory")
        .map(|e| e.expect("source entry"))
    {
        let path = entry.path();
        if matches!(
            path.file_name().and_then(|s| s.to_str()),
            Some("target" | ".git")
        ) {
            continue;
        }
        if path.is_dir() {
            collect(&path, paths);
        } else if matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("rs" | "toml")
        ) {
            paths.push(path);
        }
    }
}
fn main() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidate = package.join("../..");
    let workspace = candidate.join("Cargo.toml").is_file()
        && candidate.join("crates/audiokit/src/lib.rs").is_file();
    let root = if workspace {
        candidate
    } else {
        package.to_path_buf()
    }
    .canonicalize()
    .expect("source root");
    let source_dir = if workspace {
        root.join("crates")
    } else {
        root.clone()
    };
    println!("cargo:rerun-if-changed={}", source_dir.display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("Cargo.toml").display()
    );
    if root.join("Cargo.lock").exists() {
        println!(
            "cargo:rerun-if-changed={}",
            root.join("Cargo.lock").display()
        );
    }
    // A vendored package must not claim the consuming application's Git revision.
    let own_checkout = git(&root, &["rev-parse", "--show-toplevel"])
        .and_then(|p| Path::new(&p).canonicalize().ok())
        .is_some_and(|p| p == root);
    let revision = if own_checkout {
        if let Some(path) = git(&root, &["rev-parse", "--absolute-git-dir"]) {
            println!(
                "cargo:rerun-if-changed={}",
                Path::new(&path).join("HEAD").display()
            );
            println!(
                "cargo:rerun-if-changed={}",
                Path::new(&path).join("refs").display()
            );
        }
        git(&root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unavailable".into())
    } else {
        "unavailable".into()
    };
    let mut files = vec![root.join("Cargo.toml")];
    if root.join("Cargo.lock").exists() {
        files.push(root.join("Cargo.lock"));
    }
    collect(&source_dir, &mut files);
    files.sort();
    files.dedup();
    let mut digest = Sha256::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .expect("workspace source")
            .to_string_lossy()
            .replace('\\', "/");
        let data = fs::read(&path).expect("read build input");
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        digest.update((data.len() as u64).to_le_bytes());
        digest.update(data);
    }
    println!("cargo:rustc-env=AUDIOKIT_REVISION={revision}");
    println!(
        "cargo:rustc-env=AUDIOKIT_SOURCE_DIGEST={:x}",
        digest.finalize()
    );
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

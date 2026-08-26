//! Compiles the Blueprint UI files, bundles them with the stylesheet and
//! icons into a GResource, and compiles the GSettings schema -- all into
//! OUT_DIR, so `cargo run` works from a checkout with nothing installed.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_data = manifest_dir.join("../../data");

    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=resources");
    println!(
        "cargo:rerun-if-changed={}",
        repo_data
            .join("io.github.turbinebmw.Rustle.gschema.xml")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_data
            .join("io.github.turbinebmw.Rustle.metainfo.xml.in")
            .display()
    );

    compile_blueprints(&manifest_dir.join("ui"), &out_dir.join("ui"));
    stage_resources(&manifest_dir.join("resources"), &repo_data, &out_dir);
    glib_build_tools::compile_resources(
        &[out_dir.to_str().unwrap()],
        out_dir.join("rustle.gresource.xml").to_str().unwrap(),
        "rustle.gresource",
    );
    compile_schemas(&repo_data, &out_dir.join("schemas"));
}

fn compile_blueprints(source_dir: &Path, target_dir: &Path) {
    fs::create_dir_all(target_dir).unwrap();
    let mut inputs: Vec<PathBuf> = fs::read_dir(source_dir)
        .expect("ui/ directory exists")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "blp"))
        .collect();
    inputs.sort();
    let status = Command::new("blueprint-compiler")
        .arg("batch-compile")
        .arg(target_dir)
        .arg(source_dir)
        .args(&inputs)
        .status()
        .expect("blueprint-compiler must be installed (package: blueprint-compiler)");
    assert!(status.success(), "blueprint-compiler failed");
}

/// Copy the static assets next to the compiled UI and write the manifest
/// listing everything, so the gresource compiler sees one flat tree.
fn stage_resources(resource_dir: &Path, repo_data: &Path, out_dir: &Path) {
    let icons_dir = out_dir.join("icons/scalable/actions");
    fs::create_dir_all(&icons_dir).unwrap();
    fs::copy(resource_dir.join("style.css"), out_dir.join("style.css")).unwrap();
    let mut icon_entries = Vec::new();
    for entry in fs::read_dir(resource_dir.join("icons/scalable/actions")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        fs::copy(&path, icons_dir.join(&name)).unwrap();
        icon_entries.push(format!(
            "    <file preprocess=\"xml-stripblanks\">icons/scalable/actions/{name}</file>"
        ));
    }
    // The metainfo has no meson placeholders; it ships as written.
    fs::copy(
        repo_data.join("io.github.turbinebmw.Rustle.metainfo.xml.in"),
        out_dir.join("metainfo.xml"),
    )
    .unwrap();

    let mut ui_entries: Vec<String> = fs::read_dir(out_dir.join("ui"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_str().unwrap().to_string())
        .filter(|name| name.ends_with(".ui"))
        .map(|name| format!("    <file preprocess=\"xml-stripblanks\">ui/{name}</file>"))
        .collect();
    ui_entries.sort();

    let manifest = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gresources>\n  <gresource prefix=\"/io/github/turbinebmw/Rustle\">\n{}\n    <file>style.css</file>\n    <file>metainfo.xml</file>\n{}\n  </gresource>\n</gresources>\n",
        ui_entries.join("\n"),
        icon_entries.join("\n")
    );
    fs::write(out_dir.join("rustle.gresource.xml"), manifest).unwrap();
}

fn compile_schemas(repo_data: &Path, target_dir: &Path) {
    fs::create_dir_all(target_dir).unwrap();
    let status = Command::new("glib-compile-schemas")
        .arg("--strict")
        .arg("--targetdir")
        .arg(target_dir)
        .arg(repo_data)
        .status()
        .expect("glib-compile-schemas must be installed (package: glib2)");
    assert!(status.success(), "glib-compile-schemas failed");
}

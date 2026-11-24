// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Bundles application resources and records build facts for Settings > About.

fn main() {
    bundle_agent_resources();
    // The text presets that ship: a folder each under text-presets/, the
    // same preset.toml a user or a community preset is. Listed here into a
    // table of include_str!s, so adding a built-in preset is adding a
    // folder and nothing in Rust names it.
    {
        let root =
            std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
                .join("text-presets");
        println!("cargo:rerun-if-changed={}", root.display());
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
            .expect("text-presets/ exists")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join("preset.toml"))
            .filter(|file| file.is_file())
            .collect();
        files.sort();
        let mut table = String::from("pub static BUILTIN: &[&str] = &[\n");
        for file in &files {
            println!("cargo:rerun-if-changed={}", file.display());
            table.push_str(&format!(
                "    include_str!({:?}),\n",
                file.display().to_string()
            ));
        }
        table.push_str("];\n");
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("out dir"))
            .join("text_presets.rs");
        std::fs::write(out, table).expect("write text_presets.rs");
    }

    // The effect cards that ship: cards/<package id>.jpg, drawn on a real
    // GPU by scripts/bundle-cards.sh. A table of include_bytes!s, which a
    // shelf shows until the machine has drawn its own (#416).
    {
        let root =
            std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
                .join("cards");
        println!("cargo:rerun-if-changed={}", root.display());
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "jpg"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        let mut table = String::from("pub static CARDS: &[(&str, &[u8])] = &[\n");
        for file in &files {
            println!("cargo:rerun-if-changed={}", file.display());
            let id = file
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("a card's name");
            table.push_str(&format!(
                "    ({id:?}, include_bytes!({:?})),\n",
                file.display().to_string()
            ));
        }
        table.push_str("];\n");
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("out dir"))
            .join("bundled_cards.rs");
        std::fs::write(out, table).expect("write bundled_cards.rs");
    }

    // The .exe's icon. The window's own is set in app.slint, but Explorer,
    // the Start menu and a pinned taskbar button read the file's resources,
    // and without one they show Windows' generic program icon. Needs the
    // Windows SDK's rc.exe, which an MSVC toolchain has.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let icon = "../../../assets/icons/bk-cut-app.ico";
        println!("cargo:rerun-if-changed={icon}");
        // The file's name and description as Windows shows them in its
        // properties, and as code signing checks them: "BK CUT" in every
        // build, where the default would be the crate's lower-case name.
        winresource::WindowsResource::new()
            .set_icon(icon)
            .set("ProductName", "BK CUT")
            .set("FileDescription", "BK CUT")
            .set(
                "LegalCopyright",
                "Copyright 2026 Jareer Abdullah, AGPL-3.0-or-later",
            )
            .compile()
            .expect("could not embed the Windows icon");
    }

    // Three facts about the build that the built thing cannot ask for at run
    // time. Settings > About shows them in the block a bug report is copied
    // out of: which triple this binary is for, which profile it came out of,
    // and which compiler made it — the three questions every "cannot
    // reproduce" ends up asking.
    //
    // Change rustc and this file has to run again or `Toolchain` would name
    // the old one.
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!(
        "cargo:rustc-env=BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
    );
    println!(
        "cargo:rustc-env=BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into())
    );
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = std::process::Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=BUILD_RUSTC={version}");
}

// Original editor builds can omit the agent bundle and remain usable. Product
// packaging sets BK_CAT_REQUIRE_AGENT_ASSETS=1 and therefore cannot accidentally
// ship an installer with a missing runtime. Present bundles are validated again
// by RuntimeResources before a platform web view is created.
fn bundle_agent_resources() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .join("../../agent-runtime/dist");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-env-changed=BK_CAT_REQUIRE_AGENT_ASSETS");
    let names = [
        "manifest.json",
        "index.html",
        "runtime.js",
        "bootstrap.js",
        "PI-LICENSE.txt",
    ];
    let complete = names.iter().all(|name| root.join(name).is_file());
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let product_release = matches!(target.as_str(), "windows" | "android")
        && matches!(profile.as_str(), "release" | "app");
    let required =
        product_release || std::env::var("BK_CAT_REQUIRE_AGENT_ASSETS").as_deref() == Ok("1");
    assert!(
        complete || !required,
        "build src/agent-runtime with the pinned Pi reference before packaging BK CUT"
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("out dir"));
    let mut table = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    if complete {
        for name in names {
            let source = root.join(name);
            println!("cargo:rerun-if-changed={}", source.display());
            let target = out.join(format!("bk-cat-{name}"));
            std::fs::copy(&source, &target).expect("copy reviewed runtime resource");
            table.push_str(&format!(
                "    ({name:?}, include_bytes!({:?})),\n",
                target.to_string_lossy()
            ));
        }
    }
    table.push_str("];\n");
    std::fs::write(out.join("agent_assets.rs"), table).expect("write runtime resource table");
}

fn main() {
    println!("cargo:rerun-if-changed=java/ProbeFixture.java");
    for key in ["ANDROID_HOME", "ANDROID_SDK_ROOT", "ANDROID_JAR", "JAVA_HOME"] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    println!("cargo:rustc-link-arg=-Wl,--no-undefined");
    println!("cargo:rustc-link-arg=-Wl,-z,max-page-size=16384");

    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let classes = out.join("probe-java");
    if classes.exists() {
        std::fs::remove_dir_all(&classes).expect("remove old probe classes");
    }
    std::fs::create_dir_all(&classes).expect("create probe classes directory");
    let android_jar = android_build::android_jar(None).expect("Android platform is required");
    let compiled = android_build::JavaBuild::new()
        .file("java/ProbeFixture.java")
        .class_path(&android_jar)
        .classes_out_dir(&classes)
        .java_source_version(8)
        .java_target_version(8)
        .debug_info(android_build::DebugInfo {
            line_numbers: true,
            variables: false,
            source_files: true,
        })
        .command()
        .expect("construct probe javac command")
        .args(["-encoding", "UTF-8"])
        .output()
        .expect("run probe javac");
    assert!(
        compiled.status.success(),
        "probe javac failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let dexed = android_build::Dexer::new()
        .android_jar(&android_jar)
        .class_path(&classes)
        .collect_classes(&classes)
        .expect("collect probe classes")
        .release(false)
        .android_min_api(26)
        .out_dir(&out)
        .command()
        .expect("construct probe d8 command")
        .output()
        .expect("run probe d8");
    assert!(
        dexed.status.success(),
        "probe d8 failed: {}",
        String::from_utf8_lossy(&dexed.stderr)
    );
}


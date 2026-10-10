use std::path::PathBuf;

fn main() {
    // The app is light-only: pin the std-widgets to the light Fluent variant so
    // LineEdit/ComboBox/CheckBox don't follow a dark system theme.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-light".into());
    slint_build::compile_with_config("src/ui/app-window.slint", config)
        .expect("failed to compile Slint UI");

    if std::env::var("TARGET")
        .unwrap_or_default()
        .contains("android")
    {
        build_java_helpers();
    }
}

/// Compile `src/android/java` into `$OUT_DIR/classes.dex`, which src/android.rs
/// embeds and loads at runtime (cargo-apk packages no Java code itself).
/// Same toolchain as Slint's own Android helper: javac + d8 from the SDK.
fn build_java_helpers() {
    use android_build::{Dexer, JavaBuild};

    let sources = [
        "src/android/java/com/flipflop/app/FilePicker.java",
        "src/android/java/com/flipflop/app/BluetoothLink.java",
        "src/android/java/com/flipflop/app/NetworkWatch.java",
        "src/android/java/com/flipflop/app/Storage.java",
        "src/android/java/com/flipflop/app/QrCamera.java",
    ];
    for src in sources {
        println!("cargo:rerun-if-changed={src}");
    }
    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    let out_dir: PathBuf = std::env::var_os("OUT_DIR").unwrap().into();
    let classes = out_dir.join("java");
    let _ = std::fs::remove_dir_all(&classes);
    std::fs::create_dir_all(&classes).expect("create java output dir");
    let android_jar = android_build::android_jar(None).expect("no Android platform found");

    let javac = JavaBuild::new()
        .files(sources)
        .class_path(&android_jar)
        .classes_out_dir(&classes)
        .java_source_version(8)
        .java_target_version(8)
        .command()
        .expect("javac command")
        .args(["-encoding", "UTF-8", "-Xlint:-options"])
        .output()
        .expect("run javac");
    assert!(
        javac.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&javac.stderr)
    );

    let d8 = Dexer::new()
        .android_jar(&android_jar)
        .class_path(&classes)
        .collect_classes(&classes)
        .expect("collect classes")
        .release(release)
        .android_min_api(26)
        .out_dir(out_dir)
        .command()
        .expect("d8 command")
        .output()
        .expect("run d8");
    assert!(
        d8.status.success(),
        "d8 failed: {}",
        String::from_utf8_lossy(&d8.stderr)
    );
}

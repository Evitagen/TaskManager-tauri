fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "perf_sample",
                "proc_sample",
                "proc_kill",
                "meta_get",
                "debug_log",
            ]),
        ),
    )
    .expect("failed to run tauri-build");
}

fn main() {
    tauri_build::build();

    // Test binaries for this crate transitively link `rfd`, whose Windows
    // backend calls `comctl32!TaskDialogIndirect`. That entry point only
    // resolves when the binary carries a Common-Controls v6 activation
    // context, which the real app (and `cargo build`) get from the bundled
    // manifest but bare `cargo test` binaries do not — so without this they
    // fail to launch with STATUS_ENTRYPOINT_NOT_FOUND (0xC0000139).
    //
    // The crate has no `[[test]]` integration target (tests are lib unit
    // tests), so `rustc-link-arg-tests` is rejected; the general
    // `rustc-link-arg` covers the lib test harness. The app already carries a
    // Common-Controls v6 manifest from Tauri, so the duplicate dependency
    // directive is a no-op there. Gate on the *target* OS (not the host).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
}

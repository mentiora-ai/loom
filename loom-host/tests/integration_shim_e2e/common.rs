//! Locating the shim and fake-chromium binaries next to the test binary.

/// Locate the loom-shims target binaries. They live in another crate's
/// bin slot so CARGO_BIN_EXE_* isn't available — use the test binary's
/// own location to find the cargo target dir.
pub(crate) fn target_bin_dir() -> std::path::PathBuf {
    // The test binary itself lives at `<target>/debug/deps/<test>-<hash>`.
    let test_exe = std::env::current_exe().expect("current_exe");
    let deps = test_exe.parent().expect("deps dir");
    deps.parent().expect("debug dir").to_path_buf()
}

pub(crate) fn shim_bin() -> String {
    target_bin_dir()
        .join("loom-shim-chromium")
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn fake_chromium_bin() -> String {
    target_bin_dir()
        .join("fake-chromium")
        .to_string_lossy()
        .into_owned()
}

// Embeds the app icon into the .exe on Windows targets (shown in Explorer).
// Works for both the MSVC toolchain and the mingw/GNU cross-build from WSL;
// embed-resource auto-detects the right resource compiler for the target.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile("app.rc", embed_resource::NONE);
    }
}

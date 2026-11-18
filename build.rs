//! Embeds the application icon into the executable's resource section.
//!
//! Without this the binary carries no icon at all, and Explorer, the taskbar
//! and Alt-Tab all fall back to the generic Windows application glyph. The
//! resource compiler is discovered by `winresource` (windres on the GNU
//! toolchain, rc.exe on MSVC), so the same build works in CI.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    println!("cargo:rerun-if-changed=assets/app.ico");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/app.ico");
    resource.set("FileDescription", "BatteryTray");
    resource.set("ProductName", "BatteryTray");
    resource.set("OriginalFilename", "battery-tray.exe");

    if let Err(error) = resource.compile() {
        // Keep the build usable on hosts without a resource compiler: the
        // application still runs, it only loses the embedded icon.
        println!("cargo:warning=无法嵌入应用图标：{error}");
    }
}

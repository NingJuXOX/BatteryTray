#![windows_subsystem = "windows"]

fn main() {
    if let Err(error) = battery_tray::platform::gui::run() {
        // Keep the reason in stderr as well; the dialog is easy to miss when
        // the application is started from a script or a scheduled task.
        eprintln!("电量托盘启动失败：{error}");
        battery_tray::platform::gui::show_error("电量托盘启动失败", &error.to_string());
    }
}

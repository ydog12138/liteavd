//! liteavd 入口：libadwaita 应用启动。

use libadwaita as adw;
use libadwaita::gio::prelude::*;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(liteavd::core::paths::APPLICATION_ID)
        .build();
    app.connect_activate(liteavd::ui::main_window::build);
    app.run()
}

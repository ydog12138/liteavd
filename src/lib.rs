//! liteavd 核心库。
//!
//! - `core`：纯逻辑层，零 GTK 依赖
//! - `ui`：默认 `gui` feature 下的 GTK4/libadwaita 界面层。

pub mod core;
#[cfg(feature = "gui")]
pub mod ui;

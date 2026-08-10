//! GUI 冒烟集成测试：镜像管理页在线仓库渲染（post_ui 线程模型回归测试）。
//!
//! 需要图形环境：`Xvfb :97 & DISPLAY=:97 cargo test --test gui_images_smoke -- --ignored`
//! 5.6.1 修复 #3 前：在线列表为空（post_ui thread_local 跨线程失效）。
//!
//! 注意：不能用 gtk::Application（会解析 argv 并拒绝 `--ignored`），
//! 用 gtk4::init() + glib::MainLoop + 普通 Window。

use gtk4::prelude::*;

fn count_listbox_rows(root: &gtk4::Widget) -> (usize, usize) {
    let mut online = 0usize;
    let mut installed = 0usize;
    let mut seen_first = false;
    let mut stack: Vec<gtk4::Widget> = vec![root.clone()];
    while let Some(w) = stack.pop() {
        let mut child = w.first_child();
        while let Some(c) = child {
            if let Some(lb) = c.downcast_ref::<gtk4::ListBox>() {
                let mut count = 0;
                let mut idx = 0;
                while lb.row_at_index(idx).is_some() {
                    count += 1;
                    idx += 1;
                }
                if !seen_first {
                    installed = count;
                    seen_first = true;
                } else {
                    online = count;
                }
            }
            stack.push(c.clone());
            child = c.next_sibling();
        }
    }
    (installed, online)
}

#[test]
#[ignore = "需要图形环境（Xvfb/Wayland）"]
fn images_page_renders_online_repository() {
    if gtk4::init().is_err() {
        panic!("gtk4 初始化失败（无图形环境？）");
    }
    let main_loop = glib::MainLoop::new(None, false);
    let result = std::sync::Arc::new(std::sync::Mutex::new(None::<(usize, usize)>));
    let result2 = result.clone();
    let win = gtk4::Window::builder()
        .title("smoke")
        .default_width(800)
        .default_height(600)
        .build();
    liteavd::ui::images_page::open(&win, std::sync::Arc::new(|| {}));
    let loop2 = main_loop.clone();
    glib::timeout_add_seconds_local(25, move || {
        let mut online = 0usize;
        let mut installed = 0usize;
        let mut windows: Vec<gtk4::Window> = Vec::new();
        for w in gtk4::Window::list_toplevels() {
            if let Some(win) = w.downcast_ref::<gtk4::Window>() {
                windows.push(win.clone());
            }
        }
        for w in &windows {
            let (i, o) = count_listbox_rows(&w.clone().upcast());
            if i > installed {
                installed = i;
            }
            if o > online {
                online = o;
            }
        }
        *result2.lock().unwrap() = Some((installed, online));
        loop2.quit();
        glib::ControlFlow::Break
    });
    win.present();
    main_loop.run();
    let (installed, online) = result.lock().unwrap().expect("超时未完成");
    assert!(
        online >= 2,
        "在线仓库列表应 ≥2 行（post_ui 修复 #3 前为 0），实际 online={online}"
    );
    assert!(
        installed == 0 || installed >= 2,
        "本机组件列表异常 installed={installed}"
    );
}

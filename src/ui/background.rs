//! UI 共用的长存 Tokio executor；GTK callback 不再为每个操作创建 runtime。

use std::future::Future;
use std::sync::OnceLock;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("liteavd-ui-worker")
            .enable_all()
            .build()
            .expect("创建 UI 后台 Tokio runtime 失败")
    })
}

pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    std::mem::drop(runtime().spawn(future));
}

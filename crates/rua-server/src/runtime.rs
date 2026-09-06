//! 全局共享 sidecar runtime：所有 turn 线程与 script 绑定统一 `block_on`
//! 它。engine 的 turn future 持有 `&dyn Fn` 跨 await，是 `!Send` 的，不能
//! `tokio::spawn` 到多线程 runtime——所以 turn 在专用线程上驱动；但每个
//! turn 各建 current_thread runtime 是错的：reqwest 连接池全引擎共享，
//! HTTP/2 连接的帧由创建时那条 runtime 上的连接任务驱动，子轮从池里
//! 复用父轮 runtime 池化的连接时，父轮正同步阻塞在 script 工具里
//! （reactor 停摆），子轮一个字节都收不到，只能等各自的读空闲超时
//! （spawn 子轮曾因此全体 120s 挂死，而父轮与 DeepSeek 直连的调用始终
//! 正常）。共享之后，连接驱动任务常驻这里的 worker，任何 turn 线程的
//! 同步阻塞都不影响别的连接收帧。

/// 单 worker 足够：worker 只负责驱动 reactor/timer 与常驻的转发任务，
/// turn 的 future 都在自己线程的 `block_on` 上，不占 worker。
pub(crate) fn shared_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("shared sidecar runtime")
    })
}

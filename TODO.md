# TODO

## 未解决

- [ ] provider LLM 请求无超时：经本机代理抖动时（2026-09-06 benchmark，clash 7890 隧道 CLOSE_WAIT），子 turn 永久 park 在 0% CPU、无任何错误。需要连接/读超时（reqwest Client 层或 per-call 包装），挂死应以 `Outcome::Failed` 闭账而不是无限等待。
- [ ] spawn 子任务失败/取消后的重试策略：父代 script 里 `graph.wait` 收到 `cancelled`/`running` 空文本结果时，目前只会拿到空文本继续。应支持（prompt 引导或绑定面增强）对失败子任务自动重新 spawn（携带上一次失败上下文）——goal 模式闭环的最后一环。

## 已解决

（空）

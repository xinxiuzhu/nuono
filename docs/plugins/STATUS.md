# 插件接入 `nuono状态`

核心指令 `nuono状态` 汇总框架运行状态，并将已启用插件提供的公开聚合指标绘制在状态卡底部。插件通过 `nuono-plugin` 的 `Plugin::status_metrics` 接入；框架不直接读取各业务插件的数据表或缓存结构。已有插件无需修改，默认返回空列表，不占用卡片行。

## 公开信息边界

状态指令面向普通用户开放。该回调只接收统一的快照生成时间，不包含用户、群、权限或会话身份；全局启用插件返回的同一组指标会展示给所有调用者，因此不得包含个人绑定账号、群内记录、账号 ID、凭据、私有内容或受权限保护的管理数据。

适合展示的指标包括公开账号总量、签到总次数、索引条目数或缓存完成率。插件作者需自行确认聚合指标适合公开；SDK 字段校验只限制格式和长度，无法识别业务敏感信息。涉及个人数据或管理详情时，应使用带权限校验的独立业务指令。

## 回调接口

```rust
use nuono_plugin::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
struct MyPlugin {
    // 由业务流程维护的公开聚合快照；不在状态回调里遍历用户记录。
    public_accounts: AtomicU64,
    public_sign_ins: AtomicU64,
}

#[async_trait]
impl Plugin for MyPlugin {
    fn name(&self) -> &str { "MyPlugin" }
    fn display_name(&self) -> &str { "我的插件" }
    fn version(&self) -> &str { "1.0.0" }
    fn description(&self) -> &str { "示例插件" }

    async fn status_metrics(
        &self,
        request: &PluginStatusRequest,
    ) -> Result<Vec<PluginStatusMetric>> {
        // request.generated_at 是 chrono::DateTime<Utc>，与框架卡片快照一致。
        // 统计今日数据时显式使用业务时区，避免把 UTC 日期当成本地日期。
        let _snapshot_at = request.generated_at;
        Ok(vec![
            PluginStatusMetric::new(
                "账号总量",
                self.public_accounts.load(Ordering::Relaxed).to_string(),
            ),
            PluginStatusMetric::new(
                "签到总次数",
                self.public_sign_ins.load(Ordering::Relaxed).to_string(),
            ),
        ])
    }
}
```

使用 `#[nuono_plugin]` 宏时，采用 `impl Plugin` 形态并将 `status_metrics` 写入同一实现块，不要在保留 struct 形态宏的同时重复添加第二个 `Plugin` 实现。

`PluginStatusMetric::new(label, value)` 中的 `value` 由插件预先格式化并附带单位（如 `2.9万`、`1,234`、`98%`）。插件无需启用 `image` feature 或自行绘图，卡片排版与发送由框架统一完成。

| 限制 | 约定 |
| --- | --- |
| 每插件指标数 | 最多 4 项；空列表隐藏整行 |
| label | 最多 16 个 Unicode 字符，去掉首尾空白后不得为空或重复 |
| value | 最多 24 个 Unicode 字符，去掉首尾空白后不得为空 |
| 文本内容 | 禁止换行、制表符、NUL 等控制字符；仅展示纯文本 |
| 图片、图标、链接 | 此协议不提供 URL、文件路径或自定义绘图字段 |

插件测试中可调用 `nuono_plugin::status::validate_status_metrics(&metrics)` 提前校验。宿主也会校验整组返回值：任一指标不合法时整组不予展示，卡片仅显示不可用状态，避免截断部分指标造成误导。

## 执行预算与生命周期

宿主只收集全局启用插件，按稳定插件名排序，最多检查前 256 个插件；返回空列表的插件不占行，过滤后最多展示 24 行。单次状态生成最多并发执行 4 个回调，单项预算 400 ms，含锁等待在内的总预算为 1.5 秒；插件集合锁与启用注册表锁各自最多等待 100 ms。超时、panic、返回错误或格式不合法均不影响其他插件和整张状态卡，错误详情也不会写入公开图片。

回调应直接读取内存快照或执行轻量聚合，不得发起网络请求、全表扫描、同步阻塞、同步重入插件启停流程，或通过裸 `tokio::spawn` 留下取消后仍继续运行的任务。超时取消仅丢弃异步 future；源码插件与宿主同权限、同地址空间，并非可强制终止阻塞代码的沙箱。当前插件实例以 `Box<dyn Plugin>` 持有，宿主在预算窗口内保留集合读锁，插件需配合异步取消及时让出执行权。

锁繁忙或超出总预算时，对应插件行显示“忙碌”；已启动回调超出单项预算时显示“超时”；默认空实现或返回空列表时不显示占位行。插件禁用后，后续状态快照不再调用该插件；新增或修改回调源码后需重新构建并启动应用，Dashboard 实例重建不会更新已编译代码。

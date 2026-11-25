# 原生目标主动发送

插件持久化通知目的地时保存完整 `RuntimeBotRoute + domain::DeliveryTarget`。原生身份包含平台、kind、scheme、value 和 namespace；单独的用户字符串不能证明同一平台身份。机器人所有者与通知会话分别保存，例如飞书私聊会话可以携带 chat_id。

SDK 提供：

```rust
bot.send_native_routed(&route, &target, "通知").await?; // RuntimeActionReceipt
bot.send_native_routed_notify(&route, &target, "通知").await?; // ()，send_action 语义
bot.call_native_routed(&route, action).await?; // domain::OutboundAction，含回复等操作
bot.notify_native_routed(&route, action).await?;
bot.send_native_file_routed(
    &route, &target, document_bytes, "export.json", Some("application/json"),
).await?; // FileDelivery::Sent { receipt } 或明确的 Unsupported
```

消息参数 `NativeMessage` 支持字符串、`domain::MessageSegment` 或其数组；图片通过 `ImageSource::into_native_segment().await?` 生成，与现有图片入口共享格式、大小、文件读取和 URL 校验。插件不需要拼平台 JSON 或编码 base64。

`MessageSegment::At` 目前只携带原始用户字符串，不能表达或验证 scheme 与 namespace。插件应从已验证、属于当前应用和会话的原生所有者生成提及，不把任意裸 ID 当作身份凭据。飞书适配器发送富文本 `post.at.user_id`，该字段接受 OpenId、UserId、UnionId，原值直接传递，不按前缀推断 ID 类型。参见[官方消息 FAQ](https://open.larksuite.com/document/server-docs/im-v1/faq)。

目标使用既有 `DeliveryTarget::actor(identity)`、`DeliveryTarget::conversation(ConversationRef::direct(participant, conversation))`、group 或 channel 构造。需要回复原消息时使用 `OutboundAction::SendMessage { target, segments, reply_to: Some(native_message_id) }`，保留原生会话和精确消息 ID。

路由匹配调用者给出的三字段原值和当前注册 generation。QQ 的 `qq` 与 `qq_official` 平台别名不会被平台显示名重建覆盖。未指定 route 的宿主 typed 回复仅在注册表中只有一个匹配 BotIdentity 时解析；存在多个别名路由时要求显式 route。发送沿用既有结构校验、authority、审计、宿主 runtime 与取消，不隐式重试。取消可以停止尚未完成的调用，不能撤回平台已经接受的消息。

在目标转成兼容参数之前，transport 校验每一个原生身份，适配器再通过 `RuntimeActionSink::validate_native_target()` 校验实际客户端拥有的应用 scope：

| 平台 | 支持的原生目标 |
| --- | --- |
| QQ 官方 | app_id namespace 内的 OpenId 用户（C2C）或群；群成员 namespace `app_id/groups/group_openid` 不能作为 C2C 目的地 |
| 飞书 | 当前 app_id 内 OpenId / UserId / UnionId 用户，或 ChatId 会话、群、频道；直接会话带 chat_id 时向该会话发送；收件类型由 scheme 显式选择 |
| Telegram | 无 namespace、规范 i64 字符串的 Numeric 身份 |
| Discord | 无 namespace、规范非零 u64 字符串的 Snowflake 身份 |
| OneBot | 无 namespace 的 Native 身份，原始字符串保持精确 |

不匹配的 scope 在 HTTP 请求之前拒绝；不支持的原生类型不会隐式推断为另一类型。原生 API 保留 `Validation`、`NotImplemented` 与 `PermissionDenied` 错误类别。成功回执保留精确原生 message IDs 和原始响应，不以兼容数字 API 重建 ID。

旧 `RuntimeMessageTarget`、`send_routed` 和 raw routed API 继续支持已有扁平协议入口。它们不能表达完整原生身份，新的有 namespace 的业务数据应使用本页的 typed 入口。插件与宿主同权限，typed 校验用于避免身份误用，不提供安全沙箱。

文件使用同一完整目标校验和精确路由，领域操作为 `OutboundAction::SendFile`，由 `RuntimeActionSink::call_native_file(NativeFileSendRequest)` 执行；插件直接传入字节，无需拼接平台 JSON 或编码 base64。飞书按身份 scheme 选择 `open_id`、`user_id`、`union_id`、`chat_id`，已有私聊会话使用其 ChatId；Discord 已有私聊频道时直接发送，无频道时才解析用户的 DM。Telegram 的 User 与直接会话必须为正数 ID，群和频道保留规范的有符号 chat ID。QQ 群成员身份不能转换为 C2C 文件收件人。

仅当适配器明确返回无上传、无发送副作用的 `FileSendResponse::Unsupported` 时，才允许 `FileDelivery::Unsupported` 回退。目标或应用不匹配、权限、网络、大小、无效回执与超时均返回错误；上传成功但发送失败也不会重试或回退。文件须使用带回执的 `call`，`notify` 不接受 `SendFile`。SDK 上限为 32 MiB、整个调用限时 45 秒；平台可实施更严格的限制。详见[文件投递与官方协议依据](MEDIA.md#发送非图片附件)。

`nuono-plugin-test::TestBot` 捕获 `send_native_file` 与完整 `params.target`，从 `push_reply` 脚本读取严格的 `FileSendResponse`，可测试 `Sent`、`Unsupported` 与无效回执。

自动验证覆盖 SDK 的 native API、runtime router 和 transport 单元测试，以及 `crates/nuono-core/tests/test_native_delivery.rs`。集成测试仅使用合成凭据与本地 HTTP 服务，检查实际 QQ/飞书收件目标、应用范围、回复上下文及 Telegram/Discord 发送路径；这些测试不代表真实平台权限或上游接入验收。文件链路另由 `test_native_file_delivery.rs` 验证 Bot → router → 实际适配器，覆盖身份 scheme、实际应用作用域、已有直接会话、精确回执和失败不重试；各适配器的 `file_delivery` 测试覆盖 multipart 字节及上传前拒绝。

## 入站原生提及与文本范围

`Event::native_user_mentions()` 返回去重的原生 `IdentityRef`，包含平台、身份规则和应用作用域；明确识别为机器人的身份不会作为用户目标。`native_domain_event()` 同时支持借用 typed 领域事件与校验后的 runtime `domain_event` 投影，旧 `domain_event()` 借用接口保持可用。业务仍需按当前机器人路由验证身份，并重新核对自身数据归属。

`text_without_native_mentions()` 清理完整消息中的已确认提及；`text_without_native_mentions_fragment(&data.args)` 清理原文中唯一出现的参数片段，保留其余空格与文本。片段截断提及、范围重叠或不唯一时会报错。OneBot 需要真实 At 段及对应 CQ/完整文本投影，Discord 需要原生提及元数据或领域 At，飞书需要原始 mentions 中明确的 ID 规则与应用证据，Telegram 使用原生 User 或用户链接实体并按 UTF-16 范围定位。裸 `@用户名` 不解析为用户身份，QQ 官方缺少目标身份规则或作用域证据的裸 At 也不做猜测处理。

消息文本限制 256 KiB，用户/提及位置最多 64 处，消息段与非提及实体最多 4096 项。飞书适配器保留 `message.mentions` 原数据供 SDK 校验，空数组不额外写入原消息序列化。该能力仅解析当前入站事件，不查询平台用户资料、不打开目标用户凭据，也不改变框架的命令匹配行为。

## 插件对外 HTTP 提交

平台主动发送 API 与插件直接使用的 HTTP 是两条能力入口。`ctx.http` 默认保持既有 reqwest 重定向与 protocol NACK 重试策略；一次 `.send()` 并不等于只发起一次应用请求。对不能自动重复提交的 POST，在原客户端配置上派生专用客户端：

```rust
let upload_http = ctx.http.without_redirects_and_retries()?;
// 继续放在本插件已有的受管任务、并发许可与数据归属校验内。
let response = upload_http.raw().post(upload_url).json(&payload).send().await?;
```

衍生客户端保留原实例的有效总超时、连接超时和代理设置，使用独立连接池；不改变插件实例、任务 scope 或业务共享并发许可。SDK HTTP 包装器本身没有独立的用户身份或全局限流器，派生方法不增加另一套授权或调度入口。它禁止自动跟随所有重定向，也用 `reqwest::retry::never()` 禁止协议错误重试。3xx 原响应交给业务处理，不能因 `Location` 再次提交；超时、连接中断或响应不完整仍可能表示远端已经接收，不能声称远端没有执行。

需要调整其他选项时，使用 `ctx.http.rebuild_builder().follow_redirects(false).retry_protocol_nacks(false).try_build()?`。`try_build()` 在无效代理或构造失败时返回错误，不回落到默认策略。旧 `builder().build()` 的默认行为保持兼容；使用限定策略的业务应采用 fallible 构造，并在请求开始前处理错误。该 HTTP 策略约束不替代插件的数据证明、令牌校验、队列上限、任务取消和明确的上传命令。

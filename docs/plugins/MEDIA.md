# 图片、绘图任务与素材

插件负责卡片内容和排版，Nuono 负责消息发送、计算配额和素材存取。`image`、`imageproc`、`ab_glyph` 等库照常写在插件自己的 `Cargo.toml` 中，SDK 不绑定某个绘图库，也不要求新增独立 crate。

## 发图片

在指令里直接传生成的图片字节：

```rust
bot.send_image(png_bytes).await?;
bot.send_image(ImageSource::path("data/MyPlugin/assets/card.png")).await?;
bot.send_image(ImageSource::url(image_url)).await?;
```

`png_bytes` 是已编码的 `Vec<u8>`，不是 RGBA 像素数组。SDK 支持 PNG、JPEG、WebP、GIF 来源，默认最多读取 10 MiB；实际平台可能对格式、尺寸、动画或大小另有限制，失败会返回错误。

| 接口 | 返回 | 用途 |
| --- | --- | --- |
| `ImageSource::bytes(bytes: impl Into<Vec<u8>>)` | `ImageSource` | 已编码的图片内容 |
| `ImageSource::path(path: impl Into<PathBuf>)` | `ImageSource` | 本机文件，发送前读取内容 |
| `ImageSource::url(url: impl Into<String>)` | `ImageSource` | HTTP/HTTPS 图片地址 |
| `ImageSource::file_id(id: impl Into<String>)` | `ImageSource` | Telegram file_id 或飞书 image_key 等平台原生标识 |
| `source.into_segment().await` | `Result<MessageSegment>` | 与文字等消息段组合 |
| `source.into_reusable_segment().await` | `Result<MessageSegment>` | 在 QQ 原生 Markdown 组合中复用固定素材 |
| `bot.send_image(source).await` | `NuonoResult<()>` | 回复当前会话 |
| `bot.send_image_routed(&target, source).await` | `NuonoResult<RuntimeActionReceipt>` | 主动发图并取回执 |

`Vec<u8>` 和 `PathBuf` 可直接传给发图方法。文件标识只能在原平台、原可用范围内复用，不能将 Telegram 的 `file_id` 用于 Discord 发送。插件不是沙箱；文件读取采用宿主进程的权限。

文字和图片组合：

```rust
let image = ImageSource::bytes(png_bytes).into_segment().await?;
bot.send(vec![MessageSegment::text("今日便笺"), image]).await?;
```

后台任务用 `ctx.bot` 和保存的 `RuntimeMessageTarget`。OneBot 主动推送如果不需要回执，可用 `send_routed_notify(&target, message)`。

### QQ Markdown 图文

自定义 Markdown、图片和按钮可组合为同一条 QQ 消息，图片保持在声明的位置：

```rust
let logo = ImageSource::path("data/MyPlugin/assets/logo.png")
    .into_reusable_segment().await?;
let card = ImageSource::bytes(png_bytes).into_segment().await?;
bot.send(vec![
    MessageSegment::markdown("# 查询结果\n"),
    logo,
    MessageSegment::markdown("\n今日数据：\n"),
    card,
    MessageSegment::buttons(vec![vec![ButtonOne::command("刷新", "查询")]]),
]).await?;
```

默认图片使用短期上传直链；`into_reusable_segment` 才申请 QQ 官方素材，适合 logo、固定说明图等内容。常变结果图使用默认图片段，避免占用默认 100 张的素材配额。素材目录按内容摘要复用，重建客户端后也会查询已有资源；图片不在 SDK 或适配器中重新编码。[官方素材管理](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/material-management.html)

组合最多 10 张图片，需要读取的图片合计不超过 10 MiB。图片和按钮先完成校验、解析，再执行平台上传；失败不会自动重复发送整条消息。按钮仍受当前消息策略和账号权限约束，未启用原生按钮时使用已有文字选项回退。普通图片发送、其他平台以及 Markdown 模板的处理规则保持各自契约。

### 平台如何处理

| 平台 | 处理方式 |
| --- | --- |
| OneBot / QQ | 使用平台已有的图片消息与上传处理；本机文件先转成内容，远程接入也能收到 |
| Telegram | 字节和本机文件通过 multipart 上传；URL/file_id 可直接发送 |
| Discord | URL 在宿主下载后，与字节、本机文件一样作为附件上传 |
| 飞书 | 新图片先上传取得 image_key；单图片发 image，图文组合发 post；已有 image_key 可复用 |

这些接口统一调用方式，不保证各平台拥有相同的尺寸、格式或动画能力。URL 下载检查 HTTP 状态和读取大小；不会用错误页冒充图片。已有的 `MessageSegment::image_from_path` 只生成文件 URL，涉及远程 OneBot 时优先用 `ImageSource::path`。

## 短时效媒体链接

Base64 内嵌是兼容性最强的音视频传输方式，但每个文件在消息中约占原始体积的 4/3，宿主和 OneBot 实现均需为其分配内存。宿主可为指定的本地文件签发短期 URL，供聊天平台主动拉取：

```rust
let link = ctx.media_links.link_for(event.route(), &media_path).await?;
bot.send_routed(&target, MessageSegment::video(link.url.clone())).await?;
```

| 接口 | 返回 | 说明 |
| --- | --- | --- |
| `ctx.media_links.is_supported()` | `bool` | 当前宿主是否能签发链接 |
| `ctx.media_links.link_for(&route, path)` | `Result<MediaLink>` | 按该路由解析出网关可达的绝对 URL，默认 10 分钟有效 |
| `ctx.media_links.link_for_with_ttl(&route, path, ttl)` | `Result<MediaLink>` | 自定义有效期，最长 1 小时 |
| `ctx.media_links.link(path)` | `Result<MediaLink>` | 无路由上下文，按宿主监听地址解析；仅同机消费者可用 |

`MediaLink` 包含 `url`（可直接放进消息段）、`path`（宿主上的 `/media/...` 路径）、`file`（已规范化并签名的绝对路径）和 `expires_at`。

宿主使用进程内随机密钥签名，重启后所有旧链接立即失效，密钥不落盘。`/media/{token}` 仅服务显式签名的普通文件：路径先执行 `canonicalize`，签名校验采用常数时间比较，过期、篡改或文件被删除统一返回 404。读取时最后一段按 `O_NOFOLLOW` 打开，签发后被替换的软链接不会将请求重定向到其他文件。链接仅对签发时的单个文件有效，不支持遍历目录。

绝对地址按 bot 路由解析：OneBot 网关建立 WebSocket 时使用的 `Host` 与 `X-Forwarded-Proto` 会被记录，网关位于另一台机器或反向代理后方时链接仍然可达。该地址仅在当前连接存活期间有效：连接断开即清除，旧连接也不会覆盖新连接记录的地址。无路由上下文时才回退为宿主监听地址（绑定 `127.0.0.1` 时解析为回环地址，其他机器不可达）。

调用 `link_for(&route, path)` 时，若该路由尚未记录可达地址（例如非 OneBot 平台，或 OneBot 连接尚未完成注册），调用会直接返回错误，不会静默签发 `127.0.0.1` 链接；插件应按业务规则决定回退方式，例如启用 URL 传输模式的插件可在此情况下回退为 Base64。非 OneBot 平台如需 URL 传输，应优先使用平台原生上传接口。

该机制要求网关能够访问宿主 HTTP 端口。签发链接同样适用于 Telegram、Discord、飞书等平台，但这些平台通常更适合使用原生上传接口，插件可按业务需求选择。

## 读取用户头像

`Bot::read_user_avatar(&RuntimeBotRoute, &UserAvatarRequest, &PluginHttpClient)` 从指定的已连接适配器读取用户头像。SDK 公开 `UserAvatarRequest` 和 `UserAvatar`，请求身份使用 `nuono_plugin::domain::IdentityRef`。优先复制 `event.domain_event().actor` 和 `event.route()`；QQ、飞书等平台必须保留原有应用命名空间，不能把 openid 当作全局用户 ID。

```rust
let request = UserAvatarRequest {
    identity: actor.clone(), // 领域事件中的原生用户身份
    size: 256,
    max_bytes: 8 * 1024 * 1024,
    source_hint: None,
};
match bot.read_user_avatar(&route, &request, &ctx.http).await? {
    UserAvatar::Available { image, version } => {
        // image.bytes 是编码图片；解码并限制尺寸后用于插件绘图。
        // version 是可选的原生版本或内容摘要，不能作为用户身份。
    }
    UserAvatar::Absent => { /* 用户未提供头像 */ }
    UserAvatar::Unsupported => { /* 当前适配器不支持该身份 */ }
}
```

`size` 允许 16–1024，适配器选择接近的原生尺寸；`max_bytes` 必须大于零，最多 10 MiB。SDK 检查字节数、base64 与 PNG/JPEG/WebP/GIF 文件头；完整图片解码、解码内存和像素尺寸限制由插件负责。`source_hint` 只能来自接收到的发送者资料，适配器核对其来源和身份；聊天文字中的 URL 不能作为头像依据。

| 平台 | 读取方式 |
| --- | --- |
| Telegram | [getUserProfilePhotos](https://core.telegram.org/bots/api#getuserprofilephotos) 选择头像，再通过原生文件接口下载；token 留在适配器 |
| Discord | [Get User](https://github.com/discord/discord-api-docs/blob/main/developers/resources/user.mdx) 获取 avatar hash，再从 Discord CDN 下载；CDN 请求不携带 Bot token |
| 飞书 | 按实际应用命名空间和用户 ID 类型查询用户资料，从原生 `avatar` 字段选择图片；头像下载不携带应用认证头 |
| OneBot | 兼容旧插件的公开 QQ 头像服务，仅接受无命名空间的原生 QQ 用户编号；OneBot v11 没有统一头像读取 action |
| QQ 官方机器人 | 兼容旧插件的 `qqapp` 头像服务，使用适配器的实际 app_id 与该应用/群作用域 openid；收到的提示地址也须对应同一应用与用户 |

QQ/OneBot 的 CDN 路径属于旧插件兼容行为，不是官方 Bot API 的头像保证。飞书原生字段参见 [官方 SDK 的 AvatarInfo](https://github.com/larksuite/oapi-sdk-python/blob/v2_main/lark_oapi/api/contact/v3/model/avatar_info.py)。这些实现通过离线协议测试验证，真实权限、头像可见性和平台返回值仍需实际接入验收。

路由离线、HTTP 错误、图片过大或响应无效均返回错误，不伪装成“无头像”；缓存可据此保留上次成功图片。读取等待最多 20 秒，取消时终止尚未完成的适配器调用。业务缓存应以完整 route、平台、身份种类、原生值和 namespace 为键；成功图片可按内容摘要保存到素材目录，避免重复保存带认证的 URL。

## 把绘图放在线程里

```rust
let png = ctx.render("daily-card", move || {
    draw_daily_card(&daily)
}).await?;
bot.send_image(png).await?;
```

`draw_daily_card` 是插件自己的同步函数，返回 `anyhow::Result<Vec<u8>>`。闭包和结果都必须是 `Send + 'static`，需将所需数据、字体或素材克隆后 `move` 进闭包，不要借用事件回调里的短期引用。网络请求先在闭包外 `.await`，绘图闭包只执行可自行结束的同步计算。

### 跟随系统图片输出设置

插件自行编码的图片需要使用公共编码入口，才能跟随 Dashboard「系统设置 → 常规 → 图片输出」。在 `nuono-plugin` 依赖上启用 `features = ["image"]`；不启用时 SDK 不引入 `image`，策略读取接口仍可用。

```rust
let output = ctx.image_output_settings().await?;
let bytes = ctx.render("daily-card", move || {
    // 插件自己的绘图函数，返回 image::DynamicImage，尚未编码。
    let pixels = draw_daily_pixels(&daily)?;
    nuono_plugin::image_output::encode(&pixels, &output)
}).await?;
bot.send_image(bytes).await?;
```

`ctx.tasks.image_output_settings()` 和 `PluginRuntime::image_output_settings()` 也读取最新有效策略。一次请求只取一份快照，多页和成品缓存共用它；缓存键应包含 `output.cache_tag()`，避免改质量后复用旧图。默认 PNG 使用 Fast/Paeth 无损编码并保留透明度；选择 JPEG 时透传 1–100 的质量参数，透明区域合成白底，不改尺寸。PNG 的缓存标记忽略待用 JPEG 质量，当前为 `image-output-v2-png`；JPEG 保持 v1。JPEG 直接借用 RGB8 像素，其他类型按需合成白底，不创建整张 RGB 临时图。

只在最终生成卡片的编码处调用，直接传像素，不要先编码 PNG 再解码压成 JPEG。SDK 不拦截任意插件的第三方编码器，也不会在媒体上传层统一重编码。原图文件、固定格式素材、头像缓存、独立登录二维码、OCR 或 AI 视觉预算沿用各自契约；合成进卡片的头像和原链接二维码随整图编码。

默认所有插件共用一个绘图池：

- 同时最多 2 项；只有 1 个可用 CPU 时为 1 项。
- 额外排队最多 16 项，满了立即返回“请稍后重试”，不会一直堆积请求。
- 绘图 panic 转成错误；不会占住配额。
- 调用者取消等待，已经开始的线程计算会继续到结束，期间保留配额。
- 插件停用时取消排队，等待已经开始的绘图结束，再进行清理回调。

`ctx.render<T, F>(name, job) -> Result<T>` 与 `ctx.tasks.render<T, F>(name, job)` 使用同一个任务作用域，约束为 `T: Send + 'static`、`F: FnOnce() -> Result<T> + Send + 'static`。`RenderPool::new(RenderLimits { concurrency, queue_capacity }) -> Result<RenderPool>` 和 `PluginTaskScope::with_render_pool` 供宿主定制及隔离测试使用；普通插件使用宿主的 `ctx.render`，这样多个插件能共同控制占用。

配额控制计算数量，不控制插件自建图像的大小。解析外部图片时使用绘图库的尺寸/解码内存限制，复用已解码的小图标和字体，避免把全部立绘一直留在内存中。原生计算启动后无法强制终止，绘图函数必须能自行结束。

## 管理素材

默认素材目录是 `data/<插件名>/assets/`。素材名称使用相对路径，比如 `ui/bg.png`；绝对路径、`..` 不接受。

| 接口 | 返回 | 用途 |
| --- | --- | --- |
| `ctx.assets.root()` | `&Path` | 素材根目录，同步方法 |
| `ctx.assets.path(name)` | `Result<PathBuf>` | 生成目录内路径，同步方法，不保证文件存在 |
| `ctx.assets.install(name, bytes).await` | `Result<PathBuf>` | 安装随插件附带的默认素材，保留已有用户文件 |
| `ctx.assets.fetch(name, url).await` | `Result<PathBuf>` | 优先用现有文件，不存在时下载；已有文件可离线使用 |
| `ctx.assets.refresh(name, url).await` | `Result<PathBuf>` | 显式更新，失败保留原文件 |
| `ctx.assets.read(name).await` | `Result<Arc<[u8]>>` | 读取并复用素材字节，修改磁盘文件后缓存失效 |

`name` 参数接受 `impl AsRef<Path>`，`url` 为 `&str`，安装内容为 `&[u8]`。并发下载同一名称只执行一次。下载检查 HTTP 状态和大小，完成后才替换目标文件。缓存按字节预算淘汰最久未使用的素材。

默认 `AssetLimits` 为单文件 32 MiB、全进程共用的内存字节缓存 16 MiB、全进程同时下载 2 项。宿主通过 `PluginAssets::new` 创建的管理器共用预算，插件数量不会把缓存和下载并发成倍放大。缓存延迟填充，超预算的文件仍可读取，但不会常驻缓存；调用者持有的 `Arc` 和绘图库解码后的图像另占内存。

需要不同策略时可创建自己的管理器：

```rust
let assets = PluginAssets::with_limits(
    ctx.data_dir.join("assets"),
    ctx.http.clone(),
    AssetLimits {
        max_file_bytes: 8 * 1024 * 1024,
        memory_cache_bytes: 4 * 1024 * 1024,
        download_concurrency: 1,
    },
)?;
```

`PluginAssets::new(root, http)` 使用进程共享的默认策略；`with_limits` 创建独立配额，克隆同一管理器共用下载锁和缓存。素材 URL、更新时机和业务版本号由插件决定，框架不会自动替插件更新整套素材。

## 典型绘图插件用法

需要绘制卡片的插件通常在加载时用 `ctx.assets` 安装和读取背景、图标、字体，再用 `ctx.render` 解码并准备共享渲染资源，让查询、定时通知和订阅推送共用同一个 renderer。

卡片可使用 `image + imageproc + ab_glyph` 等库自行绘制；发送统一使用 `bot.send_image` 或图文消息段，业务数据、评分和卡片布局留在插件自身模块中。

## 读取非图片附件

`Bot::read_inbound_file(&route, source, message_id, &ctx.http, max_bytes)` 读取入站 `File` 段的字节，返回 `media::InboundFile::Available { bytes, file_name, content_type }` 或明确的 `Unsupported`。`route` 使用 `event.route()`，`message_id` 使用原始消息 ID；不要根据数字用户 ID 重建路由。普通文件不做图片头检查，JSON 文档可以直接交给业务解析器。

```rust
use nuono_plugin::media::InboundFile;

match bot.read_inbound_file(
    &route, received_source, original_message_id.as_deref(), &ctx.http,
    16 * 1024 * 1024,
).await? {
    InboundFile::Available { bytes, file_name, content_type } => {
        // 文件名和 MIME 都只是提示。还需校验 JSON 内容、目标用户与业务字段。
        let document: serde_json::Value = serde_json::from_slice(&bytes)?;
    }
    InboundFile::Unsupported => { /* 告知用户此附件协议尚未支持 */ }
}
```

调用者必须明确传入字节上限，范围为 1 byte 至 `media::MAX_FILE_BYTES`（32 MiB）。图片 API 的 10 MiB 上限不变。HTTP(S) 与 `base64://` 来源直接有界读取；原生句柄通过 `read_inbound_file` action 送到同一机器人适配器。读取检查 HTTP 状态、已知长度与每个响应块，最多等待 20 秒；错误文本不包含下载 URL。任意本机文件路径不接受。

Telegram 复用 `getFile` 与受限下载，下载地址中的 bot token 不返回给插件。飞书读取原消息的 `file_key`，必须传入该消息的 `message_id`；不会将 `file_key` 推断为 URL。适配器未提供文件名或 MIME 时，相应字段为 `None`，调用者应保留入站 `File` 段的名称；当前 Telegram/飞书原生读取返回的这两项为空。平台权限或更小的下载限额仍可能令请求失败；32 MiB 是框架上限，不是平台能力保证。

## 发送非图片附件

`Bot::send_native_file_routed(&route, &target, bytes, file_name, content_type)` 上传附件，并等待平台确认的消息回执。`route` 是保存的精确 `RuntimeBotRoute`，`target` 是完整 `domain::DeliveryTarget`，保留平台、身份种类、scheme、namespace 与已有私聊会话；不依赖当前命令上下文。输入为文件字节，不能传本机路径或让适配器读取插件文件。

```rust
use nuono_plugin::media::FileDelivery;

match bot.send_native_file_routed(
    &route, &target, document_bytes, "export.json", Some("application/json"),
).await? {
    FileDelivery::Sent { receipt } => {
        // receipt.message_ids 是已确认的原生消息 ID。
    }
    FileDelivery::Unsupported => {
        // 仅此结果可改用插件自己的已鉴权下载路由。
    }
}
```

SDK 上限为 32 MiB，平台可能有更小的附件限额。文件名必须为不含目录、控制字符或引号的 basename，UTF-8 编码至多 255 bytes；可选 MIME 必须是无参数的 `type/subtype`。SDK 通过 typed `OutboundAction::SendFile` 和 `RuntimeActionSink::call_native_file` 传递完整目标与有界内容，适配器负责凭据与 multipart 上传，不把凭据 URL 交给插件。结构、平台与每个身份的 scheme/namespace 均在上传前校验；飞书和 QQ 同时核对实际客户端的 app_id。

整个调用最多等待 45 秒；调用者取消会停止尚未完成的宿主上传 future，但无法撤回平台已接收的副作用。发送不自动重试，网络、权限、超限、无效回执和超时均返回错误，不能把它们当成 `Unsupported` 或宣布成功。只有 typed 文件能力返回明确的无效果 `Unsupported` 才允许回退，适配器返回的错误即使叫 `NotImplemented` 也不作为回退证明。

旧 `send_file_routed(&RuntimeMessageTarget, ...)` 与 `send_file` raw action 保留兼容，不能表达完整身份作用域。新持久化的原生目标使用上面的 typed 入口；不要把飞书 UserId、UnionId 或已有 Direct ChatId 压成裸用户字符串。

目前原生发送能力：

- Telegram 使用 [`sendDocument`](https://core.telegram.org/bots/api#senddocument) 上传 `document` 的实际字节，并核对返回的原生 chat ID 与 message ID。
- 飞书先[上传 IM 文件](https://open.feishu.cn/document/server-docs/im-v1/file/create)取得 `file_key`，再[发送消息](https://open.feishu.cn/document/server-docs/im-v1/message/create)的 `msg_type=file`。

  官方 SDK 的 [IM 模型](https://github.com/larksuite/oapi-sdk-go/blob/v3_main/service/im/v1/model.go)定义普通文件 `stream`、返回的 `file_key` 与显式收件类型 OpenId/UserId/UnionId/ChatId；typed 入口按 scheme 选择类型，已有 Direct ChatId 时直接向该会话发送，不按 ID 前缀推断类型。新入口在请求前拒绝空文件，并按 30 MB（30,000,000 bytes）保守限制内容；只有发送步骤取得 message ID 才返回 `Sent`。
- Discord 使用官方 [`Create Message`](https://github.com/discord/discord-api-docs/blob/main/developers/resources/message.mdx#create-message) 的 `files[0]` multipart 与 `payload_json`；用户目标先解析 DM 频道，已有直接会话或频道则直接发送，随后核对消息回执的频道 ID。官方限制整个请求为 25 MiB，typed 入口为文件内容预留 4 KiB multipart 开销。
- QQ 官方 SDK 仍将群/C2C `file_type=4` 标为“暂不开放”（[官方源码](https://github.com/tencent-connect/botpy/blob/master/botpy/api.py)）；OneBot 文件上传依赖实现私有协议。这两个适配器明确返回 `Unsupported`，不会转发未约定的 `send_file` API。插件可在此结果下使用随机短期 ticket 等保护自己的 Web 下载路由，不能把原生发送失败伪装成成功。

三个已实现上传的适配器均使用有界、禁止重定向的 HTTP 请求；文件只发到当前路由对应的平台客户端，上传错误不回显凭据或服务端敏感响应。离线测试覆盖 multipart 字节、文件名、MIME、精确目标、凭据隔离、重定向拒绝与失败回执；仍需真实平台账户验收其权限和平台限额。

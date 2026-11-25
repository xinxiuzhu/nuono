# 插件 Dashboard 页面

插件可以提供独立构建的页面包，由官方 Dashboard 在插件详情中显示按钮，也可声明 Dock 入口。Dashboard 只构建一个版本，按声明加载扩展页面，不向插件公开前端内部组件。页面包可以使用 Leptos、原生 HTML 或其他浏览器框架；后端 SDK 和下面的 Bridge 协议是公开契约。

## 声明与构建

在 `Plugin::web_pages()` 中返回纯声明。页面文件通过 `PluginWebAsset` 嵌入插件，与插件一起静态链接；修改页面后先构建页面，再重建并重启宿主。无需重编译官方 Dashboard。

```rust
use nuono_plugin::{PluginWebAsset, PluginWebPage, PluginWebPagePlacement};

fn web_pages(&self) -> Vec<PluginWebPage> {
    vec![PluginWebPage::new("overview", "插件工作台", vec![
        PluginWebAsset::new("index.html", "text/html; charset=utf-8",
            include_bytes!("../web/dist/index.html")),
        PluginWebAsset::new("app.js", "application/javascript",
            include_bytes!("../web/dist/app.js")),
    ]).with_placement(PluginWebPagePlacement::Dock)]
}
```

默认仅具备 `ManageRuntime` 权限的账号可见。面向普通登录用户的页面需显式调用 `.with_access(PluginWebAccessPolicy::Authenticated)`，并可通过 `.with_user_title("我的收藏")` 区分普通用户与管理员看到的入口标题。内置图标支持 `receipt`、`chart`、`settings`，其他合法标识回退使用插件图标。页面入口权限只控制发现与打开，业务 API 仍须独立校验当前调用者身份和对象归属。

页面使用相对资源 URL，例如 `./app.js`、`./app_bg.wasm`。Trunk 构建使用 `trunk build --release --public-url ./ --locked`，将本次 `dist/` 的全部文件登记为资产；清除旧的 dist 和 wasm-bindgen snippets 缓存，避免残留旧 hash。WASM 的 MIME 必须是 `application/wasm`，JS 必须是浏览器支持的 JavaScript MIME。资源通过 CORS 提供，支持 ES modules、完整性校验和 WASM 加载；带 `integrity` 的样式表和脚本必须使用 `crossorigin="anonymous"`，适配不透明 origin。不要引用 Dashboard 自己的资源 hash 或私有 Rust 模块。

插件 ID 只允许 ASCII 字母、数字、`_`、`-`。页面 ID 和图标同样限制为这些字符，长度最多 64 字节；页面 ID 在插件内唯一。每个插件最多 16 页、每页 512 个资产，全部资产合计最多 64 MiB。入口必须是已声明的 HTML；文件路径必须为规范相对路径，不允许空段、`.`、`..`、反斜线、百分号、查询串或 URL scheme。声明在迁移和生命周期回调之前校验，失败则不激活插件。

## 页面 Bridge v1

宿主在 HTML 中注入 `/api/plugin-pages/bridge.js`，提供 `window.NuonoPluginView`。页面无需保存登录凭据：

```js
const view = window.NuonoPluginView;
const context = await view.ready();
const rows = await view.apiGet("records", { limit: 20 });
await view.apiPost("records", { title: "新记录" });
const unsubscribe = view.onContext(context => {
  // context.locale: zh-CN / en / ja / ko
  // context.dark、context.role、context.username、context.support_url
});
// 页面自己的组件销毁时调用 unsubscribe()。
```

| 接口 | 行为 |
| --- | --- |
| `ready()` | 等待父容器握手，返回当前上下文；独立打开时超时报错 |
| `getContext()` | 同步读取最近一次上下文，握手前为 `null` |
| `onContext(callback)` | 立即回调已有上下文，并订阅后续变更；返回取消订阅函数 |
| `apiGet(endpoint, query?)` | 请求本插件 GET API，返回业务数据 |
| `apiPost(endpoint, body?)` | 请求本插件 POST API，返回业务数据 |
| `request(method, endpoint, body?, query?)` | 支持 GET / POST / PUT / PATCH / DELETE |
| `navigate(path)` | 请求父界面站内导航，例如 `/groups` |
| `openExternal(url)` | 请求父界面以新窗口打开 http、https 或 mailto 链接 |

`endpoint` 是不带开头斜线的插件内部路径，例如 `groups/123`，主界面固定转为 `/api/plugin/<当前插件>/groups/123`。不能请求其他插件或宿主管理 API；不要传已编码路径、完整 URL、查询串或 `..`。查询参数通过独立对象传入，值只允许字符串、数字和布尔值。

主界面负责 Cookie、CSRF、语言头与响应解包，插件后端沿用 `PluginWebRoute` / `handle_web_request`。`{success:true,data:...}` 解包为 `data`；HTTP 失败、`success:false`、超时或协议错误会拒绝 Promise。每页最多 32 个并发请求，单次 JSON 请求不超过 1 MiB、响应不超过 4 MiB，请求最多等待 30 秒。v1 面向 JSON 交互，不提供文件上传、流式响应或任意 Header 转发。

Leptos 可通过 `#[wasm_bindgen(module = "/src/bridge.mjs")]` 调用一层自己的 JS 模块，将 Bridge 返回值转成 JSON 字符串后用 serde 解码。页面自行负责业务组件和国际化，不需要链接 Dashboard Rust crate。

## 主题、生命周期与开销

主界面同步语言、明暗主题及一组字体、文字、表面和强调色 CSS variables。Bridge 自动应用到页面的 `documentElement`，页面使用这些变量即可；框架专属组件仍需页面自己维护。业务数据和权限应以每次 API 响应为准，不把上下文角色当作后端授权。

Dashboard 只读取轻量页面目录，打开页面时才请求 HTML、JS 和 WASM。当前只挂载一个 iframe，离开后销毁文档、断开主题监听并取消父容器的未完成请求。目录刷新结果不变时不重挂页面。独立页面有自己的浏览器运行时和 WASM 内存；首次加载和激活期间存在额外开销，不会同时预载所有插件页面。用户离开前应保存输入，v1 不保证跨页面保留未提交表单。

页面注册与 Web API 共用插件激活边界：校验 → 迁移 → `on_load` → `on_enable` → 注册并发布。停用、卸载、重建及失败补偿移除页面声明；再次启用产生新的激活代次，旧资产票据不能用于新实例。目录项的 `revision` 是不透明字符串，标识本轮进程与激活代次；前端在切换账号、窗口聚焦、插件详情变化及可见页面每 30 秒刷新目录，入口消失后移除当前 iframe，重建或服务重启导致代次变化时重新打开页面。

## HTTP 与隔离边界

| 路径 | 访问方式 |
| --- | --- |
| `GET /api/plugin-pages` | 真实登录会话；返回 `version:1,pages`，按当前账号和插件启用状态过滤 |
| `GET /api/plugin-pages/{plugin}/{page}` | 同上；返回页面信息和临时 `content_path` |
| `GET /api/plugin-pages/assets/{ticket}/{path}` | 随机资产票据，仅能读取对应页面的已声明文件 |
| `GET /api/plugin-pages/bridge.js` | 无凭据的公开 Bridge 客户端 |

资产票据有效期为 1 小时，仅用于静态资源读取与页面来源确认，不能代替登录凭据调用业务 API。服务端保留关联会话，每次读取资源时重新核验会话、账号角色、插件启用状态及激活代次，不向页面暴露真实会话凭据。HTML 不缓存，带 hash 的静态资产使用私有短缓存；请勿在日志或外链中记录临时资产 URL。

iframe 使用 `sandbox="allow-scripts allow-forms allow-downloads"`，不开放 `allow-same-origin`、父窗口导航或弹窗权限；响应 CSP 同样限制页面能力。页面为不透明 origin，不能读取主界面 DOM、Cookie 或 localStorage。通信先检查当前 iframe 的 source、不透明 origin、资产票据和协议版本，再建立绑定本份文档的 MessagePort，通过实例标识和 nonce 区分重载前后的请求。API 和上下文只经过私有端口；iframe 跳转到外站后，替换文档不能接收原页面的响应或主题上下文。API 路径由父界面绑定到所属插件。

这层隔离用于浏览器页面。Rust 插件仍与宿主处于同一进程、拥有同等权限；安装插件的信任边界没有变化。

Bridge 请求被服务端明确拒绝时，抛出的错误包含 HTTP `status`（400–599）。超时、网络中断或无法读取响应时不提供该字段，写操作应保留原操作编号和内容以便重试确认，不能把未知结果当作失败重新提交。

## 插件管理图标

Dashboard 的插件列表与详情保留独立图标位。开发者将图标放在插件安装根目录，与用于安装的 `Cargo.toml` 同级；内部业务目录的层级不影响这个约定。

```text
plugins/my-plugin/
├── Cargo.toml
├── README.md
├── icon.svg          # 可选，由插件开发者设计
└── src/              # 插件入口与业务代码
```

有双层业务目录的插件仍使用外层安装根目录的图标，不将图标放入 `src/` 或内部业务包目录。

支持 `icon.svg`、`icon.png`、`icon.webp`、`icon.ico`，按此顺序选择第一个存在的文件。建议只保留一种格式，使用正方形、透明背景的图标；SVG 设置 `viewBox`，位图建议至少 128 × 128 像素。单个文件上限为 512 KiB，必须是实际普通文件，不能使用符号链接。

框架在生成已安装插件的应用时检测图标，并将图标字节嵌入程序。插件不需要声明额外的 SDK 方法、配置字段或清单；没有图标时继续正常运行，Dashboard 显示统一的默认插件图标。浏览器加载失败时也会使用默认图标。

插件列表和详情 API 提供可选 `icon_url`。`GET /api/plugins/{运行名}/icon` 返回嵌入的图像，运行名由插件实例注册时确定，不要求它与安装目录名相同。URL 带内容版本，浏览器可缓存并使用 ETag；替换图标后重新构建并启动应用即可更新。仅刷新 Dashboard 不会重新读取构建机上的图标文件。

导出的应用保留嵌入的图标，部署机器无需保留插件源码。框架只发布选中的图标字节，不开放插件目录中的其他文件；SVG 作为图片显示，并限制脚本和外部资源。

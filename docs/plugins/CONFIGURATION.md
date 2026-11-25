# 插件配置表单声明

本页说明插件配置接口。正常宿主使用数据库保存全局和个人／群差异，TOML 仅作为显式导入材料；独立文件模式用于脱离数据库的嵌入调用或测试。

## 配置声明契约

`Plugin::config_fields()` 是唯一业务配置入口。每个表单字段必须通过 SDK 的 `PluginConfigField` 声明；宿主从字段的 `default` 提取默认值，不再提供独立的 `Plugin::config_default()` 或 `Plugin::config_schema()`。没有可配置项的插件可使用默认空列表。

`cargo plugin new` 生成 `src/default_config.rs`，它是组织源码的约定，不是框架扫描的特殊文件。可以改名为 `config.rs` 或把声明放在 `lib.rs`，但必须通过 Rust 模块引用并由 `config_fields()` 返回。开发者修改声明需要重新构建应用；首发用户运行值由宿主在数据库保存，不要求修改源码。

简单配置使用结构体：

```rust
use nuono_plugin::{ConfigSchema, serde::Deserialize};

#[derive(Deserialize, ConfigSchema)]
#[serde(crate = "nuono_plugin::serde")]
pub struct Config {
    /// 欢迎语
    #[config(default = "你好")]
    pub greeting: String,
    /// 温度偏移
    #[config(default = -3, min = -10, max = 10)]
    pub offset: i64,
}

// 在 Plugin 实现中只需注册：
// fn config_fields(&self) -> Vec<PluginConfigField> { Config::get_schema() }
```

derive 支持 `String`、`bool`、标准整数（值须落在 i64 范围）、`f32/f64` 和 `Vec<String>`。默认值表达式按 Rust 类型检查，支持负数和 `vec![...]`；缺省时使用该类型零值。整数范围同时受 Rust 字段宽度约束（例如 u8 为 0–255），显式边界不会放宽类型范围。未知属性、重复属性、不支持的属性组合和会改变字段映射的 serde 属性会编译报错；动态选项及其他运行时表达式仍由 SDK 校验。

分组、密码框、下拉选项和条件显示也可以直接写在 `default_config.rs` 中。`#[config(...)]` 是字段属性，不写在结构体上：

```rust
use nuono_plugin::{ConfigSchema, serde::Deserialize};

#[derive(Deserialize, ConfigSchema)]
#[serde(crate = "nuono_plugin::serde")]
pub struct Config {
    #[config(default = false, label = "下载媒体", section = "下载",
        widget = "switch")]
    pub download_enabled: bool,

    #[config(default = 49, label = "最大体积", section = "下载",
        min = -1, max = 500, unit = "MB",
        visible_when(key = "download_enabled", value = true))]
    pub max_mb: i64,

    #[config(label = "Cookie", description = "可选认证 Cookie",
        section = "平台", password)]
    pub cookie: String,

    #[config(default = "720P", label = "画质", section = "平台", widget = "select",
        options = ["1080P", "720P", "480P", "360P"])]
    pub quality: String,

    #[config(label = "禁用平台", section = "平台", editor = "multiselect",
        encoding = "json_string", options = [("B 站", "bilibili"), ("抖音", "douyin")],
        allow_custom)]
    pub disabled_platforms: Vec<String>,
}
```

| 属性 | 用途 |
| --- | --- |
| `default`、`label`、`description` | 默认值、名称、说明；字段文档仍可作为名称和说明的缺省来源 |
| `section`、`order` | 分组；默认按字段声明顺序排序，显式 `order`（i32 表达式）可覆盖 |
| `widget` | `String` 的 `text` / `textarea` / `password` / `select`，或 `bool` 的 `boolean` / `switch`；`password` 可简写为裸属性 |
| `options` | Rust 迭代表达式：字符串选项、`(标签, 值)` 元组或 `ConfigFieldOption`；也可调用函数生成选项 |
| `visible_when(key = "字段名", value = ...)` | 与指定配置值相等时显示；字段引用必须存在 |
| `min`、`max`、`unit`、`step` | 数字范围、单位和步长；范围边界保持整数语义 |
| `placeholder`、`required` | 占位提示和必填；`required` 也支持显式布尔值 |
| `editor`、`encoding` | `Vec<String>` 的 `tags` / `multiselect` 与 `native` / `json_string` 编码 |
| `allow_custom`、`user_scoped`、`group_scoped` | 允许自定义列表选项、允许个人独立配置、允许群独立配置；均可显式设置为布尔值 |

`encoding = "json_string"` 保留已有列表的字符串存储：字段默认值和 Dashboard 提交值仍是 JSON 字符串，运行时解码为 `Vec<String>`。此时 `default` 仍写 Rust 字符串向量，`Config::from_value()` 同时接受编码字符串和原生字符串数组。转换只针对宏声明的列表，不改变普通 `String` 字段或宿主的保存校验。

复杂对象、嵌套对象列表和选项来源关联继续使用下文的显式 SDK builder。宏和 builder 生成相同的 `PluginConfigField`，共用宿主校验与表单渲染。

加载插件时先校验声明：重复键、错误默认值、范围、选项、步长、组件、显示条件及选项来源引用无效时明确拒绝，不改写已保存配置。顶层必填项必须有有效默认值；对象列表中的必填空子字段可作为新条目模板，但实际条目必须通过完整校验。`ConfigSchema::from_value` 的默认实现与宿主共用 SDK 校验，`LiveConfig` 绑定失败会保留原来的句柄。额外业务约束仍由插件负责。

未声明的字段不能通过管理 API 写入或生成表单；首发导入时遇到未知键需明确处理，不静默丢弃。服务前缀、权限和禁用指令等框架字段由宿主代码声明。配置值和导入文件中的标题或范围不能改变表单、权限和校验规则。

命令前缀统一限定命令、正则、普通消息与文件规则、`on_event` 消息回调：配置非空前缀后，先经过前缀路由，再匹配去前缀正文。前缀按最长匹配选择，ASCII 字母不区分大小写；不同插件的相同前缀冲突会拒绝派发。`force_prefix` 提供并列的额外入口，`disable_force_prefix` 可关闭这些入口。旧 `allow_empty_prefix` 不能绕过非空前缀；没有有效非空前缀时直接匹配正文。

被动功能由 `Plugin::message_listener()` 独立声明范围，`on_message_listen()` 接收范围内、未被显式前缀认领的原始消息。只有声明能力的插件显示「自动监听」开关。服务字段 `auto_listen_enabled` 是保留键，不可作为业务字段声明；可群级继承或覆盖，仍受插件总开关、权限和黑白名单约束。缺失值采用声明默认（未指定则关闭），已保存的关闭状态保留。完整规则和迁移示例见 [插件消息路由](MESSAGE_ROUTING.md)。

路由确定候选插件后，消息匹配仍按带规则前缀的规则优先，再比较命令、触发器或 SV 声明的规则级 `priority`；数字越小越先执行，相同值保持本轮匹配顺序。`block` 继续决定是否停止后续匹配处理。插件整体不提供优先级配置，Dashboard 中的规则优先级为代码声明的只读信息；首发导入旧插件整体 priority 时按未知字段处理，不让它进入消息调度。

插件测试可以独立检查声明，无需启动宿主或连接数据库：

```rust
nuono_plugin::config_validation::validate_config_fields(&Config::get_schema()).unwrap();
let config = Config::from_value(&nuono_plugin::serde_json::json!({})).unwrap();
```

插件通过 `Plugin::config_fields()` 返回 `Vec<PluginConfigField>`。默认值、类型、约束和表单描述由插件代码声明；首发全局差异保存在 `nuono_plugins`，个人／群差异保存在 `nuono_plugin_overrides`。插件用 `ctx.config_handle.snapshot()`、`ctx.current_config()` 或 `LiveConfig<T>` 读取有效快照。对象配置解析由宿主结合本次消息身份完成，不把成员的个人配置叠加到群配置。

### 复用类型化配置快照

`LiveConfig<T>::get()` 每次解码并返回独立的 `T`，用法保持不变。频繁读取且字段声明、默认值和解码仅取决于已发布配置时，可用 `snapshot()` 返回共享的 `Arc<T>`（要求 `T: Send + Sync + 'static`）：

```rust,ignore
let settings = self.config.snapshot()?;
bot.send(settings.greeting.as_str()).await?;
```

相同的宿主配置快照在读取时只解析一次，后续读取复用结果；宿主发布新快照或绑定新句柄后，首次读取重新解析，绑定时仍单独校验。每个 `LiveConfig` 最多缓存一份解析结果；调用方持有的旧快照在更新后仍可读取。把返回值视为只读配置，并在一次处理开始时取一次，保证后续字段读取来自同一版本。解析失败返回错误，不用旧缓存冒充当前值。

此入口读取宿主已发布的内存快照；宿主拒绝无效修改时，已发布值继续保持原有语义。它不替代持久化入口的校验和版本检查，也不替代 `on_config_changed` 的派生资源刷新。若手写声明、默认值或解码依赖会独立变化的外部状态，继续使用逐次解码的 `get()`。

### 持久更新插件配置

插件需要持久更新自己的配置时，保存 `ctx.config_updates.clone()`，通过宿主的版本化补丁端口写入：

```rust,ignore
let before = ctx.config_updates.read_latest().await?;
let saved = ctx.config_updates.update_patch(
    nuono_plugin::serde_json::json!({"greeting": "欢迎回来"}),
    &before.revision,
).await?;
```

该端口的首发契约是返回同一版本的 `values` 和 `revision`，不把旧快照冒充最新持久值。`update_patch` 只修改补丁给出的已声明字段，保留未提交字段，版本必须提供；数据库事务内检查 revision 并校验保存，冲突后调用者重新读取并明确合并。提交成功才发布 `config_handle` 并失效对象配置缓存。该端口始终修改插件自己的全局差异；事件回调中的对象快照不会改变写入目标。

写端口由宿主绑定插件名，操作不接受其他插件名或文件路径。未接配置管理器的嵌入环境会明确返回不支持；测试可以实现 `PluginConfigUpdatePort` 并用 `PluginConfigUpdates::from_port` 注入。`on_config_changed` 由宿主在保存成功且当前插件调用释放锁后通知，持久保存成功不代表派生客户端已更新；回调失败会进入配置状态中的 `runtime_error`。不要用 `config_handle.publish` 代替持久保存。

表单描述属于宿主的活 schema。`ConfigFieldUi` 默认为空，旧声明无需填写；原有 `required`、`placeholder` 和带标签的 `options` 会递归融合进 `ui`。宿主的 `ConfigField` 各类型都有 `ui`；首发配置差异不保存 UI、动态选项来源或显示条件。修改表单布局不会覆盖用户值，也不会自动转换已有字段类型。

## 选择值类型和编辑器

下面的方法均返回 `PluginConfigField`，其中 `key`、`label`、`description` 都是 `&str`：

```rust
pub fn string_list(key: &str, label: &str, description: &str, default: Vec<String>) -> Self;
pub fn json_string_list(key: &str, label: &str, description: &str, default: &str) -> Self;
pub fn json_object_list(
    key: &str, label: &str, description: &str, default: &str,
    fields: Vec<PluginConfigField>, item_label: &str,
) -> Self;
pub fn json_number_map(key: &str, label: &str, description: &str, default: &str) -> Self;
pub fn decimal(key: &str, label: &str, description: &str, default: f64) -> Self;
```

| 构造器 | 实际配置值 | 默认表单描述 |
| --- | --- | --- |
| `string_list` | 原生字符串数组 | `field_type=token`，`editor=tags`，`encoding=native` |
| `json_string_list` | 包含 JSON 字符串数组的字符串 | `editor=tags`，`encoding=json_string` |
| `json_object_list` | 包含 JSON 对象数组的字符串 | `editor=object_list`，`encoding=json_string`，递归 `fields` 和 `item_label` |
| `json_number_map` | 包含 JSON 数字映射的字符串 | `editor=key_value`，`encoding=json_string` |
| `decimal` | 浮点数字，保留小数 | `field_type=float` |

`json_*` 构造器的默认值传入 JSON 文本，例如 `"[]"`、`"{}"`。`json_string` 是显式编码声明：Dashboard 在编辑时解析字符串，提交时再编码成字符串，插件收到的类型保持不变。宿主不会根据字段名、内容像不像 JSON 或历史用法猜测编辑器。普通 `text` / `textarea` 仍然是普通文本。

`#[derive(ConfigSchema)]` 将 `Vec<String>` 声明为真正的 Token 列表，将 `f32` / `f64` 声明为 Float；字符串和小数不再通过文本框互相代替。复杂结构使用上面的显式字段声明。

常用 builder 签名：

```rust
.with_order(order: i32)
.with_unit(unit: impl Into<String>)
.with_step(step: f64)
.with_visible_when(key: &str, value: impl Into<serde_json::Value>)
.with_options(options: Vec<ConfigFieldOption>)
.with_options_source(key: &str, value_key: &str, label_key: Option<&str>)
.with_editor(editor: impl Into<String>)
.with_encoding(encoding: impl Into<String>)
.with_fields(fields: Vec<PluginConfigField>)
.with_item_label(item_label: impl Into<String>)
.with_custom_options()
.with_allow_custom_options(allow_custom: bool)
```

这些方法可以与现有的 `.required()`、`.with_placeholder(...)`、`.with_section(...)`、`.with_min(i64)`、`.with_max(i64)` 和 `.with_default(...)` 串联使用。

`ConfigSchema` 派生宏默认按 Rust 结构体的字段声明顺序生成 `ui.order`（从 0 开始），不需要逐项写 `order`。`section` 相同的字段放入同一组，未指定排序时分组按源码中首次出现的顺序显示；建议同组字段在源码中连续声明。显式 `order` 仍可覆盖该字段的默认序号，Dashboard 按数值排序，相同数值按配置键排序，分组按组中最早的字段排列。手写 builder 的默认 `order` 仍为 0，需要自定义顺序时使用 `.with_order(...)`。顺序来自编译后的字段声明，Dashboard 不扫描 `.rs` 文件，也不以运行配置 TOML 的键顺序排序。

`unit` / `step` 描述数字输入；`step` 不代表后端会四舍五入或把值限制为步长的整数倍。当前 `min` / `max` builder 接收整数边界，字段值本身仍可为小数。

## 对象列表、原生子对象和动态选项

下面的例子保留外层模型列表的 JSON 字符串存储。每个模型中的 `pricing` 已经是 JSON 对象，因此显式使用 `native`，不会把子对象再编码成另一层字符串：

```rust
use nuono_plugin::{ConfigFieldOption, PluginConfigField, serde_json::json};

pub fn config_fields() -> Vec<PluginConfigField> {
    let pricing = PluginConfigField::json_number_map(
        "pricing", "价格表", "不同规格的单次费用", "{}",
    )
    .with_encoding("native")
    .with_default(json!({}))
    .with_fields(vec![
        PluginConfigField::decimal("value", "单价", "", 0.0)
            .with_min(0)
            .with_step(0.01)
            .with_unit("积分"),
    ]);

    let models = PluginConfigField::json_object_list(
        "models", "模型列表", "模型标识必须唯一",
        r#"[{"name":"basic","title":"基础模型","pricing":{"standard":0.05},"extensions":{"version":1}}]"#,
        vec![
            PluginConfigField::text("name", "标识", "供配置引用", "").required(),
            PluginConfigField::text("title", "显示名称", "", ""),
            pricing,
        ],
        "name",
    )
    .with_order(10);

    vec![
        PluginConfigField::switch("enabled", "启用模型", "", true),
        models,
        PluginConfigField::text("active_model", "默认模型", "", "basic")
            .with_order(20)
            .with_options_source("models", "name", Some("title"))
            .with_visible_when("enabled", true),
        PluginConfigField::string_list(
            "allowed_types", "允许的类型", "", vec!["image".into()],
        )
        .with_editor("multiselect")
        .with_options(vec![
            ConfigFieldOption { label: "图片".into(), value: "image".into() },
            ConfigFieldOption { label: "文本".into(), value: "text".into() },
        ]),
    ]
}
```

`object_list.fields` 描述对象中的已知字段，使用相同的 `PluginConfigField` 递归声明。例子中的 `extensions` 没有声明，编辑已知字段时仍保留其键和值。已有对象可以省略可选子字段，表单不会因展示而补写默认值；显式填写 `null` 仍须满足字段类型。`item_label="name"` 指定每个对象的标识：必须是非空字符串，且列表中不能重复。

`key_value` 是字符串键、有限数字值的映射，键不能为空。需要数字范围时，用 `.with_fields(vec![decimal(...).with_min(...).with_max(...)])` 提供一个值描述；`fields[0]` 的类型和范围约束应用于每个映射值。子字段名称不用于猜测映射中的用户键。

`options_source` 从另一个声明的 JSON 数组字段读取候选项：`key` 是来源配置键，`value_key` 是对象中用作选项值的键，`label_key` 可选，用作显示标签。例子使用模型 `name` 作为值、`title` 作为标签。这是动态选项展示声明，当前后端不会据此强制外键关系；当前值不在来源数组中时仍保留并显示，不能静默重置成首项。需要跨字段业务约束的插件应在业务配置解析或配置变更回调中明确校验。

`visible_when` 只影响显示。条件不成立时，字段值仍保留，保存和导入仍校验它。条件不能用来跳过必填、枚举或范围检查，也不授予群级修改权限；群级覆盖仍须插件显式声明 `.group_scoped()`。个人覆盖使用 `.user_scoped()`；两种声明都还需要部署策略允许，不能仅凭显示条件获得权限。

## 静态多选和历史值

带 `.with_options(...)` 的列表默认严格限制元素值。若插件的名单本来就允许扩展项，或历史值存在大小写、旧标识等差异，可以只对该列表显式允许自定义选项：

```rust
use nuono_plugin::{ConfigFieldOption, PluginConfigField};

let disabled = PluginConfigField::json_string_list(
    "disabled_resolvers", "禁用的平台", "保留原有平台标识", r#"["Twitter"]"#,
)
.with_editor("multiselect")
.with_options(vec![
    ConfigFieldOption { label: "Twitter".into(), value: "twitter".into() },
])
.with_custom_options();
```

`.with_custom_options()` 设置 `ui.allow_custom=true`；默认是 `false`，可用 `.with_allow_custom_options(false)` 关闭。它仅让 `tags` / `multiselect` 接受未列举的字符串，保留 `Twitter` 或未知平台标识的原值。容器仍必须是字符串列表，必填约束仍执行。其他列表保持严格枚举；普通 String / Select 的选项约束不会因这个标志放宽。

## 保存与校验语义

首发全局、个人和群设置统一使用数据库差异；UI 元数据仍来自插件代码。未设置字段采用默认值，显式 false、0、空串、空列表均保留；恢复继承使用删除覆盖字段操作。对象和数组作为一个声明字段整体替换，不自动拼接或猜测合并。

宿主对 Dashboard 提交和显式导入使用同一套验证，检查字段类型、必填、JSON 容器和子字段、静态选项、有限数字及范围、对象列表唯一标识。必填文本不能只有空白，必填容器不能为空。结构校验、权限或 revision 检查失败不写数据库、不发布候选快照；错误描述字段和约束，不回显秘密值。

默认值变化只影响未覆盖字段；不兼容的配置格式变更必须转换并验证所有层级的已存差异。跨字段约束由插件提供，事务提交后的外部资源刷新失败另报 runtime_error／需重载状态。存储事务成功不等于插件的外部客户端已全部更新。

现有结构化 type/data 与扁平 TOML 的处理仅用于开发值导入。`encoding="json_string"` 继续是声明的字段值编码，不由迁移工具偷偷重编码为别的类型。

正常宿主将 顶层 `password` 配置字段保存为受管密钥引用，读取插件有效快照时才解析明文，管理响应不回显。未提交字段保持原引用；逐项恢复继承才删除当前覆盖引用，并按仍在使用的引用回收密钥。独立于配置的凭据继续用 `secret_fields()` 声明和 `ctx.secrets` 存取。

`PluginConfigField::token` / `ConfigFieldType::Token` 表示普通字符串标签列表，不是服务凭据。服务 token 请使用顶层 `password` 或 `secret_fields()`。对象列表内不能嵌套密码字段；宿主会拒绝这类声明，并提示改为独立顶层凭据。

## 指令分组与显示名称

Dashboard 默认打开「使用设置」，命令前缀可按标签增删，与权限、范围和名单共用版本化保存。「插件配置」只显示业务参数；「指令」先按服务分组浏览，组内显示指令用法、别名和开关，原始正则在匹配详情中保留。

插件作者用已有的 `Command::with_sv("角色查询")` 声明分组。未声明的指令进入「未分组」，前端不按正则内容猜测业务归属。同一分组中的各条指令仍各自管理开关；别名属于原指令，不能作为另一条功能重复计数。分组名属于指令管理标识的一部分，发布后应保持稳定。

`Command::with_display_name("角色面板")` 可给正则等复杂匹配声明友好标题。它只影响展示和搜索，不改变 `name`、匹配模式、别名、优先级或已保存的开关键；未声明标题时界面仍可展示旧插件。

```rust,ignore
Command::new(r"^(?P<char>.+)面板$", handler)
    .with_match_type(TriggerType::Regex)
    .with_display_name("角色面板")
    .with_sv("角色查询")
    .with_description("查看角色面板与评分")
    .with_usage("ww长离面板");
```

使用宏时可直接写 `#[nuono_command(pattern = "帮助", display_name = "使用帮助", sv = "帮助与说明")]`，支持 Event 和 Message 两种处理器，也支持手动 `Handler::register()`。其他触发器宏可用 `sv` 归组；`display_name` 仅用于结构化指令，其他触发器宏会拒绝该参数，避免静默失效。无需额外依赖。

## 首发的个人与群配置

插件通过 `PluginConfigField::user_scoped()` 和 `.group_scoped()` 声明可按个人、群调整的字段，默认均关闭。部署管理员还必须在插件策略里开启对应覆盖范围。字段声明和部署开关同时允许时，账号才能保存差异；群配置另需有效的人工／插件签发授权，或宿主实时核验的平台群主、管理员角色。

全局配置叠加代码默认；私聊只叠加已验证绑定账号的个人差异，群聊只叠加群差异。不会把群主或发言人的个人配置带进群。`LiveConfig` 和配置句柄在事件回调中读取同一份不可变对象快照；受管异步任务、绘图任务继承该次快照。后台生命周期读取全局配置。

`nuono_plugins` 以稳定 `plugin_id` 为主键，保存全局差异与部署策略。`nuono_plugin_overrides` 用 `user_id` 或 `group_id` 唯一定位一份对象差异，不为每个对象建表。修改必须携带读到的 `revision`；对象覆盖同时携带 `expected_id`，删除重建后旧表单不能覆盖新记录。删除某个字段表示恢复继承，空差异自动移除。

业务配置结构改变时，插件提高 `Plugin::config_version()`，并通过 `Plugin::config_migration()` 提供 `ConfigurationMigration`。转换器收到旧版本、目标对象和显式业务差异，返回新差异。宿主在一个事务里转换并校验全局及全部个人／群覆盖；任一对象转换失败，全部回滚且拒绝激活新代码。不要把代码默认值复制到差异中，也不要在转换器中执行外部副作用。

生产宿主以数据库为唯一配置权威。历史 `config.toml` 必须先预览，逐项选择导入、保留数据库、恢复继承或舍弃。未知字段和不支持的日期类型会显示错误；确认后的配置、受管密钥及审计在同一事务提交，成功后归档源文件。运行时不再从归档或新写回的 TOML 恢复配置。顶层密码配置以及独立 `secret_fields()` 声明的密钥进入受管密钥表，管理响应不返回明文。


## 管理 API 与并发保存

以下路径均带 `/api` 前缀。ID、revision、config_version 和 UTC 毫秒时间在 JSON 中使用十进制字符串；数据库内部使用有符号 64 位整数。权限由登录账号和宿主核验，调用者不能在 JSON 中自行声明平台角色。

| 入口 | 用途 |
| --- | --- |
| `GET/POST /plugins/{name}/policy` | 读取／CAS 更新部署开关及个人、群覆盖策略；POST 提供 `expected_revision` |
| `GET /plugins/personal` | 当前账号可配置的个人字段、有效值和覆盖版本 |
| `POST /plugins/personal/{name}` | 当前账号的个人差异 |
| `GET /groups/{group_id}/plugins` | 群插件字段、有效值和覆盖版本；路径是框架群 ID |
| `POST /groups/{group_id}/plugins/{name}/config` | 群差异、单字段恢复继承和开关覆盖 |
| `GET/POST /plugins/{name}/config/import` | 预览旧 TOML／提交逐字段选择 |
| `GET /plugins/{name}/grant-delegation` | 管理员读取当前签发委托；未设置为空，读取错误不降级为空 |
| `POST /plugins/{name}/grant-delegation` | 管理员指定该扩展可签发哪些插件的群配置权限 |
| `GET/POST /groups/{group_id}/authorizations` | 管理员列出来源／人工添加独立来源 |
| `DELETE /groups/{group_id}/authorizations/{id}` | 管理员撤销一个来源 |

对象差异请求示例：

```json
{
  "expected_id": "25",
  "expected_revision": "3",
  "config_version": "1",
  "set_config": {"greeting": "你好"},
  "set_service": {},
  "remove_config": ["tone"],
  "remove_service": [],
  "inherit_enabled": true
}
```

第一次保存使用 `expected_id: null`、`expected_revision: "0"`；以后使用刚读取的记录 ID 和版本。`set_enabled` 写入对象开关；`inherit_enabled: true` 删除开关差异，两者不能同时使用。数组作为一个字段整体替换，`remove_config` / `remove_service` 按顶层字段删除。空差异删除记录，旧页面必须重新读取后再保存。

写事务先锁定并核验账号，再核验群授权或可信平台角色、插件策略和版本，最后保存差异、密钥及审计。撤销授权、注销或降权与配置保存使用一致的账号锁；失效证明不能提交旧页面的更改。平台群主／管理员资格由宿主向当前平台查询，并受部署开关控制；它与人工、扩展签发授权是独立来源。

导入 POST 提交 `source_revision`、`database_revision` 和全部字段的 `fields` 选择：`import`、`keep_database`、`inherit` 或 `discard`。源文件变化和数据库变化都会使旧预览冲突。首次空白配置在同一事务记录当前代码的 `config_version`，不会把刚导入的当前格式当作旧版本再次转换。事务失败保留源文件；成功后归档失败会明确报告已提交，运行值仍以数据库为准。

## 结构化全局配置资源

目录别名、按角色组织的评分规则等结构化配置，可以通过 `Plugin::config_resources()` 声明 `ConfigurationResourceDefinition`。声明包含字段键、展示文案、格式版本、默认 JSON、字节上限和校验函数。宿主把它作为普通全局 JSON 文本字段展示，不允许个人或群覆盖，也不另建插件业务表。字段文本保存 `{ "revision": 0, "version": 1, "value": ... }`；网页编辑保留读到的外层版本，保存时宿主检查并递增版本，恢复默认也递增，避免旧页面重新变成有效版本。

`ctx.db.read_configuration_resource(key)` 读取经当前声明校验的快照。需要与业务数据、导入回执原子提交时，使用 `ctx.db.begin()` 后在同一个事务调用 `read_configuration_resource` 和 `replace_configuration_resource(key, expected_revision, value)`。事务读取锁定该插件配置行，替换检查资源独立版本并递增插件整体配置版本，同时写入审计；不同资源各自保留版本。面向用户的管理命令仍应先在事务内调用 `require_administrator`，结构配置端口不替代操作者授权。

只有事务成功提交后才发布配置快照；宿主配置监视器随后调用 `on_config_changed`。读配置和重新启动都从数据库重新校验，未通过当前资源格式校验的配置不能激活。不要在持有插件业务锁时主动重入配置回调。资源声明升级时，使用插件 `config_migration` 转换相应 JSON 文本中的格式版本和值；遗漏转换会明确失败。

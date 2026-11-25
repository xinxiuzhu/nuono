//! Nuono 插件 SDK 的过程宏。
//!
//! 展开代码固定通过 `::nuono_plugin::...` 路径引用 SDK 类型和 SDK 重导出的宏，
//! 因此插件 crate 只需直接依赖 `nuono-plugin`。触发器与工具会提交到 `inventory`，再由宿主在
//! 插件启用时统一收集；宏生成的 handler/tool 类型名也会占用调用方模块的命名空间。

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, ItemFn, LitStr, Token, parse::Parse, parse::ParseStream, parse_macro_input};

// 解析触发器属性参数；prefix 仅表示该触发器显式声明的前缀。

#[derive(Default)]
struct TriggerArgs {
    aliases: Vec<String>,
    display_name: Option<String>,
    sv: Option<String>,
    description: Option<String>,
    usage: Option<String>,
    permission: Option<String>,
    pattern: Option<String>,
    prefix: Option<String>,
    priority: Option<i32>,
    block: Option<bool>,
    to_me: Option<bool>,
}

#[derive(Default)]
struct ToolArgs {
    name: Option<String>,
    description: Option<String>,
    risk: Option<String>,
    schema: Option<syn::Path>,
}

impl Parse for ToolArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut args = ToolArgs::default();
        let mut seen = std::collections::HashSet::new();

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            if !seen.insert(key.to_string()) {
                return Err(syn::Error::new(key.span(), "工具参数重复声明"));
            }
            input.parse::<Token![=]>()?;
            let val: LitStr = input.parse()?;

            match key.to_string().as_str() {
                "name" => args.name = Some(val.value()),
                "description" => args.description = Some(val.value()),
                "risk" => args.risk = Some(val.value()),
                "schema" => args.schema = Some(val.parse()?),
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("Unknown tool argument: {key}"),
                    ));
                }
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(args)
    }
}

impl Parse for TriggerArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut args = TriggerArgs::default();

        let mut seen = std::collections::HashSet::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            if !seen.insert(key.to_string()) {
                return Err(syn::Error::new(key.span(), "参数重复声明"));
            }
            input.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "aliases" => {
                    let content;
                    syn::bracketed!(content in input);
                    args.aliases = content
                        .parse_terminated(syn::parse::ParseBuffer::parse::<LitStr>, Token![,])?
                        .iter()
                        .map(LitStr::value)
                        .collect();
                    if args.aliases.iter().any(|a| a.trim().is_empty()) {
                        return Err(syn::Error::new(key.span(), "指令别名不能为空"));
                    }
                }
                "display_name" | "sv" => {
                    let value: LitStr = input.parse()?;
                    if value.value().trim().is_empty() {
                        return Err(syn::Error::new(value.span(), "显示名称和服务分组不能为空"));
                    }
                    if key == "sv" {
                        args.sv = Some(value.value());
                    } else {
                        args.display_name = Some(value.value());
                    }
                }
                "description" => args.description = Some(input.parse::<LitStr>()?.value()),
                "usage" => args.usage = Some(input.parse::<LitStr>()?.value()),
                "permission" => {
                    let value = input.parse::<LitStr>()?;
                    if !matches!(
                        value.value().as_str(),
                        "everyone" | "group_admin" | "group_owner" | "bot_admin" | "bot_owner"
                    ) {
                        return Err(syn::Error::new(
                            value.span(),
                            "权限可选 everyone、group_admin、group_owner、bot_admin、bot_owner",
                        ));
                    }
                    args.permission = Some(value.value());
                }
                "pattern" => {
                    let val: LitStr = input.parse()?;
                    args.pattern = Some(val.value());
                }
                "prefix" => {
                    let val: LitStr = input.parse()?;
                    args.prefix = Some(val.value());
                }
                "priority" => {
                    let val: syn::LitInt = input.parse()?;
                    args.priority = Some(val.base10_parse()?);
                }
                "block" => {
                    let val: syn::LitBool = input.parse()?;
                    args.block = Some(val.value);
                }
                "to_me" => {
                    let val: syn::LitBool = input.parse()?;
                    args.to_me = Some(val.value);
                }
                // area 曾被接受但生成代码从不使用；显式报错优于静默失效。
                "area" => {
                    return Err(syn::Error::new(
                        key.span(),
                        "`area` 参数尚未实现，触发器作用域请通过 Plugin::service_config 声明",
                    ));
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("Unknown argument: {key}"),
                    ));
                }
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(args)
    }
}

// 生成触发器注册代码

/// 从 handler 函数签名里识别可选的第 4 个参数 `state: &T`。
///
/// 返回 `(Option<T 的类型>, 传给原函数的实参表达式)`：
/// - 3 参签名 `(bot, msg, data)`：`(None, quote!{})`，保持旧用法；
/// - 4 参签名 `(bot, msg, data, state)`：状态类型来自最后一个参数，插件宏生成的
///   `commands()` / `triggers()` 会按 `TypeId` 从插件 struct 的 `Arc<T>` 字段注入。
fn extract_state_param(
    fn_name: &syn::Ident,
    inputs: &syn::punctuated::Punctuated<syn::FnArg, syn::Token![,]>,
) -> syn::Result<(Option<syn::Type>, proc_macro2::TokenStream)> {
    match inputs.len() {
        3 => Ok((None, quote! {})),
        4 => {
            let syn::FnArg::Typed(last) = inputs.last().expect("len == 4") else {
                return Err(syn::Error::new(
                    fn_name.span(),
                    "触发器函数的第 4 个参数不能是 self，签名应为 (bot, msg, data, state: &T)",
                ));
            };
            let syn::Type::Reference(ty_ref) = &*last.ty else {
                return Err(syn::Error::new(
                    fn_name.span(),
                    "触发器函数的第 4 个参数（状态）必须按引用传入：state: &MyState",
                ));
            };
            Ok((Some((*ty_ref.elem).clone()), quote! { state }))
        }
        n => Err(syn::Error::new(
            fn_name.span(),
            format!(
                "触发器函数签名不合法：需要 (bot, msg, data) 或 (bot, msg, data, state: &T)，实际 {n} 个参数"
            ),
        )),
    }
}

fn generate_trigger_registration(
    trigger_type: &str,
    args: TriggerArgs,
    fn_item: &ItemFn,
) -> TokenStream {
    let fn_name = &fn_item.sig.ident;
    let fn_vis = &fn_item.vis;

    let display_name = args
        .display_name
        .as_deref()
        .map_or_else(|| quote!(None::<&str>), |name| quote!(Some(#name)));
    let sv_name = args
        .sv
        .as_deref()
        .map_or_else(|| quote!(None::<&str>), |name| quote!(Some(#name)));
    let aliases = &args.aliases;
    let description = args.description.as_deref().unwrap_or("");
    let usage = args.usage.as_deref().unwrap_or("");
    let permission = permission_tokens(args.permission.as_deref());
    if trigger_type == "Command" && args.prefix.is_some() {
        return syn::Error::new(
            fn_name.span(),
            "指令前缀请在 service_config 中声明，指令宏不接受 prefix 参数",
        )
        .to_compile_error()
        .into();
    }
    if trigger_type != "Command"
        && (!aliases.is_empty()
            || args.description.is_some()
            || args.usage.is_some()
            || args.permission.is_some()
            || args.display_name.is_some())
    {
        return syn::Error::new(fn_name.span(), "别名、说明、用法和权限请写在指令宏上")
            .to_compile_error()
            .into();
    }
    let pattern = args.pattern.unwrap_or_else(|| fn_name.to_string());
    let prefix = args.prefix.unwrap_or_default();
    let priority = args.priority.unwrap_or(0);
    // 命令族默认阻断（与 Command::new 一致）；监听型触发器默认放行。消除
    // “builder 默认 true、属性宏默认 false” 的历史分叉。
    let block = args.block.unwrap_or(trigger_type == "Command");
    let to_me = args.to_me.unwrap_or(false);

    let trigger_type_ident = match trigger_type {
        "Prefix" => quote! { ::nuono_plugin::TriggerType::Prefix },
        "Keyword" => quote! { ::nuono_plugin::TriggerType::Keyword },
        "FullMatch" => quote! { ::nuono_plugin::TriggerType::FullMatch },
        "Suffix" => quote! { ::nuono_plugin::TriggerType::Suffix },
        "Regex" => quote! { ::nuono_plugin::TriggerType::Regex },
        "File" => quote! { ::nuono_plugin::TriggerType::File },
        "Message" => quote! { ::nuono_plugin::TriggerType::Message },
        _ => quote! { ::nuono_plugin::TriggerType::Command },
    };

    // 状态注入是编译期签名检查 + 运行期 TypeId 匹配，两处不一致会得到带指引的错误。
    let (state_ty, state_arg) = match extract_state_param(fn_name, &fn_item.sig.inputs) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };

    let struct_name = format!("{}Handler", to_pascal_case(&fn_name.to_string()));
    let struct_ident = syn::Ident::new(&struct_name, fn_name.span());
    let factory_ident = format_ident!("__nuono_create_{}_handler", fn_name);

    let (struct_def, handle_body, factory_body, state_type_field, register_ctor) = match &state_ty {
        Some(ty) => (
            quote! {
                #fn_vis struct #struct_ident {
                    /// 由 `#[nuono_plugin]` 生成的 commands()/triggers() 按 TypeId 注入。
                    pub state: Option<std::sync::Arc<#ty>>,
                }
            },
            quote! {
                let state = self.state.as_deref().ok_or_else(|| ::nuono_plugin::anyhow!(
                    "handler {} 需要 {} 状态，但未注入：请让插件 struct 持有 Arc<{}> 字段，\
                     或在手写 commands()/triggers() 时用 __decorators::commands_with 显式传入",
                    #struct_name,
                    stringify!(#ty),
                    stringify!(#ty),
                ))?;
                #fn_name(bot, event, match_data, #state_arg).await.map_err(::core::convert::Into::into)
            },
            quote! {
                |state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>| {
                    std::sync::Arc::new(#struct_ident {
                        state: state.and_then(|s| s.downcast::<#ty>().ok()),
                    })
                }
            },
            quote! { Some(|| std::any::TypeId::of::<#ty>()) },
            quote! { #struct_ident { state: None } },
        ),
        None => (
            quote! { #fn_vis struct #struct_ident; },
            quote! { #fn_name(bot, event, match_data).await.map_err(::core::convert::Into::into) },
            quote! {
                |_state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>| {
                    std::sync::Arc::new(#struct_ident)
                }
            },
            quote! { None },
            quote! { #struct_ident },
        ),
    };

    let expanded = quote! {
        #fn_item

        #struct_def

        #[::nuono_plugin::async_trait]
        impl ::nuono_plugin::TriggerHandler for #struct_ident {
            async fn handle(
                &self,
                bot: &::nuono_plugin::Bot,
                event: &::nuono_plugin::Message,
                match_data: ::nuono_plugin::MatchData,
            ) -> ::nuono_plugin::Result<()> {
                #handle_body
            }
        }

        impl #struct_ident {
            pub fn register(registry: &::nuono_plugin::TriggerRegistry) {
                use ::nuono_plugin::{Trigger, TriggerType, TriggerHandler};

                let mut trigger = Trigger::new(
                    #trigger_type_ident,
                    #pattern,
                    #prefix,
                    #block,
                    #to_me,
                    std::sync::Arc::new(#register_ctor),
                )
                .with_priority(#priority);
                trigger.sv_name = #sv_name.map(str::to_string);

                if #trigger_type_ident == TriggerType::Command {
                    let mut command = ::nuono_plugin::Command::from_message(#pattern, std::sync::Arc::new(#register_ctor))
                        .with_description(#description).with_permission(#permission)
                        .with_priority(#priority).with_block(#block).with_to_me(#to_me);
                    command.aliases = vec![#(#aliases.to_string()),*];
                    command.usage = #usage.to_string();
                    command.display_name = #display_name.map(str::to_string);
                    command.sv_name = #sv_name.map(str::to_string);
                    registry.register_command(command);
                } else {
                    registry.register(trigger);
                }
            }
        }

        #[doc(hidden)]
        pub fn #factory_ident(
            state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
        ) -> std::sync::Arc<dyn ::nuono_plugin::TriggerHandler> {
            let __factory = #factory_body;
            __factory(state)
        }

        ::nuono_plugin::inventory::submit! {
            ::nuono_plugin::__decorators::DecoratedTrigger {
                owner: env!("CARGO_PKG_NAME"),
                trigger_type: #trigger_type_ident,
                pattern: #pattern,
                display_name: #display_name,
                sv_name: #sv_name,
                aliases: &[#(#aliases),*],
                description: #description,
                usage: #usage,
                permission: #permission,
                prefix: #prefix,
                priority: #priority,
                block: #block,
                to_me: #to_me,
                state_type: #state_type_field,
                handler: #factory_ident,
            }
        }
    };

    TokenStream::from(expanded)
}

fn generate_command_registration(args: TriggerArgs, fn_item: &ItemFn) -> TokenStream {
    let fn_name = &fn_item.sig.ident;
    let fn_vis = &fn_item.vis;
    let display_name = args
        .display_name
        .as_deref()
        .map_or_else(|| quote!(None::<&str>), |name| quote!(Some(#name)));
    let sv_name = args
        .sv
        .as_deref()
        .map_or_else(|| quote!(None::<&str>), |name| quote!(Some(#name)));
    let aliases = &args.aliases;
    let description = args.description.as_deref().unwrap_or("");
    let usage = args.usage.as_deref().unwrap_or("");
    let permission = permission_tokens(args.permission.as_deref());
    let pattern = args.pattern.unwrap_or_else(|| fn_name.to_string());
    let priority = args.priority.unwrap_or(0);
    let block = args.block.unwrap_or(true);
    let to_me = args.to_me.unwrap_or(false);

    if args.prefix.is_some() {
        return syn::Error::new(
            fn_name.span(),
            "command 不接受 prefix 参数；请在插件 service_config 中声明业务前缀",
        )
        .to_compile_error()
        .into();
    }

    // 简单回复只需要 bot；需要参数或事件时再添加，不强迫作者写占位参数。
    let (state_ty, call_args) = match event_handler_arguments(fn_name, &fn_item.sig.inputs) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };
    let struct_name = format!("{}Handler", to_pascal_case(&fn_name.to_string()));
    let struct_ident = syn::Ident::new(&struct_name, fn_name.span());
    let factory_ident = format_ident!("__nuono_create_{}_command_handler", fn_name);

    let (struct_def, handle_body, factory_body, state_type_field) = match &state_ty {
        Some(ty) => (
            quote! {
                #fn_vis struct #struct_ident {
                    pub state: Option<std::sync::Arc<#ty>>,
                }
            },
            quote! {
                let state = self.state.as_deref().ok_or_else(|| ::nuono_plugin::anyhow!(
                    "handler {} 需要 {} 状态，但未注入：请让插件 struct 持有 Arc<{}> 字段，或在手写 commands() 时用 __decorators::commands_with 显式传入",
                    #struct_name,
                    stringify!(#ty),
                    stringify!(#ty),
                ))?;
                #fn_name(#call_args).await.map_err(::core::convert::Into::into)
            },
            quote! {
                |state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>| {
                    std::sync::Arc::new(#struct_ident {
                        state: state.and_then(|s| s.downcast::<#ty>().ok()),
                    })
                }
            },
            quote! { Some(|| std::any::TypeId::of::<#ty>()) },
        ),
        None => (
            quote! { #fn_vis struct #struct_ident; },
            quote! { #fn_name(#call_args).await.map_err(::core::convert::Into::into) },
            quote! {
                |_state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>| {
                    std::sync::Arc::new(#struct_ident)
                }
            },
            quote! { None },
        ),
    };

    TokenStream::from(quote! {
        #fn_item

        #struct_def

        #[::nuono_plugin::async_trait]
        impl ::nuono_plugin::CommandHandler for #struct_ident {
            async fn handle(
                &self,
                bot: &::nuono_plugin::Bot,
                event: &::nuono_plugin::Event<'_>,
                match_data: ::nuono_plugin::MatchData,
            ) -> ::nuono_plugin::Result<()> {
                #handle_body
            }
        }

        #[doc(hidden)]
        pub fn #factory_ident(
            state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
        ) -> std::sync::Arc<dyn ::nuono_plugin::CommandHandler> {
            let __factory = #factory_body;
            __factory(state)
        }

        ::nuono_plugin::inventory::submit! {
            ::nuono_plugin::__decorators::DecoratedCommand {
                owner: env!("CARGO_PKG_NAME"),
                pattern: #pattern,
                display_name: #display_name,
                sv_name: #sv_name,
                aliases: &[#(#aliases),*],
                description: #description,
                usage: #usage,
                permission: #permission,
                priority: #priority,
                block: #block,
                to_me: #to_me,
                state_type: #state_type_field,
                handler: #factory_ident,
            }
        }
    })
}

fn event_handler_arguments(
    name: &syn::Ident,
    inputs: &syn::punctuated::Punctuated<syn::FnArg, syn::Token![,]>,
) -> syn::Result<(Option<syn::Type>, proc_macro2::TokenStream)> {
    match inputs.len() {
        1 => Ok((None, quote! { bot })),
        2 => {
            let syn::FnArg::Typed(arg) = &inputs[1] else {
                return Err(syn::Error::new(name.span(), "指令处理函数不能有 self 参数"));
            };
            match arg.ty.as_ref() {
                syn::Type::Reference(reference) if matches!(reference.elem.as_ref(), syn::Type::Path(path) if path.path.segments.last().is_some_and(|s| s.ident == "Event")) => {
                    Ok((None, quote! { bot, event }))
                }
                syn::Type::Path(path)
                    if path
                        .path
                        .segments
                        .last()
                        .is_some_and(|s| s.ident == "MatchData") =>
                {
                    Ok((None, quote! { bot, match_data }))
                }
                _ => Err(syn::Error::new_spanned(
                    &arg.ty,
                    "第二个参数可写 &Event<'_> 或 MatchData；注入状态时使用 (bot, event, data, state: &T)",
                )),
            }
        }
        _ => {
            let (state, argument) = extract_state_param(name, inputs)?;
            let arguments = if state.is_some() {
                quote! { bot, event, match_data, #argument }
            } else {
                quote! { bot, event, match_data }
            };
            Ok((state, arguments))
        }
    }
}

fn generate_tool_registration(args: ToolArgs, fn_item: ItemFn) -> TokenStream {
    if fn_item.sig.asyncness.is_none()
        || fn_item.sig.inputs.len() != 2
        || !fn_item.sig.generics.params.is_empty()
    {
        return syn::Error::new_spanned(&fn_item.sig,
            "工具需要 async fn(context: ToolContext 或 AgentToolContext, input: 参数类型) -> NuonoResult<可序列化结果>")
            .to_compile_error().into();
    }
    let Some(syn::FnArg::Typed(input)) = fn_item.sig.inputs.iter().nth(1) else {
        return syn::Error::new_spanned(&fn_item.sig, "工具参数必须是拥有所有权的类型")
            .to_compile_error()
            .into();
    };
    let input_type = &input.ty;
    if matches!(input_type.as_ref(), syn::Type::Reference(_)) {
        return syn::Error::new_spanned(input_type, "工具参数必须是拥有所有权的类型")
            .to_compile_error()
            .into();
    }
    let schema = match args.schema {
        Some(path) => quote! { #path() },
        None => quote! {{
            let mut schema = ::nuono_plugin::schemars::generate::SchemaSettings::draft2020_12()
                .for_deserialize().into_generator().into_root_schema_for::<#input_type>().to_value();
            // JSON Value（含类型别名）及对象联合类型生成的 Schema 可能省略顶层 `type`，
            // 而 LLM 函数调用的参数 Schema 要求根节点必须声明为 `object`。
            if let Some(object) = schema.as_object_mut() {
                object.entry("type").or_insert_with(|| ::nuono_plugin::Value::String("object".into()));
            }
            schema
        }},
    };
    let fn_name = &fn_item.sig.ident;
    let fn_vis = &fn_item.vis;

    let tool_name = args.name.unwrap_or_else(|| fn_name.to_string());
    let description = args.description.unwrap_or_default();
    let risk = args.risk.unwrap_or_else(|| "read_only".to_string());
    let risk_ident = match risk.as_str() {
        "read_only" | "readonly" | "ReadOnly" => quote! { ::nuono_plugin::ToolRiskLevel::ReadOnly },
        "low" | "low_risk" | "LowRisk" => quote! { ::nuono_plugin::ToolRiskLevel::LowRisk },
        "approval" | "requires_approval" | "RequiresApproval" => {
            quote! { ::nuono_plugin::ToolRiskLevel::RequiresApproval }
        }
        "admin" | "admin_only" | "AdminOnly" => quote! { ::nuono_plugin::ToolRiskLevel::AdminOnly },
        _ => {
            return syn::Error::new(fn_name.span(), format!("Unsupported tool risk: {risk}"))
                .to_compile_error()
                .into();
        }
    };

    let struct_name = format!("{}Tool", to_pascal_case(&fn_name.to_string()));
    let struct_ident = syn::Ident::new(&struct_name, fn_name.span());
    let factory_ident = format_ident!("__nuono_create_{}_tool", fn_name);

    let expanded = quote! {
        #fn_item

        #fn_vis struct #struct_ident;

        #[::nuono_plugin::async_trait]
        impl ::nuono_plugin::AgentTool for #struct_ident {
            fn name(&self) -> &str {
                #tool_name
            }

            fn description(&self) -> &str {
                #description
            }

            fn risk_level(&self) -> ::nuono_plugin::ToolRiskLevel {
                #risk_ident
            }

            fn input_schema(&self) -> Option<::nuono_plugin::Value> {
                Some(#schema)
            }

            async fn call(
                &self,
                ctx: ::nuono_plugin::AgentToolContext,
                input: ::nuono_plugin::Value,
            ) -> ::nuono_plugin::NuonoResult<::nuono_plugin::Value> {
                let input: #input_type = ::nuono_plugin::serde_json::from_value(input)
                    .map_err(|_| ::nuono_plugin::NuonoError::Validation("工具参数无法转换为声明类型".into()))?;
                let ctx = ::std::convert::TryInto::try_into(ctx)
                    .map_err(|_| ::nuono_plugin::NuonoError::Validation("工具调用身份上下文无效".into()))?;
                let output = #fn_name(ctx, input).await?;
                ::nuono_plugin::serde_json::to_value(output)
                    .map_err(|_| ::nuono_plugin::NuonoError::Validation("工具结果无法序列化".into()))
            }
        }

        #[doc(hidden)]
        fn #factory_ident() -> std::sync::Arc<dyn ::nuono_plugin::AgentTool> {
            std::sync::Arc::new(#struct_ident)
        }

        ::nuono_plugin::inventory::submit! {
            ::nuono_plugin::__decorators::DecoratedTool {
                owner: env!("CARGO_PKG_NAME"),
                handler: #factory_ident,
            }
        }
    };

    TokenStream::from(expanded)
}

fn to_pascal_case(input: &str) -> String {
    let mut out = String::new();
    let mut capitalize_next = true;

    for ch in input.chars() {
        if ch == '_' || ch == '-' || ch == ' ' {
            capitalize_next = true;
            continue;
        }

        if capitalize_next {
            out.extend(ch.to_uppercase());
            capitalize_next = false;
        } else {
            out.push(ch);
        }
    }

    if out.is_empty() {
        "Generated".to_string()
    } else {
        out
    }
}

// 简单属性宏（只生成注册辅助方法）

fn permission_tokens(raw: Option<&str>) -> proc_macro2::TokenStream {
    match raw {
        Some("group_admin") => quote!(::nuono_plugin::PermissionLevel::GroupAdmin),
        Some("group_owner") => quote!(::nuono_plugin::PermissionLevel::GroupOwner),
        Some("bot_admin") => quote!(::nuono_plugin::PermissionLevel::BotAdmin),
        Some("bot_owner") => quote!(::nuono_plugin::PermissionLevel::BotOwner),
        _ => quote!(::nuono_plugin::PermissionLevel::Everyone),
    }
}

fn takes_onebot_message(fn_item: &ItemFn) -> bool {
    fn_item.sig.inputs.iter().nth(1).is_some_and(|arg| {
        matches!(arg, syn::FnArg::Typed(arg) if matches!(arg.ty.as_ref(), syn::Type::Reference(reference) if matches!(reference.elem.as_ref(), syn::Type::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == "Message"))))
    })
}

fn generate_event_trigger_registration(
    trigger_type: &str,
    args: TriggerArgs,
    fn_item: ItemFn,
) -> TokenStream {
    let fn_name = &fn_item.sig.ident;
    let fn_vis = &fn_item.vis;
    if !args.aliases.is_empty()
        || args.description.is_some()
        || args.usage.is_some()
        || args.permission.is_some()
        || args.display_name.is_some()
    {
        return syn::Error::new(fn_name.span(), "别名、说明、用法和权限请写在指令宏上")
            .to_compile_error()
            .into();
    }
    let sv_name = args
        .sv
        .as_deref()
        .map(|name| quote!(Some(#name)))
        .unwrap_or_else(|| quote!(None::<&str>));
    let pattern = args.pattern.unwrap_or_else(|| fn_name.to_string());
    let prefix = args.prefix.unwrap_or_default();
    let priority = args.priority.unwrap_or(0);
    let block = args.block.unwrap_or(false);
    let to_me = args.to_me.unwrap_or(false);
    let trigger_type_ident = match trigger_type {
        "Prefix" => quote! { ::nuono_plugin::TriggerType::Prefix },
        "Keyword" => quote! { ::nuono_plugin::TriggerType::Keyword },
        "FullMatch" => quote! { ::nuono_plugin::TriggerType::FullMatch },
        "Suffix" => quote! { ::nuono_plugin::TriggerType::Suffix },
        "Regex" => quote! { ::nuono_plugin::TriggerType::Regex },
        "File" => quote! { ::nuono_plugin::TriggerType::File },
        "Message" => quote! { ::nuono_plugin::TriggerType::Message },
        _ => quote! { ::nuono_plugin::TriggerType::EventCommand },
    };

    let (state_ty, call_args) = match event_handler_arguments(fn_name, &fn_item.sig.inputs) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };
    let struct_name = format!("{}Handler", to_pascal_case(&fn_name.to_string()));
    let struct_ident = syn::Ident::new(&struct_name, fn_name.span());
    let factory_ident = format_ident!("__nuono_create_{}_event_trigger_handler", fn_name);

    let (struct_def, handle_body, factory_body, state_type_field) = match &state_ty {
        Some(ty) => (
            quote! {
                #fn_vis struct #struct_ident {
                    pub state: Option<std::sync::Arc<#ty>>,
                }
            },
            quote! {
                let state = self.state.as_deref().ok_or_else(|| ::nuono_plugin::anyhow!(
                    "handler {} 需要 {} 状态，但未注入：请让插件 struct 持有 Arc<{}> 字段",
                    #struct_name,
                    stringify!(#ty),
                    stringify!(#ty),
                ))?;
                #fn_name(#call_args).await.map_err(::core::convert::Into::into)
            },
            quote! {
                |state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>| {
                    std::sync::Arc::new(#struct_ident {
                        state: state.and_then(|s| s.downcast::<#ty>().ok()),
                    })
                }
            },
            quote! { Some(|| std::any::TypeId::of::<#ty>()) },
        ),
        None => (
            quote! { #fn_vis struct #struct_ident; },
            quote! { #fn_name(#call_args).await.map_err(::core::convert::Into::into) },
            quote! {
                |_state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>| {
                    std::sync::Arc::new(#struct_ident)
                }
            },
            quote! { None },
        ),
    };

    TokenStream::from(quote! {
        #fn_item

        #struct_def

        #[::nuono_plugin::async_trait]
        impl ::nuono_plugin::CommandHandler for #struct_ident {
            async fn handle(
                &self,
                bot: &::nuono_plugin::Bot,
                event: &::nuono_plugin::Event<'_>,
                match_data: ::nuono_plugin::MatchData,
            ) -> ::nuono_plugin::Result<()> {
                #handle_body
            }
        }

        #[doc(hidden)]
        pub fn #factory_ident(
            state: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
        ) -> std::sync::Arc<dyn ::nuono_plugin::CommandHandler> {
            let __factory = #factory_body;
            __factory(state)
        }

        ::nuono_plugin::inventory::submit! {
            ::nuono_plugin::__decorators::DecoratedEventTrigger {
                owner: env!("CARGO_PKG_NAME"),
                trigger_type: #trigger_type_ident,
                pattern: #pattern,
                sv_name: #sv_name,
                prefix: #prefix,
                priority: #priority,
                block: #block,
                to_me: #to_me,
                state_type: #state_type_field,
                handler: #factory_ident,
            }
        }
    })
}

fn generate_simple_trigger(trigger_type: &str, args: TokenStream, input_fn: ItemFn) -> TokenStream {
    let parsed_args = if args.is_empty() {
        TriggerArgs::default()
    } else {
        match syn::parse::<TriggerArgs>(args) {
            Ok(a) => a,
            Err(e) => return e.to_compile_error().into(),
        }
    };

    if takes_onebot_message(&input_fn) {
        generate_trigger_registration(trigger_type, parsed_args, &input_fn)
    } else {
        generate_event_trigger_registration(trigger_type, parsed_args, input_fn)
    }
}

// 插件宏 — 自动生成 Plugin trait 实现

struct PluginArgs {
    name: String,
    version: String,
    description: String,
    /// impl 块形态下用于状态注入的字段名列表（struct 形态自动收集全部 Arc 字段）。
    state_fields: Vec<syn::Ident>,
}

impl Parse for PluginArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut name: Option<String> = None;
        let mut version = String::from("0.1.0");
        let mut description = String::new();
        let mut state_fields = Vec::new();

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let val: LitStr = input.parse()?;

            match key.to_string().as_str() {
                "name" => name = Some(val.value()),
                "version" => version = val.value(),
                "description" => description = val.value(),
                "state_fields" => {
                    for field in val
                        .value()
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        if !field.chars().all(|c| c.is_alphanumeric() || c == '_') {
                            return Err(syn::Error::new(
                                val.span(),
                                format!("state_fields 包含非法字段名: {field}"),
                            ));
                        }
                        state_fields.push(syn::Ident::new(field, val.span()));
                    }
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("Unknown plugin arg: {}", key),
                    ));
                }
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        // 插件名是配置目录、数据库记录和工具命名空间的主键，
        // 缺失时必须编译报错，而不是静默使用 "unnamed" 造成多插件冲突。
        let name = name.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[nuono_plugin] 缺少必填参数 name = \"...\"（插件名是全局唯一标识）",
            )
        })?;

        Ok(PluginArgs {
            name,
            version,
            description,
            state_fields,
        })
    }
}

/// `#[nuono_plugin]` 有两种标注形态，生成的内容一致：
///
/// **struct 形态**（快速开始）：标注在插件 struct 上，宏生成完整的 `Plugin` 实现
/// ——`name` / `version` / `description` 与装饰器收集器 `triggers()` / `commands()` /
/// `tools()`。struct 的每个 `Arc<T>` 字段自动成为状态注入候选。生命周期钩子使用
/// trait 默认实现；需要定制时换用 impl 块形态。
///
/// **impl 块形态**（需要定制）：直接标注在你自己的 `impl Plugin for X` 块上，宏只
/// 把**缺失**的方法合并进同一个块，你写的 `on_load` / `event_filter` / `config_*`
/// 等原样保留。注意 Rust 不允许把一个 trait 实现拆到多个 impl 块，所以定制必须
/// 在这一个块里完成。状态注入用 `state_fields = "state, downloads"` 声明字段名。
///
/// 两种形态的完整示例见
/// [SDK 宏文档](../nuono_plugin/attr.nuono_plugin.html#examples)。
/// 示例随 SDK 的文档测试编译并运行，使用真实 SDK 类型与宏展开；
/// 过程宏 crate 不反向依赖 SDK。
#[proc_macro_attribute]
pub fn nuono_plugin(args: TokenStream, input: TokenStream) -> TokenStream {
    let parsed_args = match syn::parse::<PluginArgs>(args) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };

    if let Ok(item) = syn::parse::<syn::ItemStruct>(input.clone()) {
        return expand_plugin_on_struct(parsed_args, item);
    }
    if let Ok(item) = syn::parse::<syn::ItemImpl>(input) {
        return match expand_plugin_on_impl(parsed_args, item) {
            Ok(tokens) => tokens,
            Err(e) => e.to_compile_error().into(),
        };
    }
    syn::Error::new(
        proc_macro2::Span::call_site(),
        "#[nuono_plugin] 只能标注在插件 struct 或 `impl Plugin for ...` 块上",
    )
    .to_compile_error()
    .into()
}

/// struct 形态：收集全部 `Arc<T>` 字段后生成完整实现。
fn expand_plugin_on_struct(args: PluginArgs, item: syn::ItemStruct) -> TokenStream {
    let struct_name = &item.ident;
    let PluginArgs {
        name,
        version,
        description,
        ..
    } = args;

    // 收集所有 Arc<T> 字段：生成的 commands()/triggers() 把它们作为状态候选
    // 传给 __decorators，装饰器 handler 按 TypeId 领取。非 Arc 字段不参与。
    let state_fields: Vec<proc_macro2::TokenStream> = match &item.fields {
        syn::Fields::Named(fields) => fields
            .named
            .iter()
            .filter(|f| {
                let syn::Type::Path(path) = &f.ty else {
                    return false;
                };
                path.path
                    .segments
                    .last()
                    .map(|seg| seg.ident == "Arc")
                    .unwrap_or(false)
            })
            .filter_map(|f| {
                let ident = f.ident.as_ref()?;
                Some(quote! { self.#ident.clone() })
            })
            .collect(),
        _ => Vec::new(),
    };

    let (meta, collectors) = plugin_impl_methods(&name, &version, &description, &state_fields);

    let expanded = quote! {
        #item

        #[::nuono_plugin::async_trait]
        impl ::nuono_plugin::Plugin for #struct_name {
            #(#meta)*
            #(#collectors)*
        }
    };

    TokenStream::from(expanded)
}

/// impl 块形态：把缺失的方法合并进用户已有的 `impl Plugin for X` 块。
fn expand_plugin_on_impl(args: PluginArgs, mut item: syn::ItemImpl) -> syn::Result<TokenStream> {
    let is_plugin_impl = match &item.trait_ {
        Some((path, _)) => path
            .segments
            .last()
            .map(|seg| seg.ident == "Plugin")
            .unwrap_or(false),
        None => false,
    };
    if !is_plugin_impl {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "impl 块形态要求 `impl ::nuono_plugin::Plugin for ...`；标注普通 struct 请去掉 trait",
        ));
    }

    // 已手写的方法名：宏不再生成，避免 E0201 重复定义。
    let existing: Vec<String> = item
        .items
        .iter()
        .filter_map(|i| {
            if let syn::ImplItem::Fn(f) = i {
                Some(f.sig.ident.to_string())
            } else {
                None
            }
        })
        .collect();

    // 去掉用户已写的 #[async_trait]，由宏统一补一份，避免属性重复。
    item.attrs.retain(|attr| {
        !attr
            .path()
            .segments
            .last()
            .map(|seg| seg.ident == "async_trait")
            .unwrap_or(false)
    });

    let PluginArgs {
        name,
        version,
        description,
        state_fields,
    } = args;
    let state_exprs: Vec<proc_macro2::TokenStream> = state_fields
        .iter()
        .map(|ident| quote! { self.#ident.clone() })
        .collect();

    let (meta, collectors) = plugin_impl_methods(&name, &version, &description, &state_exprs);
    let missing: Vec<proc_macro2::TokenStream> = meta
        .into_iter()
        .chain(collectors)
        .filter(|tokens| {
            // 每个生成方法形如 `fn name(...) ...`：取 `fn` 之后的 ident 与已有方法比对。
            let mut iter = tokens.clone().into_iter();
            let mut method_name = None;
            while let Some(tt) = iter.next() {
                if let proc_macro2::TokenTree::Ident(ident) = &tt
                    && ident == "fn"
                {
                    if let Some(proc_macro2::TokenTree::Ident(next)) = iter.next() {
                        method_name = Some(next.to_string());
                    }
                    break;
                }
            }
            !matches!(&method_name, Some(m) if existing.contains(m))
        })
        .collect();

    for method in missing {
        item.items.push(syn::parse_quote! { #method });
    }

    let expanded = quote! {
        #[::nuono_plugin::async_trait]
        #item
    };
    Ok(TokenStream::from(expanded))
}

/// 生成元数据方法（name/version/description）与装饰器收集器方法
/// （triggers/commands/tools，含状态注入）。
fn plugin_impl_methods(
    name: &str,
    version: &str,
    description: &str,
    state_exprs: &[proc_macro2::TokenStream],
) -> (Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>) {
    let meta = vec![
        quote! { fn name(&self) -> &str { #name } },
        quote! { fn version(&self) -> &str { #version } },
        quote! { fn description(&self) -> &str { #description } },
    ];
    let collectors = vec![
        quote! {
            fn triggers(&self) -> Vec<::nuono_plugin::Trigger> {
                #[doc(hidden)]
                let __nuono_states: Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>> =
                    vec![#(#state_exprs),*];
                ::nuono_plugin::__decorators::triggers_with(&__nuono_states, env!("CARGO_PKG_NAME"))
            }
        },
        quote! {
            fn commands(&self) -> Vec<::nuono_plugin::Command> {
                #[doc(hidden)]
                let __nuono_states: Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>> =
                    vec![#(#state_exprs),*];
                ::nuono_plugin::__decorators::commands_with(&__nuono_states, env!("CARGO_PKG_NAME"))
            }
        },
        quote! {
            fn tools(&self) -> Vec<std::sync::Arc<dyn ::nuono_plugin::AgentTool>> {
                ::nuono_plugin::__decorators::tools(env!("CARGO_PKG_NAME"))
            }
        },
    ];
    (meta, collectors)
}

/// 从具名字段结构体生成配置三件套：`get_schema()`（Dashboard 表单）、
/// `default_value()`（从字段声明提取默认值 JSON）、
/// `from_value(&Value)`（运行时配置反序列化，供 `on_load`）。
///
/// 字段第一行 `///` 会成为 Dashboard label，完整文档成为 description，因此修改字段文档
/// 会改变产品界面文案。支持 String、bool、标准整数、f32/f64 和 `Vec<String>`；
/// 其他类型明确报错。`#[config(...)]` 支持默认值与数字范围（default、min、max），
/// 文案与分组（label、description、section、order、unit、step、placeholder），以及
/// required、group_scoped 等标记；标记也可显式写为 true/false。
/// String 可声明 `widget = "text"/"textarea"/"password"/"select"`，bool 可声明
/// `widget = "boolean"/"switch"`；裸 `password` 是密码框简写。
/// `options = ["a", "b"]`、标签和值的元组数组或返回选项的迭代表达式可用于字符串与列表。
/// `visible_when(key = "enabled", value = true)` 按另一字段的值显示控件。
/// `Vec<String>` 支持 `editor = "tags"/"multiselect"`、allow_custom 和
/// `encoding = "native"/"json_string"`；后者在配置存储中使用 JSON 字符串，业务读取仍为 Vec。
/// 默认值按 Rust 类型检查；未知、重复或类型不匹配的属性和不支持的 serde 字段映射明确报错。
///
/// 同时实现 SDK 的 `ConfigSchema` trait，供 `LiveConfig<T>` 自动解析实时快照。
/// `from_value` 要求结构体同时派生 `Deserialize`，对象中的缺省字段使用声明的默认值；
/// 非对象输入和字段类型错误会返回错误。
#[proc_macro_derive(ConfigSchema, attributes(config))]
pub fn derive_config_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::ItemStruct);
    config_derive::expand(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

mod config_derive;

// 触发器属性宏

macro_rules! def_trigger_macro {
    ($name:ident, $type_name:literal) => {
        #[doc = concat!(
            "注册一个 `", $type_name, "` 触发器。\n\n",
            "被标注函数必须是 `async fn(&Bot, &Message, MatchData) -> ::nuono_plugin::Result<()>`。",
            "可选参数为 `pattern`、`priority`、`block`、`to_me`；普通触发器还支持 `prefix`，指令前缀在 service_config 设置。未知参数会产生",
            "编译错误。指令还支持 aliases、description、usage、permission。宏会生成 `<函数名 PascalCase>Handler`，同模块中的同名类型会冲突。"
        )]
        #[proc_macro_attribute]
        pub fn $name(args: TokenStream, input: TokenStream) -> TokenStream {
            let input_fn = parse_macro_input!(input as ItemFn);
            generate_simple_trigger($type_name, args, input_fn)
        }
    };
}

#[doc = "注册指令。简单回复只写 `(&Bot)`；需要事件或参数时写 `(&Bot, &Event)` 或 `(&Bot, MatchData)`。完整签名为 `(&Bot, &Event, MatchData)`；OneBot 指令也可用 `(&Bot, &Message, MatchData)`，还可追加 `state: &T`。返回 Result<()> 或 NuonoResult<()>。可选属性为 pattern、aliases、description、usage、permission、priority、block、to_me。"]
#[proc_macro_attribute]
pub fn nuono_command(args: TokenStream, input: TokenStream) -> TokenStream {
    let parsed_args = if args.is_empty() {
        TriggerArgs::default()
    } else {
        match syn::parse::<TriggerArgs>(args) {
            Ok(a) => a,
            Err(e) => return e.to_compile_error().into(),
        }
    };
    let input_fn = parse_macro_input!(input as ItemFn);
    if takes_onebot_message(&input_fn) {
        generate_trigger_registration("Command", parsed_args, &input_fn)
    } else {
        generate_command_registration(parsed_args, &input_fn)
    }
}

def_trigger_macro!(nuono_prefix, "Prefix");
def_trigger_macro!(nuono_keyword, "Keyword");
def_trigger_macro!(nuono_fullmatch, "FullMatch");
def_trigger_macro!(nuono_suffix, "Suffix");
def_trigger_macro!(nuono_regex, "Regex");
def_trigger_macro!(nuono_file, "File");
def_trigger_macro!(nuono_message, "Message");

/// 注册一个 Agent 工具并提交到插件的 `inventory`。
///
/// 被标注函数必须是
/// `async fn(ToolContext, Input) -> NuonoResult<Output>`。Input 实现 Deserialize + JsonSchema，
/// Output 实现 Serialize。仍支持 AgentToolContext + Value；原始 JSON 可用 `schema = "函数路径"`
/// 显式声明 object JSON Schema。支持 `name`、`description` 和
/// `risk` 字符串；`risk` 可取 `read_only`、`low`、`requires_approval`、`admin_only` 及代码
/// 中列出的兼容别名。宏会生成 `<函数名 PascalCase>Tool`，同模块同名项会冲突。
#[proc_macro_attribute]
pub fn nuono_tool(args: TokenStream, input: TokenStream) -> TokenStream {
    let parsed_args = if args.is_empty() {
        ToolArgs::default()
    } else {
        match syn::parse::<ToolArgs>(args) {
            Ok(a) => a,
            Err(e) => return e.to_compile_error().into(),
        }
    };
    let input_fn = parse_macro_input!(input as ItemFn);
    generate_tool_registration(parsed_args, input_fn)
}

#[cfg(test)]
mod command_args_tests {
    use super::*;
    #[test]
    fn invalid_metadata_is_not_silently_ignored() {
        for invalid in [
            r#"permission = "admin""#,
            r#"aliases = [""]"#,
            r#"description = "a", description = "b""#,
            r#"aliases = "help""#,
            r#"sv = "  ""#,
            r#"display_name = """#,
        ] {
            assert!(syn::parse_str::<TriggerArgs>(invalid).is_err(), "{invalid}");
        }
        let args = syn::parse_str::<TriggerArgs>(r#"aliases = ["天气", "weather"], permission = "group_admin", description = "查询天气", usage = "天气 北京""#).unwrap();
        assert_eq!(args.aliases, ["天气", "weather"]);
        assert_eq!(args.permission.as_deref(), Some("group_admin"));
    }
}

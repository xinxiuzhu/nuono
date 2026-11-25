use proc_macro2::TokenStream;
use quote::quote;
use syn::{Field, Ident};

use super::attributes::{ConfigAttributes, FieldKind, field_doc, field_key};

pub(super) fn expand(
    name: &Ident,
    declarations: &[(&Field, ConfigAttributes, FieldKind)],
) -> TokenStream {
    let schemas = declarations
        .iter()
        .enumerate()
        .map(|(index, (field, attributes, kind))| schema(field, attributes, *kind, index as i32));
    let encoded_keys = declarations
        .iter()
        .filter(|(_, attributes, _)| attributes.json_string_list())
        .map(|(field, _, _)| field_key(field))
        .collect::<Vec<_>>();
    let encoding_hooks = if encoded_keys.is_empty() {
        TokenStream::new()
    } else {
        quote! {
            fn normalize_value(mut value: ::nuono_plugin::Value) -> ::nuono_plugin::Result<::nuono_plugin::Value> {
                ::nuono_plugin::config::normalize_json_string_lists(&mut value, &[#(#encoded_keys),*])?;
                Ok(value)
            }
            fn decode_value(mut value: ::nuono_plugin::Value) -> ::nuono_plugin::Result<Self> {
                ::nuono_plugin::config::decode_json_string_lists(&mut value, &[#(#encoded_keys),*])?;
                ::nuono_plugin::serde_json::from_value(value)
                    .map_err(|_| ::nuono_plugin::anyhow!("配置解析失败：字段类型不匹配"))
            }
        }
    };
    quote! {
        impl ::nuono_plugin::ConfigSchema for #name {
            fn get_schema() -> Vec<::nuono_plugin::PluginConfigField> { vec![#(#schemas),*] }
            #encoding_hooks
        }
        impl #name {
            pub fn get_schema() -> Vec<::nuono_plugin::PluginConfigField> { <Self as ::nuono_plugin::ConfigSchema>::get_schema() }
            pub fn default_value() -> ::nuono_plugin::Value { <Self as ::nuono_plugin::ConfigSchema>::default_value() }
            pub fn from_value(value: &::nuono_plugin::Value) -> ::nuono_plugin::Result<Self> { <Self as ::nuono_plugin::ConfigSchema>::from_value(value) }
        }
    }
}

fn schema(
    field: &Field,
    attributes: &ConfigAttributes,
    kind: FieldKind,
    declaration_order: i32,
) -> TokenStream {
    let key = field_key(field);
    let doc = field_doc(field);
    let label = attributes
        .label
        .as_ref()
        .map(|value| value.value())
        .unwrap_or_else(|| doc.lines().next().unwrap_or(&key).to_string());
    let description = attributes
        .description
        .as_ref()
        .map(|value| value.value())
        .unwrap_or(doc);
    let ty = &field.ty;
    let value = attributes
        .default
        .as_ref()
        .map(|value| quote!(#value))
        .unwrap_or_else(|| match kind {
            FieldKind::String => quote!(""),
            FieldKind::Boolean => quote!(false),
            FieldKind::Float => quote!(0.0),
            FieldKind::Integer => quote!(0),
            FieldKind::StringList => quote!(Vec::new()),
        });
    let options = attributes.options.as_ref().map(|value| {
        quote! {
            ::std::iter::IntoIterator::into_iter(#value)
                .map(::std::convert::Into::<::nuono_plugin::ConfigFieldOption>::into)
                .collect::<Vec<::nuono_plugin::ConfigFieldOption>>()
        }
    });
    let make = match kind {
        FieldKind::String => {
            let constructor = match attributes
                .widget
                .as_ref()
                .map(|widget| widget.value())
                .as_deref()
            {
                Some("textarea") => {
                    quote!(::nuono_plugin::PluginConfigField::textarea(#key, #label, #description, &value))
                }
                Some("password") => {
                    quote!(::nuono_plugin::PluginConfigField::password(#key, #label, #description).with_default(value))
                }
                Some("select") => {
                    let options = options.as_ref().unwrap();
                    quote!(::nuono_plugin::PluginConfigField::select(#key, #label, #description, &value, #options))
                }
                _ => {
                    quote!(::nuono_plugin::PluginConfigField::text(#key, #label, #description, &value))
                }
            };
            quote!({ let value: String = ::std::convert::Into::into(#value); #constructor })
        }
        FieldKind::Boolean => {
            if attributes
                .widget
                .as_ref()
                .is_some_and(|widget| widget.value() == "switch")
            {
                quote!(::nuono_plugin::PluginConfigField::switch(#key, #label, #description, #value))
            } else {
                quote!(::nuono_plugin::PluginConfigField::boolean(#key, #label, #description, #value))
            }
        }
        FieldKind::Float => {
            quote!({ let value: #ty = #value; ::nuono_plugin::PluginConfigField::decimal(#key, #label, #description, value as f64) })
        }
        FieldKind::Integer => {
            quote!({ let value: #ty = #value; ::nuono_plugin::PluginConfigField::number(#key, #label, #description,
            ::std::convert::TryInto::<i64>::try_into(value).expect("配置默认值超出 TOML 整数范围"))
                .with_min(i64::try_from(<#ty>::MIN).unwrap_or(i64::MIN))
                .with_max(i64::try_from(<#ty>::MAX).unwrap_or(i64::MAX)) })
        }
        FieldKind::StringList if attributes.json_string_list() => quote!({
            let value: #ty = #value;
            let encoded = ::nuono_plugin::serde_json::to_string(&value)
                .expect("字符串列表默认值无法编码");
            ::nuono_plugin::PluginConfigField::json_string_list(#key, #label, #description, &encoded)
        }),
        FieldKind::StringList => {
            quote!({ let value: #ty = #value; ::nuono_plugin::PluginConfigField::string_list(#key, #label, #description, value) })
        }
    };
    let min = attributes.min.as_ref().map(|value| quote! { let bound: i64 = #value; field.min = Some(field.min.map_or(bound, |current| current.max(bound))); });
    let max = attributes.max.as_ref().map(|value| quote! { let bound: i64 = #value; field.max = Some(field.max.map_or(bound, |current| current.min(bound))); });
    let section = attributes
        .section
        .as_ref()
        .map(|value| quote!(field = field.with_section(#value);));
    // 表单默认沿用源码声明顺序；显式 order 继续作为排序覆盖。
    let order = attributes
        .order
        .as_ref()
        .map(|value| quote!(#value))
        .unwrap_or_else(|| quote!(#declaration_order));
    let unit = attributes
        .unit
        .as_ref()
        .map(|value| quote!(field = field.with_unit(#value);));
    let step = attributes
        .step
        .as_ref()
        .map(|value| quote!(field = field.with_step(#value);));
    let placeholder = attributes
        .placeholder
        .as_ref()
        .map(|value| quote!(field = field.with_placeholder(#value);));
    let required = attributes
        .required
        .as_ref()
        .map(|value| quote!(field.required = #value; field.ui.required = #value;));
    let group_scoped = attributes
        .group_scoped
        .as_ref()
        .map(|value| quote!(field.group_scoped = #value;));
    let user_scoped = attributes
        .user_scoped
        .as_ref()
        .map(|value| quote!(field.user_scoped = #value;));
    let allow_custom = attributes
        .allow_custom
        .as_ref()
        .map(|value| quote!(field = field.with_allow_custom_options(#value);));
    let editor = attributes
        .editor
        .as_ref()
        .map(|value| quote!(field = field.with_editor(#value);));
    let encoding = attributes
        .encoding
        .as_ref()
        .map(|value| quote!(field = field.with_encoding(#value);));
    // Select 的构造器保留原有顶层 options 表示，其他类型使用同步 UI 的 builder。
    let options = if attributes
        .widget
        .as_ref()
        .is_some_and(|value| value.value() == "select")
    {
        None
    } else {
        options.map(|value| quote!(field = field.with_options(#value);))
    };
    let visible_when = attributes.visible_when.as_ref().map(|condition| {
        let key = &condition.key;
        let value = &condition.value;
        quote!(field = field.with_visible_when(#key, #value);)
    });
    let customization = quote! {
        #min #max #section
        field = field.with_order(#order);
        #unit #step #placeholder #required #group_scoped #user_scoped
        #allow_custom #options #editor #encoding #visible_when
    };
    quote!({ let mut field = #make; #customization field })
}

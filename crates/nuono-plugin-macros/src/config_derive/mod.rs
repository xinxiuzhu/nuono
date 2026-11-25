use proc_macro2::TokenStream;
use syn::{ItemStruct, spanned::Spanned};

mod attributes;
mod generate;

#[cfg(test)]
mod tests;

use attributes::{ConfigAttributes, FieldKind};

pub(super) fn expand(input: &ItemStruct) -> syn::Result<TokenStream> {
    let name = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "ConfigSchema 不支持泛型；请手写 SDK 字段声明",
        ));
    }
    let syn::Fields::Named(fields) = &input.fields else {
        return Err(syn::Error::new(name.span(), "ConfigSchema 仅支持具名字段"));
    };
    // 会改变配置键名、字段存在性或默认语义的 serde 属性不能悄悄忽略。
    for attr in &input.attrs {
        if attr.path().is_ident("config") {
            return Err(syn::Error::new(
                attr.span(),
                "ConfigSchema 的 config 属性应声明在字段上，不能用于结构体",
            ));
        }
        if attr.path().is_ident("serde") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("crate") {
                    let _: syn::LitStr = meta.value()?.parse()?;
                    Ok(())
                } else if meta.path.is_ident("default") && !meta.input.peek(syn::Token![=]) {
                    Ok(())
                } else {
                    Err(meta.error(
                        "ConfigSchema 不支持此 serde 属性；请使用 config 属性或手写配置声明",
                    ))
                }
            })?;
        }
    }

    let declarations = fields
        .named
        .iter()
        .map(|field| {
            let attributes = ConfigAttributes::parse(field)?;
            let kind = FieldKind::parse(&field.ty)?;
            attributes.validate(kind, &field.ty)?;
            Ok((field, attributes, kind))
        })
        .collect::<syn::Result<Vec<_>>>()?;

    for (_, attributes, _) in &declarations {
        if let Some(condition) = &attributes.visible_when {
            let target = declarations
                .iter()
                .find(|(field, _, _)| attributes::field_key(field) == condition.key.value())
                .ok_or_else(|| syn::Error::new(condition.key.span(), "显示条件引用了未声明字段"))?;
            condition.validate_literal(target.2, &target.1)?;
        }
    }

    Ok(generate::expand(name, &declarations))
}

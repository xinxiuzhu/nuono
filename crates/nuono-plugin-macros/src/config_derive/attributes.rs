use std::collections::HashSet;

use syn::{Expr, Field, LitBool, LitStr, Type, meta::ParseNestedMeta, spanned::Spanned};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FieldKind {
    String,
    Boolean,
    Integer,
    Float,
    StringList,
}

impl FieldKind {
    pub(super) fn parse(ty: &Type) -> syn::Result<Self> {
        let Type::Path(path) = ty else {
            return Err(syn::Error::new(
                ty.span(),
                "不支持的配置类型；请手写 SDK 字段声明",
            ));
        };
        let segment = path.path.segments.last().unwrap();
        match segment.ident.to_string().as_str() {
            "String" => Ok(Self::String),
            "bool" => Ok(Self::Boolean),
            "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize" => {
                Ok(Self::Integer)
            }
            "f32" | "f64" => Ok(Self::Float),
            "Vec" => {
                let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                    return Err(syn::Error::new(ty.span(), "仅支持 Vec<String>"));
                };
                if args.args.len() != 1
                    || !matches!(args.args.first(), Some(syn::GenericArgument::Type(Type::Path(p))) if p.path.segments.last().is_some_and(|s| s.ident == "String"))
                {
                    return Err(syn::Error::new(
                        ty.span(),
                        "ConfigSchema 仅支持 Vec<String>；其他容器请显式声明编码与组件",
                    ));
                }
                Ok(Self::StringList)
            }
            _ => Err(syn::Error::new(
                ty.span(),
                "ConfigSchema 不支持此类型；请显式使用 SDK 声明，不能自动推断为文本",
            )),
        }
    }

    fn numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Float)
    }
}

#[derive(Default)]
pub(super) struct ConfigAttributes {
    pub(super) default: Option<Expr>,
    pub(super) min: Option<Expr>,
    pub(super) max: Option<Expr>,
    pub(super) label: Option<LitStr>,
    pub(super) description: Option<LitStr>,
    pub(super) section: Option<LitStr>,
    pub(super) order: Option<Expr>,
    pub(super) unit: Option<LitStr>,
    pub(super) step: Option<Expr>,
    pub(super) placeholder: Option<LitStr>,
    pub(super) required: Option<LitBool>,
    pub(super) group_scoped: Option<LitBool>,
    pub(super) user_scoped: Option<LitBool>,
    pub(super) allow_custom: Option<LitBool>,
    pub(super) widget: Option<LitStr>,
    pub(super) options: Option<Expr>,
    pub(super) editor: Option<LitStr>,
    pub(super) encoding: Option<LitStr>,
    pub(super) visible_when: Option<Visibility>,
}

impl ConfigAttributes {
    pub(super) fn parse(field: &Field) -> syn::Result<Self> {
        let mut result = Self::default();
        let mut seen = HashSet::new();
        for attr in &field.attrs {
            if attr.path().is_ident("serde") {
                return Err(syn::Error::new(
                    attr.span(),
                    "ConfigSchema 字段不支持 serde 重命名、跳过或独立默认值；请使用 config 属性或手写配置声明",
                ));
            }
            if !attr.path().is_ident("config") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                let name = meta
                    .path
                    .get_ident()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                // password 是 widget 的简写，混用时不能静默覆盖。
                let unique_name = if name == "password" { "widget" } else { &name };
                if !seen.insert(unique_name.to_string()) {
                    return Err(meta.error("重复的 config 属性"));
                }
                match name.as_str() {
                    "default" => result.default = Some(meta.value()?.parse()?),
                    "min" => result.min = Some(meta.value()?.parse()?),
                    "max" => result.max = Some(meta.value()?.parse()?),
                    "label" => result.label = Some(meta.value()?.parse()?),
                    "description" => result.description = Some(meta.value()?.parse()?),
                    "section" => result.section = Some(meta.value()?.parse()?),
                    "order" => result.order = Some(meta.value()?.parse()?),
                    "unit" => result.unit = Some(meta.value()?.parse()?),
                    "step" => result.step = Some(meta.value()?.parse()?),
                    "placeholder" => result.placeholder = Some(meta.value()?.parse()?),
                    "required" => result.required = Some(parse_flag(&meta)?),
                    "group_scoped" => result.group_scoped = Some(parse_flag(&meta)?),
                    "user_scoped" => result.user_scoped = Some(parse_flag(&meta)?),
                    "allow_custom" => result.allow_custom = Some(parse_flag(&meta)?),
                    "widget" => result.widget = Some(meta.value()?.parse()?),
                    "password" => {
                        if meta.input.peek(syn::Token![=]) || meta.input.peek(syn::token::Paren) {
                            return Err(meta.error("password 简写不接受值；请使用 widget = \"password\""));
                        }
                        result.widget = Some(LitStr::new("password", meta.path.span()));
                    }
                    "options" => result.options = Some(meta.value()?.parse()?),
                    "editor" => result.editor = Some(meta.value()?.parse()?),
                    "encoding" => result.encoding = Some(meta.value()?.parse()?),
                    "visible_when" => result.visible_when = Some(Visibility::parse(&meta)?),
                    _ => {
                        return Err(meta.error(
                            "未知 config 属性；支持 default、min、max、label、description、section、order、unit、step、placeholder、required、group_scoped、allow_custom、widget、password、options、editor、encoding、visible_when",
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(result)
    }

    pub(super) fn validate(&self, kind: FieldKind, ty: &Type) -> syn::Result<()> {
        if !kind.numeric() && (self.min.is_some() || self.max.is_some()) {
            return Err(syn::Error::new(ty.span(), "min/max 仅用于数字字段"));
        }
        for (name, span) in [
            ("unit", self.unit.as_ref().map(Spanned::span)),
            ("step", self.step.as_ref().map(Spanned::span)),
        ] {
            if !kind.numeric()
                && let Some(span) = span
            {
                return Err(syn::Error::new(span, format!("{name} 仅用于数字字段")));
            }
        }
        if let Some(widget) = &self.widget {
            let supported = match kind {
                FieldKind::String => matches!(
                    widget.value().as_str(),
                    "text" | "textarea" | "password" | "select"
                ),
                FieldKind::Boolean => matches!(widget.value().as_str(), "boolean" | "switch"),
                _ => false,
            };
            if !supported {
                return Err(syn::Error::new(
                    widget.span(),
                    "widget 与字段类型不匹配；String 支持 text/textarea/password/select，bool 支持 boolean/switch",
                ));
            }
            if widget.value() == "select" && self.options.is_none() {
                return Err(syn::Error::new(widget.span(), "select 必须声明 options"));
            }
        }
        if let Some(options) = &self.options
            && !matches!(kind, FieldKind::String | FieldKind::StringList)
        {
            return Err(syn::Error::new(
                options.span(),
                "options 仅用于字符串或 Vec<String> 字段",
            ));
        }
        if let Some(editor) = &self.editor {
            if kind != FieldKind::StringList {
                return Err(syn::Error::new(
                    editor.span(),
                    "editor 仅用于 Vec<String> 字段",
                ));
            }
            if !matches!(editor.value().as_str(), "tags" | "multiselect") {
                return Err(syn::Error::new(
                    editor.span(),
                    "Vec<String> 的 editor 仅支持 tags/multiselect",
                ));
            }
        }
        if let Some(encoding) = &self.encoding {
            if kind != FieldKind::StringList {
                return Err(syn::Error::new(
                    encoding.span(),
                    "encoding 仅用于 Vec<String> 字段",
                ));
            }
            if !matches!(encoding.value().as_str(), "native" | "json_string") {
                return Err(syn::Error::new(
                    encoding.span(),
                    "Vec<String> 的 encoding 仅支持 native/json_string",
                ));
            }
        }
        if let Some(allow_custom) = &self.allow_custom
            && kind != FieldKind::StringList
        {
            return Err(syn::Error::new(
                allow_custom.span(),
                "allow_custom 仅用于 Vec<String> 字段",
            ));
        }
        Ok(())
    }

    pub(super) fn json_string_list(&self) -> bool {
        self.encoding
            .as_ref()
            .is_some_and(|value| value.value() == "json_string")
    }
}

fn parse_flag(meta: &ParseNestedMeta<'_>) -> syn::Result<LitBool> {
    if meta.input.peek(syn::Token![=]) {
        meta.value()?.parse()
    } else {
        Ok(LitBool::new(true, meta.path.span()))
    }
}

pub(super) struct Visibility {
    pub(super) key: LitStr,
    pub(super) value: Expr,
}

impl Visibility {
    fn parse(meta: &ParseNestedMeta<'_>) -> syn::Result<Self> {
        let mut key = None;
        let mut value = None;
        let mut seen = HashSet::new();
        meta.parse_nested_meta(|nested| {
            let name = nested
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            if !seen.insert(name.clone()) {
                return Err(nested.error("重复的 visible_when 属性"));
            }
            match name.as_str() {
                "key" => key = Some(nested.value()?.parse()?),
                "value" => value = Some(nested.value()?.parse()?),
                _ => return Err(nested.error("visible_when 仅支持 key 和 value")),
            }
            Ok(())
        })?;
        Ok(Self {
            key: key.ok_or_else(|| meta.error("visible_when 缺少 key"))?,
            value: value.ok_or_else(|| meta.error("visible_when 缺少 value"))?,
        })
    }

    pub(super) fn validate_literal(
        &self,
        target: FieldKind,
        attributes: &ConfigAttributes,
    ) -> syn::Result<()> {
        let Expr::Lit(value) = &self.value else {
            // Rust 表达式在生成代码中检查；具体动态值由 SDK 字段校验检查。
            return Ok(());
        };
        let valid = match &value.lit {
            syn::Lit::Str(_) => target == FieldKind::String || attributes.json_string_list(),
            syn::Lit::Bool(_) => target == FieldKind::Boolean,
            syn::Lit::Int(_) => matches!(target, FieldKind::Integer | FieldKind::Float),
            syn::Lit::Float(_) => target == FieldKind::Float,
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(syn::Error::new(
                value.span(),
                "visible_when 的 value 与目标字段类型不匹配",
            ))
        }
    }
}

pub(super) fn field_key(field: &Field) -> String {
    field
        .ident
        .as_ref()
        .unwrap()
        .to_string()
        .trim_start_matches("r#")
        .to_string()
}

pub(super) fn field_doc(field: &Field) -> String {
    field
        .attrs
        .iter()
        .filter_map(|attr| {
            if attr.path().is_ident("doc")
                && let syn::Meta::NameValue(nv) = &attr.meta
                && let Expr::Lit(lit) = &nv.value
                && let syn::Lit::Str(value) = &lit.lit
            {
                return Some(value.value().trim().to_owned());
            }
            None
        })
        .collect::<Vec<_>>()
        .join("\n")
}

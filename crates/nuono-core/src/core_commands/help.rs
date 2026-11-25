//! 系统帮助指令渲染：展示当前生效的系统核心指令，以及已启用插件与单插件指令速查。

use nuono_app::localization::Locale;

use super::{CoreCommandPermission, CoreCommands, localized, resolve_commands};
use crate::config::CoreCommandsConfig;

const MAX_PLUGIN_HELP_COMMANDS: usize = 20;

pub(super) async fn render(
    commands: &CoreCommands,
    config: &CoreCommandsConfig,
    locale: Locale,
    query: &str,
) -> String {
    let query = query.trim();
    if !query.is_empty() {
        return render_plugin_detail(commands, config, locale, query).await;
    }
    let prefix = config.effective_prefix();
    let mut lines = vec![localized(
        locale,
        "系统核心指令",
        "System Core Commands",
        "システムコアコマンド",
        "시스템 핵심 명령",
    )];
    for item in resolve_commands(config) {
        let permission = permission_suffix(item.permission, locale);
        let state = if item.enabled {
            String::new()
        } else {
            localized(
                locale,
                "［已禁用］",
                " [Disabled]",
                " [無効]",
                " [비활성화]",
            )
        };
        let alias_note = if item.aliases.is_empty() {
            String::new()
        } else {
            let joined = item.aliases.join(" / ");
            localized(
                locale,
                &format!("（快捷别名：{joined}）"),
                &format!(" (aliases: {joined})"),
                &format!("（別名：{joined}）"),
                &format!(" (별칭: {joined})"),
            )
        };
        let description = localized_description(item.definition.key, locale);
        lines.push(format!(
            "{}：{description}{permission}{alias_note}{state}",
            item.usage
        ));
    }

    if let Some(source) = &commands.status_source {
        let plugin_lines = enabled_plugin_summary(&source.host, locale).await;
        if !plugin_lines.is_empty() {
            lines.push(String::new());
            lines.push(localized(
                locale,
                "已启用插件",
                "Enabled Plugins",
                "有効なプラグイン",
                "활성화된 플러그인",
            ));
            lines.extend(plugin_lines);
            lines.push(localized(
                locale,
                &format!("发送「{prefix} 帮助 <插件名>」可查看该插件的详细指令。"),
                &format!("Send \"{prefix} help <plugin>\" to view commands for a plugin."),
                &format!(
                    "「{prefix} help <プラグイン名>」でプラグイン別のコマンド一覧を確認できます。"
                ),
                &format!(
                    "\"{prefix} help <플러그인명>\"으로 플러그인별 명령을 확인할 수 있습니다."
                ),
            ));
        }
    }

    lines.join("\n")
}

async fn render_plugin_detail(
    commands: &CoreCommands,
    config: &CoreCommandsConfig,
    locale: Locale,
    query: &str,
) -> String {
    let prefix = config.effective_prefix();
    let Some(source) = &commands.status_source else {
        return localized(
            locale,
            "当前运行模式未挂载插件目录。",
            "No plugin catalog is attached to this runtime.",
            "現在の実行環境にはプラグイン一覧が接続されていません。",
            "현재 런타임에는 플러그인 목록이 연결되어 있지 않습니다.",
        );
    };

    let target = query.to_lowercase();
    let configs = source.host.list_plugin_configs().await;
    let mut matched = None;
    for plugin in configs {
        if !plugin.enabled {
            continue;
        }
        let info = source.host.get_plugin_info(&plugin.name).await;
        if plugin.name.to_lowercase() == target || info.display_name.to_lowercase() == target {
            matched = Some((plugin, info));
            break;
        }
    }

    let Some((plugin, info)) = matched else {
        return localized(
            locale,
            &format!("未找到已启用的插件「{query}」，请发送「{prefix} 帮助」查看插件列表。"),
            &format!(
                "Enabled plugin \"{query}\" was not found. Send \"{prefix} help\" to list available plugins."
            ),
            &format!(
                "有効なプラグイン「{query}」が見つかりません。「{prefix} help」で一覧を確認してください。"
            ),
            &format!(
                "활성화된 플러그인 \"{query}\"을(를) 찾을 수 없습니다. \"{prefix} help\"로 목록을 확인하세요."
            ),
        );
    };

    let title = if info.display_name.is_empty() || info.display_name == plugin.name {
        plugin.name.clone()
    } else {
        format!("{} ({})", info.display_name, plugin.name)
    };
    let mut lines = vec![title];
    if !info.description.trim().is_empty() {
        lines.push(info.description.trim().to_owned());
    }
    let prefixes: Vec<&str> = plugin
        .prefix
        .iter()
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .collect();
    if !prefixes.is_empty() {
        let joined = prefixes.join(" / ");
        lines.push(localized(
            locale,
            &format!("触发前缀：{joined}"),
            &format!("Prefix: {joined}"),
            &format!("プレフィックス：{joined}"),
            &format!("접두사: {joined}"),
        ));
    }

    let mut plugin_commands: Vec<_> = source
        .host
        .trigger_registry()
        .get_all_commands()
        .into_iter()
        .filter(|cmd| cmd.enabled && cmd.plugin_name.as_deref() == Some(plugin.name.as_str()))
        .collect();
    plugin_commands.sort_by(|a, b| a.name.cmp(&b.name));

    if plugin_commands.is_empty() {
        lines.push(localized(
            locale,
            "该插件暂未声明可展示的已启用指令。",
            "This plugin has no enabled commands listed.",
            "このプラグインには表示可能な有効コマンドがありません。",
            "이 플러그인에 표시할 활성 명령이 없습니다.",
        ));
    } else {
        let total = plugin_commands.len();
        for command in plugin_commands.into_iter().take(MAX_PLUGIN_HELP_COMMANDS) {
            let entry = if !command.usage.trim().is_empty() {
                command.usage.trim().to_owned()
            } else {
                command.name.clone()
            };
            if command.description.trim().is_empty() {
                lines.push(format!("· {entry}"));
            } else {
                lines.push(format!("· {entry} — {}", command.description.trim()));
            }
        }
        if total > MAX_PLUGIN_HELP_COMMANDS {
            let remaining = total - MAX_PLUGIN_HELP_COMMANDS;
            lines.push(localized(
                locale,
                &format!("……另有 {remaining} 条指令，可在控制台「指令中心」查看完整列表。"),
                &format!("...and {remaining} more commands. See the web Command Center for the full list."),
                &format!("……他 {remaining} 件のコマンドは管理画面のコマンドセンターで確認できます。"),
                &format!("...외 {remaining}개 명령은 웹 명령 센터에서 확인할 수 있습니다."),
            ));
        }
    }

    lines.join("\n")
}

async fn enabled_plugin_summary(
    host: &crate::core::plugin_host::PluginHost,
    locale: Locale,
) -> Vec<String> {
    let mut configs = host.list_plugin_configs().await;
    configs.retain(|plugin| plugin.enabled);
    configs.sort_by(|a, b| a.name.cmp(&b.name));
    if configs.is_empty() {
        return Vec::new();
    }
    let all_commands = host.trigger_registry().get_all_commands();
    let mut lines = Vec::with_capacity(configs.len());
    for plugin in configs {
        let info = host.get_plugin_info(&plugin.name).await;
        let label = if info.display_name.is_empty() || info.display_name == plugin.name {
            plugin.name.clone()
        } else {
            format!("{} ({})", info.display_name, plugin.name)
        };
        let count = all_commands
            .iter()
            .filter(|cmd| cmd.enabled && cmd.plugin_name.as_deref() == Some(plugin.name.as_str()))
            .count();
        let prefixes: Vec<&str> = plugin
            .prefix
            .iter()
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .collect();
        let prefix_part = if prefixes.is_empty() {
            String::new()
        } else {
            let joined = prefixes.join("/");
            localized(
                locale,
                &format!(" · 前缀 {joined}"),
                &format!(" · prefix {joined}"),
                &format!(" · 接頭辞 {joined}"),
                &format!(" · 접두사 {joined}"),
            )
        };
        let count_part = localized(
            locale,
            &format!("{count} 条指令"),
            &format!("{count} commands"),
            &format!("{count} 件"),
            &format!("{count}개 명령"),
        );
        lines.push(format!("· {label}（{count_part}{prefix_part}）"));
    }
    lines
}

fn permission_suffix(permission: CoreCommandPermission, locale: Locale) -> String {
    match permission {
        CoreCommandPermission::Everyone => String::new(),
        CoreCommandPermission::GroupAdmin => localized(
            locale,
            "（群/频道管理员）",
            " (Group/Channel Admin)",
            "（グループ/チャンネル管理者）",
            " (그룹/채널 관리자)",
        ),
        CoreCommandPermission::BotAdmin => localized(
            locale,
            "（机器人管理员）",
            " (Bot Admin)",
            "（ボット管理者）",
            " (봇 관리자)",
        ),
    }
}

fn localized_description(key: &str, locale: Locale) -> String {
    match key {
        "help" => localized(
            locale,
            "查看系统核心指令与已启用插件概览",
            "Show system core commands and enabled plugins",
            "システムコアコマンドと有効なプラグイン一覧を表示",
            "시스템 핵심 명령과 활성화된 플러그인 목록 보기",
        ),
        "status" => localized(
            locale,
            "查看 Nuono 图片状态卡与插件公开统计",
            "Show Nuono status card and public plugin stats",
            "Nuono のステータスカードと公開プラグイン統計を表示",
            "Nuono 상태 카드 및 공개 플러그인 통계 보기",
        ),
        "restart" => localized(
            locale,
            "安全重启 Nuono 服务进程",
            "Safely restart the Nuono service",
            "Nuono サービスを安全に再起動",
            "Nuono 서비스를 안전하게 재시작",
        ),
        _ => String::new(),
    }
}

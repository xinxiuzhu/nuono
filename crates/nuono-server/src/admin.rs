//! 本机管理员密码应急重置命令（不启动 HTTP 服务、平台连接或插件编译，也不依赖邮件服务）。
use anyhow::{Context, Result, bail, ensure};
use nuono_core::{Database, config::NuonoConfig};
use std::{io::IsTerminal, path::Path};

fn check_config_file(path: &Path) -> Result<String> {
    ensure!(
        path.is_file(),
        "找不到配置文件 {}。请进入原部署目录，或通过 --config 指定已有配置；救援命令不会创建新配置。",
        path.display()
    );
    let content = std::fs::read_to_string(path).context("无法读取已有配置文件")?;
    ensure!(
        !content.trim().is_empty(),
        "配置文件为空，请先恢复原配置；救援命令不会使用默认数据库"
    );
    Ok(content)
}

pub fn reset_password(config_path: &Path, username: &str, disable_two_factor: bool) -> Result<()> {
    let config_text = check_config_file(config_path)?;
    ensure!(!username.trim().is_empty(), "必须明确指定要恢复的用户名");
    ensure!(
        std::io::stdin().is_terminal(),
        "请在交互式终端运行，密码输入不会回显；不接受管道传入密码"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("无法初始化数据库运行环境")?;
    runtime.block_on(async {
        // 直接读取刚刚验证过的内容，救援路径绝不生成或改写配置文件。
        let config: NuonoConfig = toml::from_str(&config_text)
            .map_err(|_| anyhow::anyhow!("无法解析已有配置，请检查 TOML 格式；未修改账号"))?;
        config
            .validate_bootstrap()
            .map_err(|_| anyhow::anyhow!("启动配置无效，请检查数据库与启动参数；未修改账号"))?;
        if config.database.url.starts_with("sqlite:") {
            let file = nuono_core::db::sqlite_database_path(&config.database.url)
                .map_err(|_| anyhow::anyhow!("SQLite 地址无效"))?
                .context("救援需要已有的 SQLite 文件库，不能使用内存数据库")?;
            ensure!(
                file.is_file(),
                "SQLite 数据库文件不存在，请确认工作目录与原数据库路径；不会创建空库"
            );
        }
        let data = nuono_core::core::Paths::root().data;
        ensure!(
            !data.join(crate::backup::RESTORE_MARKER).exists()
                && !crate::backup::transfer::is_pending(&data),
            "当前有待恢复的数据，请先完成或取消恢复，再重置管理员密码"
        );
        let db = Database::from_config(&config.database).await.map_err(|_| {
            anyhow::anyhow!("无法连接现有数据库，请检查数据库服务、连接配置与文件权限；未修改账号")
        })?;
        let result = recover_interactive(&db, username, disable_two_factor).await;
        let close = db.shutdown().await;
        result?;
        close.context("账号已恢复，但关闭数据库连接时出错")?;
        println!("账号 {username} 的密码已重置，旧会话和未使用的账号令牌已失效。");
        if disable_two_factor {
            println!("两步验证已关闭；登录后可以重新设置。");
        } else {
            println!("两步验证设置保持不变。");
        }
        Ok(())
    })
}

async fn recover_interactive(
    db: &Database,
    username: &str,
    disable_two_factor: bool,
) -> Result<()> {
    let user = db
        .find_user_by_username(username)
        .await?
        .context("找不到指定账号，请确认用户名")?;
    ensure!(
        matches!(user.role.as_str(), "owner" | "admin") && user.status == "active",
        "本机救援只用于已启用的 owner 或 admin 账号"
    );
    println!("恢复账号：{}（{}）", user.username, user.role);
    if disable_two_factor {
        println!("此次操作还会移除两步验证、恢复码和未完成的两步验证设置。");
    }
    let password = rpassword::prompt_password("新密码（输入不显示）：").context("读取密码失败")?;
    nuono_app::auth::validate_password(&password).map_err(anyhow::Error::msg)?;
    let confirm = rpassword::prompt_password("再次输入新密码：").context("读取确认密码失败")?;
    if password != confirm {
        bail!("两次密码不一致，未修改账号");
    }
    db.recover_admin_credentials(username, &password, disable_two_factor)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_empty_config_does_not_create_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nuono.toml");
        assert!(check_config_file(&path).is_err());
        assert!(!path.exists());
        std::fs::write(&path, " \n").unwrap();
        assert!(check_config_file(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), " \n");
    }
}

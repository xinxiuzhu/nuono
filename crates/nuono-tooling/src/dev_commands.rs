use crate::{find_nuono_repository_root, plugin_scaffold};
pub fn run_new_plugin_command(name: &str) -> i32 {
    let current_dir = match std::env::current_dir() {
        Ok(current_dir) => current_dir,
        Err(error) => {
            eprintln!("无法确定当前目录: {error}");
            return 1;
        }
    };
    let Some(repository_root) = find_nuono_repository_root(&current_dir) else {
        eprintln!("未找到 Nuono 源码仓库根目录；请在 Nuono 源码仓库内运行此命令");
        eprintln!("用法: nuono plugin new <name>");
        return 1;
    };

    let generated = match plugin_scaffold::create_plugin(&repository_root, name) {
        Ok(generated) => generated,
        Err(error) => {
            eprintln!("生成插件脚手架失败: {error}");
            eprintln!("用法: nuono plugin new <name>");
            eprintln!("示例: nuono plugin new my-plugin");
            return 1;
        }
    };

    println!("已生成插件脚手架: {}", generated.root.display());
    println!(
        "Rust 库名为 {}；cargo run 会将已安装插件与框架统一编译",
        generated.library_stem
    );
    println!("上手说明: {}/README.md", generated.root.display());
    println!("下一步（在仓库根目录）：");
    for command in ["doctor", "check", "test", "build"] {
        println!("  cargo plugin {command} {}", generated.root.display());
    }
    println!("  cargo run # 启动宿主后启用插件并发送 ping");
    0
}

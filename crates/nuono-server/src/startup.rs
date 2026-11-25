//! 启动横幅与 HTTP socket 成功绑定后的地址报告；管理可用和业务运行另行报告。
use anyhow::Context;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Instant,
};
use tokio::net::TcpListener;
use tracing::info;

const STARTUP_LOGO: &str = "                        ▄▄▄██████████▄▄
                     ▄███████████████████▄▄
                   ▄██████▀▀       ▀▀▀██████▄
                 ▄█████▀               ▀██████
               ▄██████▀                  ██████▄▄▄▄▄
        ▄▄▄▄ ▀████████                   ████████▄▄▄ ▀▀▀▀████▄▄
 ▄▄▄██▀▀   ▄██████████                   ██████████▀     ▄█████▀
▀█████▄     ▀▀▀▀██████▄                 ▄██▀▀▀▀   ▄▄▄▄▄█▀▀▀▀▀
   ▀▀▀▀▀▀██▄▄▄▄▄▄▄▄▄▄▄▄▄▄             ▄▄▄▄▄██▀▀▀▀
                  ▀███████▄▄▄     ▄▄▄██████▀
                    ▀▀███████████████████▀
                        ▀▀██████████▀▀▀";

/// 与确认的字符画比例一致，右侧文字再留 4 列空白。
const STARTUP_LOGO_WIDTH: usize = 64;
const STARTUP_TEXT_COLUMN: usize = STARTUP_LOGO_WIDTH + 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BannerStyle {
    Plain,
    Ansi,
    Grayscale,
}

/// 交互式启动页首标识，直接写控制台以保留字符画布局。
pub(super) fn print_banner() {
    use std::io::IsTerminal;

    let stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    let style = if std::env::var_os("NO_COLOR").is_some() || term == "dumb" {
        BannerStyle::Plain
    } else if term.contains("256color")
        || matches!(
            std::env::var("COLORTERM").as_deref(),
            Ok("truecolor" | "24bit")
        )
    {
        BannerStyle::Grayscale
    } else {
        BannerStyle::Ansi
    };
    // 装饰性输出失败不影响服务启动。
    let _ = write_banner(&mut stdout.lock(), style);
}

fn write_banner(writer: &mut impl std::io::Write, style: BannerStyle) -> std::io::Result<()> {
    let (bold, dim, reset) = match style {
        BannerStyle::Plain => ("", "", ""),
        _ => ("\x1b[1m", "\x1b[2m", "\x1b[0m"),
    };
    writeln!(writer)?;
    for (row, line) in STARTUP_LOGO.lines().enumerate() {
        write_logo_line(writer, line, row, style)?;
        let (caption, style) = match row {
            3 => ("Nuono", bold),
            4 => (concat!("v", env!("CARGO_PKG_VERSION")), dim),
            6 => ("我们曾仰望星空，", dim),
            7 => ("思索自己在群星中的位置。", dim),
            _ => {
                writeln!(writer)?;
                continue;
            }
        };
        // 颜色控制序列不占显示列，间距仍按原始的单列字符计算。
        let padding = STARTUP_TEXT_COLUMN.saturating_sub(line.chars().count());
        writeln!(writer, "{:padding$}{style}{caption}{reset}", "")?;
    }
    writeln!(writer)
}

fn write_logo_line(
    writer: &mut impl std::io::Write,
    line: &str,
    row: usize,
    style: BannerStyle,
) -> std::io::Result<()> {
    if style != BannerStyle::Grayscale {
        return write!(writer, "{line}");
    }

    // 256 色灰阶中取银灰到深灰，沿左上至右下渐变，不引入彩色。
    const BRIGHTEST: usize = 252;
    const DARKEST: usize = 242;
    let max_column = STARTUP_LOGO_WIDTH - 1;
    let max_row = STARTUP_LOGO.lines().count().saturating_sub(1).max(1);
    let gradient_span = 2 * max_column * max_row;
    let mut previous_gray = None;
    for (column, glyph) in line.chars().enumerate() {
        if glyph != ' ' {
            let position = column.min(max_column) * max_row + row * max_column;
            let gray = BRIGHTEST - (BRIGHTEST - DARKEST) * position / gradient_span;
            if previous_gray != Some(gray) {
                write!(writer, "\x1b[38;5;{gray}m")?;
                previous_gray = Some(gray);
            }
        }
        write!(writer, "{glyph}")?;
    }
    // 恢复默认前景色，避免把图案的灰阶带入右侧文字或后续日志。
    write!(writer, "\x1b[39m")
}

/// 当监听地址为 `0.0.0.0` 或 `::` 通配地址时，枚举本机可用网卡的具体 IP 生成浏览器可直接点击访问的 URL。
fn access_addresses(
    addr: &SocketAddr,
    interfaces: impl IntoIterator<Item = IpAddr>,
) -> Vec<SocketAddr> {
    if !addr.ip().is_unspecified() {
        return vec![*addr];
    }
    let mut ips: Vec<_> = interfaces
        .into_iter()
        .filter(|ip| {
            ip.is_ipv4() == addr.is_ipv4()
                && !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && match ip {
                    IpAddr::V4(ip) => !ip.is_broadcast(),
                    // 链路本地 IPv6 需要接口范围，不适合用作可移植的浏览器 URL。
                    IpAddr::V6(ip) => !ip.is_unicast_link_local(),
                }
        })
        .collect();
    ips.sort_by_key(|ip| (!is_lan_ip(*ip), *ip));
    ips.dedup();
    if ips.is_empty() {
        ips.push(if addr.is_ipv4() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        });
    }
    ips.into_iter()
        .map(|ip| SocketAddr::new(ip, addr.port()))
        .collect()
}

fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_unique_local(),
    }
}

fn dashboard_addresses(addr: &SocketAddr) -> Vec<SocketAddr> {
    if !addr.ip().is_unspecified() {
        return vec![*addr];
    }
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    access_addresses(
        addr,
        interfaces
            .into_iter()
            .filter(|iface| iface.is_oper_up())
            .map(|iface| iface.ip()),
    )
}

fn dashboard_url(addr: &SocketAddr) -> String {
    format!("http://{addr}")
}

fn access_label(addr: &SocketAddr) -> &'static str {
    if addr.ip().is_loopback() {
        "本机访问地址"
    } else if is_lan_ip(addr.ip()) {
        "局域网访问地址"
    } else {
        "直连访问地址"
    }
}

fn access_hint(bound: &SocketAddr, addresses: &[SocketAddr]) -> Option<&'static str> {
    if bound.ip().is_loopback() {
        Some("直接访问仅限运行 Nuono 的电脑。")
    } else if bound.ip().is_unspecified() && addresses.iter().all(|addr| addr.ip().is_loopback()) {
        Some("未检测到可用网卡地址；当前提供本机链接，请检查服务器网络。")
    } else {
        None
    }
}

pub(super) async fn bind_and_report(
    addr: &SocketAddr,
    started: Instant,
    enabled_plugins: usize,
    disabled_plugins: usize,
    triggers: usize,
) -> anyhow::Result<TcpListener> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("无法监听 {addr}；请检查端口是否被占用，以及监听地址和权限"))?;
    let bound_addr = listener.local_addr()?;
    info!(target: "nuono::startup", "Nuono v{} 管理服务监听已绑定（耗时 {:.2} 秒）",
        env!("CARGO_PKG_VERSION"), started.elapsed().as_secs_f64());
    tracing::debug!(target: "nuono::startup", enabled_plugins, disabled_plugins, triggers, "管理路由已装配，业务状态单独报告");
    info!(target: "nuono::http", "监听地址：{bound_addr}");
    let addresses = dashboard_addresses(&bound_addr);
    for addr in &addresses {
        info!(target: "nuono::http", "{}：{}", access_label(addr), dashboard_url(addr));
    }
    if let Some(hint) = access_hint(&bound_addr, &addresses) {
        info!(target: "nuono::http", "访问提示：{hint}");
    }
    Ok(listener)
}

/// 直接输出到 `stderr` 以绕过日志级别过滤，确保首次启动时用户一定能看到管理员初始化向导地址。
pub(super) fn print_setup_required_banner(addr: &SocketAddr) {
    eprintln!("\n首次启动：打开以下地址设置管理员账号：");
    for access in dashboard_addresses(addr) {
        eprintln!("  {}", dashboard_url(&access));
    }
    eprintln!("初始化完成后，该向导自动关闭。");
    if !addr.ip().is_loopback() {
        eprintln!("当前监听 {addr}；请立即完成初始化，任何能访问该端口的人都可能抢先创建管理员。");
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };
    use tracing::instrument::WithSubscriber;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn browser_urls_use_matching_interfaces_and_preserve_concrete_hosts() {
        let interfaces = [
            "127.0.0.1",
            "::1",
            "0.0.0.0",
            "::",
            "192.168.1.20",
            "10.0.0.3",
            "192.168.1.20",
            "203.0.113.10",
            "fd00::10",
            "2001:db8::10",
            "fe80::1",
            "224.0.0.1",
            "ff02::1",
            "255.255.255.255",
        ]
        .map(|ip| ip.parse::<IpAddr>().unwrap());
        for (listen, expected) in [
            (
                "0.0.0.0:9923",
                vec![
                    "http://10.0.0.3:9923",
                    "http://192.168.1.20:9923",
                    "http://203.0.113.10:9923",
                ],
            ),
            (
                "[::]:9923",
                vec!["http://[fd00::10]:9923", "http://[2001:db8::10]:9923"],
            ),
            ("[::1]:9923", vec!["http://[::1]:9923"]),
            ("127.0.0.1:9923", vec!["http://127.0.0.1:9923"]),
            ("192.0.2.10:9923", vec!["http://192.0.2.10:9923"]),
        ] {
            let addresses = access_addresses(&listen.parse().unwrap(), interfaces);
            assert_eq!(
                addresses.iter().map(dashboard_url).collect::<Vec<_>>(),
                expected
            );
            assert!(addresses.iter().all(|addr| !addr.ip().is_unspecified()));
        }
    }

    #[test]
    fn wildcard_without_interfaces_falls_back_to_local_link_with_hint() {
        for (listen, expected) in [
            ("0.0.0.0:9923", "http://127.0.0.1:9923"),
            ("[::]:9923", "http://[::1]:9923"),
        ] {
            let bound = listen.parse().unwrap();
            let addresses = access_addresses(&bound, []);
            assert_eq!(dashboard_url(&addresses[0]), expected);
            assert_eq!(access_label(&addresses[0]), "本机访问地址");
            assert!(
                access_hint(&bound, &addresses)
                    .unwrap()
                    .contains("未检测到可用网卡地址")
            );
        }
    }

    #[tokio::test]
    async fn bind_reports_actual_port_and_does_not_announce_success_on_failure() {
        let output = Capture::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        for host in ["127.0.0.1", "0.0.0.0"] {
            let listener = bind_and_report(
                &format!("{host}:0").parse().unwrap(),
                Instant::now(),
                4,
                2,
                90,
            )
            .with_subscriber(dispatch.clone())
            .await
            .unwrap();
            let bound_addr = listener.local_addr().unwrap();
            let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
            assert!(text.contains("管理服务监听已绑定"));
            assert!(!text.contains("管理服务已启动"));
            assert!(!text.contains("个就绪"), "就绪报告不应重复插件加载清单");
            assert!(text.contains(&format!("监听地址：{bound_addr}")));
            let addresses = dashboard_addresses(&bound_addr);
            for addr in &addresses {
                assert!(text.contains(&format!("{}：{}", access_label(addr), dashboard_url(addr))));
            }
            assert!(!text.contains("http://0.0.0.0:"));
            assert!(!text.contains(":0\n"));
            assert!(!text.contains("配置的访问地址："));
            if let Some(hint) = access_hint(&bound_addr, &addresses) {
                assert!(text.contains(hint));
            } else {
                assert!(!text.contains("访问提示："));
            }
            output.0.lock().unwrap().clear();
            let error = bind_and_report(&bound_addr, Instant::now(), 4, 2, 90)
                .with_subscriber(dispatch.clone())
                .await
                .unwrap_err();
            assert!(error.to_string().contains("端口是否被占用"));
            assert!(output.0.lock().unwrap().is_empty());
        }
    }
}

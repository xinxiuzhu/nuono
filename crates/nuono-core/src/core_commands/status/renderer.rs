//! 系统状态快照的独立有界图片渲染器。
//!
//! 本模块不依赖数据库、宿主运行环境或插件 SDK；
//! 状态数据采集与最终图片编码均由调用方负责。

use ab_glyph::{Font, FontArc, Glyph, GlyphId, PxScale, ScaleFont, point};
use anyhow::{Result, anyhow};
use image::{DynamicImage, Rgba, RgbaImage};

#[derive(Clone, Debug, Default)]
pub(super) struct StatusCard {
    pub subtitle: String,
    pub uptime: String,
    pub system: Vec<CardMetric>,
    pub current: Option<Vec<CardMetric>>,
    pub all: Option<Vec<CardMetric>>,
    pub traffic: Vec<CardMetric>,
    pub daily: Vec<CardDay>,
    pub plugins: Vec<CardPlugin>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CardMetric {
    pub label: String,
    pub value: String,
    pub percent: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CardDay {
    pub label: String,
    pub current_sent: u64,
    pub all_sent: u64,
    pub all_users: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CardPlugin {
    pub name: String,
    pub metrics: Vec<CardMetric>,
    pub unavailable: bool,
    pub status: Option<String>,
}

const WIDTH: u32 = 1000;
const MARGIN: f32 = 40.0;
const PANEL_WIDTH: f32 = WIDTH as f32 - MARGIN * 2.0;
const MAX_PLUGINS: usize = 24;
const MAX_PLUGIN_METRICS: usize = 6;
const MAX_NOTES: usize = 6;
const MAX_DAYS: usize = 14;
const BACKGROUND: Rgba<u8> = Rgba([244, 247, 253, 255]);
const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
const INK: Rgba<u8> = Rgba([28, 43, 75, 255]);
const MUTED: Rgba<u8> = Rgba([113, 126, 149, 255]);
const FAINT: Rgba<u8> = Rgba([229, 235, 245, 255]);
const BLUE: Rgba<u8> = Rgba([67, 88, 214, 255]);
const TEAL: Rgba<u8> = Rgba([12, 164, 157, 255]);
const PURPLE: Rgba<u8> = Rgba([153, 105, 208, 255]);
const AMBER: Rgba<u8> = Rgba([205, 135, 53, 255]);
const FONT_BYTES: &[u8] = include_bytes!("../../../assets/status/NuonoStatusSans.ttf");

/// 在分配位图内存前预先算好各区块坐标；列表项数量不会撑大固定画布尺寸，确保内存占用有界。
#[derive(Debug)]
struct Layout {
    system: f32,
    current: Option<f32>,
    all: Option<f32>,
    today: Option<f32>,
    traffic: f32,
    daily: f32,
    plugins: f32,
    notes: f32,
    footer: f32,
    plugin_count: usize,
    note_count: usize,
    height: u32,
}

impl Layout {
    fn new(card: &StatusCard) -> Self {
        let system = 268.0;
        let mut cursor = system + 264.0 + 28.0;
        let today = (card.current.is_some() || card.all.is_some()).then_some(cursor);
        if today.is_some() {
            cursor += 40.0;
        }
        let current = card.current.as_ref().map(|_| {
            let y = cursor;
            cursor += 144.0;
            y
        });
        let all = card.all.as_ref().map(|_| {
            let y = cursor;
            cursor += 144.0;
            y
        });
        if today.is_some() {
            cursor += 16.0;
        }
        let traffic = cursor;
        cursor += 194.0;
        let daily = cursor;
        cursor += 448.0;
        let plugins = cursor;
        let plugin_count = card.plugins.len().min(MAX_PLUGINS);
        let visible_rows = plugin_count.max(1);
        cursor += 40.0 + visible_rows as f32 * 114.0 - 10.0 + 28.0;
        let notes = cursor;
        let note_count = card.notes.len().min(MAX_NOTES);
        if note_count > 0 {
            cursor += 32.0 + note_count as f32 * 25.0;
        }
        let footer = cursor + 10.0;
        Self {
            system,
            current,
            all,
            today,
            traffic,
            daily,
            plugins,
            notes,
            footer,
            plugin_count,
            note_count,
            height: (footer + 46.0).ceil() as u32,
        }
    }
}

pub(super) fn render(card: &StatusCard) -> Result<DynamicImage> {
    let font = FontArc::try_from_slice(FONT_BYTES)
        .map_err(|_| anyhow!("无法读取 Nuono 状态卡片内置字体"))?;
    let layout = Layout::new(card);
    let mut canvas = Canvas {
        image: RgbaImage::from_pixel(WIDTH, layout.height, BACKGROUND),
        font,
    };
    draw_header(&mut canvas, card);
    draw_system(&mut canvas, layout.system, &card.system);
    if let Some(y) = layout.today {
        canvas.section(y, "今日活动", "当日汇总 · 消息与命令");
    }
    if let (Some(y), Some(metrics)) = (layout.current, card.current.as_ref()) {
        draw_activity(&mut canvas, y, "当前机器人", metrics, TEAL);
    }
    if let (Some(y), Some(metrics)) = (layout.all, card.all.as_ref()) {
        draw_activity(&mut canvas, y, "全部机器人", metrics, BLUE);
    }
    draw_traffic(&mut canvas, layout.traffic, &card.traffic);
    draw_daily(&mut canvas, layout.daily, card);
    draw_plugins(&mut canvas, layout.plugins, card, layout.plugin_count);
    if layout.note_count > 0 {
        canvas.text(MARGIN + 4.0, layout.notes, 17.0, MUTED, "统计说明", 180.0);
        for (index, note) in card.notes.iter().take(MAX_NOTES).enumerate() {
            let y = layout.notes + 32.0 + index as f32 * 25.0;
            canvas.circle(MARGIN + 9.0, y + 8.0, 2.5, MUTED);
            canvas.text(MARGIN + 22.0, y, 16.0, MUTED, note, PANEL_WIDTH - 28.0);
        }
    }
    canvas.text_center(
        WIDTH as f32 / 2.0,
        layout.footer,
        15.0,
        MUTED,
        "Nuono · 宿主状态与插件公开统计",
        PANEL_WIDTH,
    );
    Ok(DynamicImage::ImageRgba8(canvas.image))
}

fn draw_header(canvas: &mut Canvas, card: &StatusCard) {
    canvas.panel(MARGIN, 40.0, PANEL_WIDTH, 202.0, 24.0);
    // 绘制轻量几何装饰图形，无需依赖外部图片素材即可保证卡片视觉效果。
    canvas.circle(917.0, 81.0, 53.0, Rgba([237, 241, 255, 255]));
    canvas.circle(881.0, 211.0, 27.0, Rgba([232, 249, 246, 255]));
    canvas.circle(946.0, 173.0, 9.0, Rgba([211, 233, 244, 255]));
    canvas.rounded(67.0, 69.0, 7.0, 42.0, 3.5, TEAL);
    canvas.text(89.0, 67.0, 45.0, INK, "Nuono 状态", 645.0);
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    canvas.rounded(794.0, 66.0, 135.0, 34.0, 12.0, BLUE);
    canvas.text_center(861.5, 74.0, 17.0, WHITE, &version, 115.0);
    canvas.text(69.0, 128.0, 20.0, MUTED, &card.subtitle, 812.0);
    canvas.rounded(68.0, 171.0, 760.0, 42.0, 12.0, Rgba([245, 248, 253, 255]));
    canvas.circle(87.0, 192.0, 4.0, TEAL);
    canvas.text(102.0, 182.0, 17.0, MUTED, "已运行", 70.0);
    canvas.text(179.0, 180.0, 20.0, INK, &card.uptime, 616.0);
}

fn draw_system(canvas: &mut Canvas, y: f32, metrics: &[CardMetric]) {
    canvas.section(y, "系统资源", "宿主当前资源快照");
    for index in 0..4 {
        let x = MARGIN + (index % 2) as f32 * 470.0;
        let row_y = y + 40.0 + (index / 2) as f32 * 116.0;
        canvas.panel(x, row_y, 450.0, 108.0, 18.0);
        let metric = metrics.get(index);
        let percent = metric
            .and_then(|metric| metric.percent)
            .filter(|p| p.is_finite());
        let color = match percent {
            Some(p) if p >= 90.0 => AMBER,
            Some(p) if p >= 75.0 => BLUE,
            _ => TEAL,
        };
        canvas.ring(x + 53.0, row_y + 54.0, 29.0, 5.0, FAINT, 100.0);
        if let Some(percent) = percent {
            canvas.ring(
                x + 53.0,
                row_y + 54.0,
                29.0,
                5.0,
                color,
                percent.clamp(0.0, 100.0),
            );
        }
        let gauge = percent
            .map(|percent| format!("{:.0}%", percent.clamp(0.0, 100.0)))
            .unwrap_or_else(|| "—".to_owned());
        canvas.text_center(x + 53.0, row_y + 45.0, 16.0, INK, &gauge, 52.0);
        canvas.text(
            x + 103.0,
            row_y + 24.0,
            17.0,
            MUTED,
            metric.map_or("暂无指标", |m| m.label.as_str()),
            322.0,
        );
        canvas.text(
            x + 103.0,
            row_y + 54.0,
            23.0,
            INK,
            metric.map_or("—", |m| m.value.as_str()),
            322.0,
        );
    }
}

fn draw_activity(
    canvas: &mut Canvas,
    y: f32,
    title: &str,
    metrics: &[CardMetric],
    color: Rgba<u8>,
) {
    canvas.panel(MARGIN, y, PANEL_WIDTH, 132.0, 18.0);
    canvas.rounded(MARGIN + 21.0, y + 20.0, 4.0, 19.0, 2.0, color);
    canvas.text(MARGIN + 37.0, y + 21.0, 17.0, color, title, 290.0);
    if metrics.is_empty() {
        canvas.text(
            MARGIN + 28.0,
            y + 69.0,
            19.0,
            MUTED,
            "暂无今日记录",
            PANEL_WIDTH - 56.0,
        );
        return;
    }
    let cell_width = (PANEL_WIDTH - 38.0) / 5.0;
    for (index, metric) in metrics.iter().take(5).enumerate() {
        let center = MARGIN + 19.0 + (index as f32 + 0.5) * cell_width;
        if index > 0 {
            canvas.line(
                center - cell_width / 2.0,
                y + 58.0,
                center - cell_width / 2.0,
                y + 111.0,
                1.0,
                FAINT,
            );
        }
        canvas.text_center(
            center,
            y + 58.0,
            31.0,
            INK,
            &metric.value,
            cell_width - 20.0,
        );
        canvas.text_center(
            center,
            y + 99.0,
            17.0,
            MUTED,
            &metric.label,
            cell_width - 20.0,
        );
    }
}

fn draw_traffic(canvas: &mut Canvas, y: f32, metrics: &[CardMetric]) {
    canvas.section(y, "处理概况", "本次进程 · 插件回调");
    canvas.panel(MARGIN, y + 40.0, PANEL_WIDTH, 126.0, 18.0);
    if metrics.is_empty() {
        canvas.text(
            MARGIN + 28.0,
            y + 94.0,
            19.0,
            MUTED,
            "暂无回调统计",
            PANEL_WIDTH - 56.0,
        );
        return;
    }
    let cell_width = (PANEL_WIDTH - 38.0) / 4.0;
    for (index, metric) in metrics.iter().take(4).enumerate() {
        let center = MARGIN + 19.0 + (index as f32 + 0.5) * cell_width;
        if index > 0 {
            canvas.line(
                center - cell_width / 2.0,
                y + 71.0,
                center - cell_width / 2.0,
                y + 141.0,
                1.0,
                FAINT,
            );
        }
        canvas.text_center(
            center,
            y + 72.0,
            29.0,
            BLUE,
            &metric.value,
            cell_width - 22.0,
        );
        canvas.text_center(
            center,
            y + 117.0,
            17.0,
            MUTED,
            &metric.label,
            cell_width - 22.0,
        );
    }
}

fn draw_daily(canvas: &mut Canvas, y: f32, card: &StatusCard) {
    canvas.section(y, "近 14 日活动", "消息发送与活跃用户");
    let panel_y = y + 40.0;
    canvas.panel(MARGIN, panel_y, PANEL_WIDTH, 380.0, 18.0);
    canvas.text(
        MARGIN + 24.0,
        panel_y + 22.0,
        18.0,
        INK,
        "消息发送量",
        180.0,
    );
    if card.current.is_some() {
        canvas.legend(566.0, panel_y + 23.0, TEAL, "当前机器人");
    }
    canvas.legend(752.0, panel_y + 23.0, BLUE, "全部机器人");
    let days = &card.daily[card.daily.len().saturating_sub(MAX_DAYS)..];
    let left = 116.0;
    let right = 913.0;
    let message_top = panel_y + 69.0;
    let message_bottom = panel_y + 203.0;
    let sent_max = nice_max(
        days.iter()
            .map(|day| {
                if card.current.is_some() {
                    day.all_sent.max(day.current_sent)
                } else {
                    day.all_sent
                }
            })
            .max()
            .unwrap_or(0),
    );
    draw_grid(canvas, left, right, message_top, message_bottom, sent_max);
    if card.current.is_some() {
        draw_series(
            canvas,
            days,
            left,
            right,
            message_top,
            message_bottom,
            sent_max,
            TEAL,
            |day| day.current_sent,
        );
    }
    draw_series(
        canvas,
        days,
        left,
        right,
        message_top,
        message_bottom,
        sent_max,
        BLUE,
        |day| day.all_sent,
    );
    canvas.text(
        MARGIN + 24.0,
        panel_y + 230.0,
        18.0,
        INK,
        "活跃用户 · 全部机器人",
        350.0,
    );
    canvas.circle(896.0, panel_y + 239.0, 4.0, PURPLE);
    let user_top = panel_y + 267.0;
    let user_bottom = panel_y + 321.0;
    let user_max = nice_max(days.iter().map(|day| day.all_users).max().unwrap_or(0));
    draw_grid(canvas, left, right, user_top, user_bottom, user_max);
    draw_series(
        canvas,
        days,
        left,
        right,
        user_top,
        user_bottom,
        user_max,
        PURPLE,
        |day| day.all_users,
    );
    if days.is_empty() {
        canvas.text_center(
            (left + right) / 2.0,
            message_top + 50.0,
            20.0,
            MUTED,
            "暂无历史记录",
            650.0,
        );
    } else {
        for (index, day) in days.iter().enumerate() {
            if index % 3 == 0 || index + 1 == days.len() {
                let x = chart_x(index, days.len(), left, right);
                canvas.text_center(x, user_bottom + 11.0, 14.0, MUTED, &day.label, 78.0);
            }
        }
    }
    canvas.text_center(
        WIDTH as f32 / 2.0,
        panel_y + 357.0,
        14.0,
        MUTED,
        "发送消息与活跃用户分别使用独立纵轴",
        800.0,
    );
}

fn draw_grid(canvas: &mut Canvas, left: f32, right: f32, top: f32, bottom: f32, maximum: f64) {
    for index in 0..=2 {
        let fraction = index as f32 / 2.0;
        let y = bottom - (bottom - top) * fraction;
        canvas.line(left, y, right, y, 1.0, FAINT);
        canvas.text_right(
            left - 16.0,
            y - 7.0,
            14.0,
            MUTED,
            &compact_count(maximum * fraction as f64),
            60.0,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_series(
    canvas: &mut Canvas,
    days: &[CardDay],
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    maximum: f64,
    color: Rgba<u8>,
    value: impl Fn(&CardDay) -> u64,
) {
    let mut previous = None;
    for (index, day) in days.iter().enumerate() {
        let x = chart_x(index, days.len(), left, right);
        let fraction = (value(day) as f64 / maximum).clamp(0.0, 1.0) as f32;
        let y = bottom - (bottom - top) * fraction;
        if let Some((previous_x, previous_y)) = previous {
            canvas.line(previous_x, previous_y, x, y, 2.5, color);
        }
        canvas.circle(x, y, 4.3, WHITE);
        canvas.circle(x, y, 2.9, color);
        previous = Some((x, y));
    }
}

fn chart_x(index: usize, count: usize, left: f32, right: f32) -> f32 {
    if count <= 1 {
        (left + right) / 2.0
    } else {
        left + (right - left) * index as f32 / (count - 1) as f32
    }
}

fn nice_max(value: u64) -> f64 {
    let value = value.max(1) as f64;
    let magnitude = 10.0_f64.powf(value.log10().floor());
    let leading = value / magnitude;
    let step = if leading <= 1.0 {
        1.0
    } else if leading <= 2.0 {
        2.0
    } else if leading <= 5.0 {
        5.0
    } else {
        10.0
    };
    magnitude * step
}

fn compact_count(value: f64) -> String {
    if value >= 100_000_000.0 {
        format!("{:.1}亿", value / 100_000_000.0)
    } else if value >= 10_000.0 {
        format!("{:.1}万", value / 10_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}千", value / 1_000.0)
    } else if value > 0.0 && value < 1.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

fn draw_plugins(canvas: &mut Canvas, y: f32, card: &StatusCard, plugin_count: usize) {
    canvas.section(y, "插件公开统计", "由插件声明并提供");
    if plugin_count == 0 {
        canvas.panel(MARGIN, y + 40.0, PANEL_WIDTH, 104.0, 18.0);
        canvas.text(
            MARGIN + 28.0,
            y + 64.0,
            22.0,
            INK,
            "暂无可展示的插件统计",
            PANEL_WIDTH - 56.0,
        );
        canvas.text(
            MARGIN + 28.0,
            y + 99.0,
            16.0,
            MUTED,
            "插件尚未提供公开指标，或统计暂不可用",
            PANEL_WIDTH - 56.0,
        );
        return;
    }
    for (index, plugin) in card.plugins.iter().take(MAX_PLUGINS).enumerate() {
        let row_y = y + 40.0 + index as f32 * 114.0;
        canvas.panel(MARGIN, row_y, PANEL_WIDTH, 104.0, 18.0);
        let color = if plugin.unavailable { AMBER } else { BLUE };
        canvas.rounded(
            MARGIN + 18.0,
            row_y + 16.0,
            34.0,
            34.0,
            11.0,
            Rgba([238, 242, 254, 255]),
        );
        let initial = plugin.name.chars().next().unwrap_or('P').to_string();
        canvas.text_center(MARGIN + 35.0, row_y + 23.0, 20.0, color, &initial, 25.0);
        canvas.text(MARGIN + 18.0, row_y + 64.0, 17.0, INK, &plugin.name, 188.0);
        canvas.line(
            MARGIN + 221.0,
            row_y + 22.0,
            MARGIN + 221.0,
            row_y + 82.0,
            1.0,
            FAINT,
        );
        if plugin.unavailable || plugin.metrics.is_empty() {
            canvas.text(
                MARGIN + 252.0,
                row_y + 44.0,
                19.0,
                if plugin.unavailable { AMBER } else { MUTED },
                if plugin.unavailable {
                    plugin.status.as_deref().unwrap_or("暂时无法获取")
                } else {
                    "暂无公开数据"
                },
                PANEL_WIDTH - 280.0,
            );
            continue;
        }
        let count = plugin.metrics.len().min(MAX_PLUGIN_METRICS);
        let cell_width = (PANEL_WIDTH - 246.0) / count as f32;
        for (metric_index, metric) in plugin.metrics.iter().take(MAX_PLUGIN_METRICS).enumerate() {
            let center = MARGIN + 238.0 + (metric_index as f32 + 0.5) * cell_width;
            canvas.text_center(
                center,
                row_y + 26.0,
                27.0,
                INK,
                &metric.value,
                cell_width - 14.0,
            );
            canvas.text_center(
                center,
                row_y + 67.0,
                16.0,
                MUTED,
                &metric.label,
                cell_width - 14.0,
            );
        }
    }
}

struct Canvas {
    image: RgbaImage,
    font: FontArc,
}

impl Canvas {
    fn blend(&mut self, x: i32, y: i32, color: Rgba<u8>, coverage: f32) {
        if x < 0 || y < 0 || x >= self.image.width() as i32 || y >= self.image.height() as i32 {
            return;
        }
        let alpha = (coverage.clamp(0.0, 1.0) * color.0[3] as f32 / 255.0).clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return;
        }
        let target = self.image.get_pixel_mut(x as u32, y as u32);
        for channel in 0..3 {
            target.0[channel] = (target.0[channel] as f32 * (1.0 - alpha)
                + color.0[channel] as f32 * alpha)
                .round() as u8;
        }
    }

    fn panel(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32) {
        self.rounded(x, y + 4.0, width, height, radius, Rgba([56, 79, 123, 7]));
        self.rounded(x, y, width, height, radius, WHITE);
    }

    fn rounded(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32, color: Rgba<u8>) {
        let radius = radius.min(width / 2.0).min(height / 2.0);
        let center_x = x + width / 2.0;
        let center_y = y + height / 2.0;
        let half_width = width / 2.0 - radius;
        let half_height = height / 2.0 - radius;
        for py in
            y.floor().max(0.0) as i32..(y + height).ceil().min(self.image.height() as f32) as i32
        {
            for px in
                x.floor().max(0.0) as i32..(x + width).ceil().min(self.image.width() as f32) as i32
            {
                let qx = (px as f32 + 0.5 - center_x).abs() - half_width;
                let qy = (py as f32 + 0.5 - center_y).abs() - half_height;
                let distance = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
                self.blend(px, py, color, 0.5 - distance);
            }
        }
    }

    fn circle(&mut self, x: f32, y: f32, radius: f32, color: Rgba<u8>) {
        for py in (y - radius - 1.0).floor() as i32..=(y + radius + 1.0).ceil() as i32 {
            for px in (x - radius - 1.0).floor() as i32..=(x + radius + 1.0).ceil() as i32 {
                let distance = (px as f32 + 0.5 - x).hypot(py as f32 + 0.5 - y);
                self.blend(px, py, color, radius + 0.5 - distance);
            }
        }
    }

    fn ring(&mut self, x: f32, y: f32, radius: f32, thickness: f32, color: Rgba<u8>, percent: f32) {
        let percent = if percent.is_finite() {
            percent.clamp(0.0, 100.0)
        } else {
            0.0
        };
        if percent <= 0.0 {
            return;
        }
        let sweep = std::f32::consts::TAU * percent / 100.0;
        let extent = radius + thickness / 2.0 + 1.0;
        for py in (y - extent).floor() as i32..=(y + extent).ceil() as i32 {
            for px in (x - extent).floor() as i32..=(x + extent).ceil() as i32 {
                let dx = px as f32 + 0.5 - x;
                let dy = py as f32 + 0.5 - y;
                let angle =
                    (dy.atan2(dx) + std::f32::consts::FRAC_PI_2).rem_euclid(std::f32::consts::TAU);
                if angle <= sweep {
                    self.blend(
                        px,
                        py,
                        color,
                        thickness / 2.0 + 0.5 - (dx.hypot(dy) - radius).abs(),
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, thickness: f32, color: Rgba<u8>) {
        let dx = x2 - x1;
        let dy = y2 - y1;
        let length_squared = dx * dx + dy * dy;
        let padding = thickness / 2.0 + 1.0;
        for py in (y1.min(y2) - padding).floor() as i32..=(y1.max(y2) + padding).ceil() as i32 {
            for px in (x1.min(x2) - padding).floor() as i32..=(x1.max(x2) + padding).ceil() as i32 {
                let projection = if length_squared > 0.0 {
                    (((px as f32 + 0.5 - x1) * dx + (py as f32 + 0.5 - y1) * dy) / length_squared)
                        .clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let distance = (px as f32 + 0.5 - x1 - projection * dx)
                    .hypot(py as f32 + 0.5 - y1 - projection * dy);
                self.blend(px, py, color, thickness / 2.0 + 0.5 - distance);
            }
        }
    }

    fn section(&mut self, y: f32, title: &str, detail: &str) {
        self.text(MARGIN + 4.0, y, 25.0, INK, title, 440.0);
        self.text_right(
            WIDTH as f32 - MARGIN - 4.0,
            y + 7.0,
            15.0,
            MUTED,
            detail,
            380.0,
        );
    }

    fn legend(&mut self, x: f32, y: f32, color: Rgba<u8>, label: &str) {
        self.line(x, y + 8.0, x + 23.0, y + 8.0, 3.0, color);
        self.circle(x + 11.5, y + 8.0, 3.0, color);
        self.text(x + 34.0, y, 15.0, MUTED, label, 145.0);
    }

    fn scale(&self, size: f32) -> PxScale {
        // 按像素每 em（px/em）换算字号，使中日韩（CJK）与西文字体在相同字号下保持一致的排版高度。
        PxScale::from(
            size * self.font.height_unscaled()
                / self
                    .font
                    .units_per_em()
                    .unwrap_or(self.font.height_unscaled()),
        )
    }

    fn supported_char(&self, character: char) -> char {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if self.font.glyph_id(character) != GlyphId(0) {
            character
        } else if self.font.glyph_id('□') != GlyphId(0) {
            '□'
        } else {
            '?'
        }
    }

    fn text_width(&self, value: &str, size: f32) -> f32 {
        let scaled = self.font.as_scaled(self.scale(size));
        let mut width = 0.0;
        let mut previous = None;
        for character in value.chars() {
            let id = self.font.glyph_id(self.supported_char(character));
            if let Some(previous) = previous {
                width += scaled.kern(previous, id);
            }
            width += scaled.h_advance(id);
            previous = Some(id);
        }
        width
    }

    fn fit_text(&self, value: &str, size: f32, maximum: f32) -> String {
        if maximum <= 0.0 {
            return String::new();
        }
        // 过滤换行等控制字符，防止意外折行，同时保证任意长度的插件名称都能按预期单行截断。
        let normalized: Vec<char> = value
            .chars()
            .take(1025)
            .map(|c| self.supported_char(c))
            .collect();
        let bounded: String = normalized.iter().take(1024).collect();
        if normalized.len() <= 1024 && self.text_width(&bounded, size) <= maximum {
            return bounded;
        }
        let ellipsis = if self.font.glyph_id('…') != GlyphId(0) {
            "…"
        } else {
            "..."
        };
        let ellipsis_width = self.text_width(ellipsis, size);
        if ellipsis_width > maximum {
            return String::new();
        }
        let scaled = self.font.as_scaled(self.scale(size));
        let mut fitted = String::new();
        let mut width = 0.0;
        let mut previous = None;
        for character in normalized.iter().take(1024) {
            let id = self.font.glyph_id(*character);
            let kerning = previous.map_or(0.0, |previous| scaled.kern(previous, id));
            let advance = scaled.h_advance(id);
            if width + kerning + advance + ellipsis_width > maximum {
                break;
            }
            width += kerning + advance;
            fitted.push(*character);
            previous = Some(id);
        }
        fitted.push_str(ellipsis);
        fitted
    }

    fn text(&mut self, x: f32, y: f32, size: f32, color: Rgba<u8>, value: &str, maximum: f32) {
        let fitted = self.fit_text(value, size, maximum);
        self.draw_fitted_text(x, y, size, color, &fitted, x, x + maximum);
    }

    fn text_center(
        &mut self,
        center: f32,
        y: f32,
        size: f32,
        color: Rgba<u8>,
        value: &str,
        maximum: f32,
    ) {
        let fitted = self.fit_text(value, size, maximum);
        let x = center - self.text_width(&fitted, size) / 2.0;
        self.draw_fitted_text(
            x,
            y,
            size,
            color,
            &fitted,
            center - maximum / 2.0,
            center + maximum / 2.0,
        );
    }

    fn text_right(
        &mut self,
        right: f32,
        y: f32,
        size: f32,
        color: Rgba<u8>,
        value: &str,
        maximum: f32,
    ) {
        let fitted = self.fit_text(value, size, maximum);
        let x = right - self.text_width(&fitted, size);
        self.draw_fitted_text(x, y, size, color, &fitted, right - maximum, right);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_fitted_text(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        color: Rgba<u8>,
        value: &str,
        clip_left: f32,
        clip_right: f32,
    ) {
        let scale = self.scale(size);
        let font = self.font.clone();
        let scaled = font.as_scaled(scale);
        let mut cursor = x;
        let mut previous = None;
        // 使用统一基线对齐中文标签与西文数值，像素裁剪时兼容字形的下延（overhang）部分。
        let baseline = y + size * 0.88;
        for character in value.chars() {
            let id = font.glyph_id(character);
            if let Some(previous) = previous {
                cursor += scaled.kern(previous, id);
            }
            let glyph = Glyph {
                id,
                scale,
                position: point(cursor, baseline),
            };
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|px, py, coverage| {
                    let px = bounds.min.x as i32 + px as i32;
                    if px as f32 >= clip_left && (px as f32) < clip_right {
                        self.blend(px, bounds.min.y as i32 + py as i32, color, coverage);
                    }
                });
            }
            cursor += scaled.h_advance(id);
            previous = Some(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(label: &str, value: &str, percent: Option<f32>) -> CardMetric {
        CardMetric {
            label: label.to_owned(),
            value: value.to_owned(),
            percent,
        }
    }

    fn preview_card() -> StatusCard {
        StatusCard {
            subtitle: "演示预览 · 模拟数据 · 2026-10-14 14:32:08".to_owned(),
            uptime: "3 天 06 小时 18 分钟".to_owned(),
            system: vec![
                metric("CPU 使用率", "18.2%", Some(18.2)),
                metric("系统内存", "6.4 / 16.0 GiB", Some(40.0)),
                metric("进程内存 RSS", "384 MiB", None),
                metric("数据盘已用", "112.0 / 512.0 GiB", Some(21.9)),
            ],
            current: Some(vec![
                metric("今日接收", "7,148", None),
                metric("今日发送", "775", None),
                metric("触发命令", "550", None),
                metric("活跃群组", "6", None),
                metric("活跃用户", "184", None),
            ]),
            all: Some(vec![
                metric("今日接收", "72,516", None),
                metric("今日发送", "7,603", None),
                metric("触发命令", "3,127", None),
                metric("活跃群组", "92", None),
                metric("活跃用户", "1,028", None),
            ]),
            traffic: vec![
                metric("完成回调", "18,420", None),
                metric("正在执行", "7", None),
                metric("平均耗时", "31.6 ms", None),
                metric("最大耗时", "1240.0 ms", None),
            ],
            daily: (0..14)
                .map(|day| CardDay {
                    label: format!("10/{:02}", day + 1),
                    current_sent: [
                        426, 613, 587, 821, 775, 692, 930, 722, 1024, 882, 1203, 967, 1455, 775,
                    ][day],
                    all_sent: [
                        4600, 5900, 5700, 6800, 7100, 6400, 8000, 6800, 8200, 7700, 11300, 8400,
                        12700, 7603,
                    ][day],
                    all_users: [
                        664, 702, 693, 743, 811, 740, 825, 783, 897, 860, 1061, 923, 1128, 1028,
                    ][day],
                })
                .collect(),
            plugins: vec![
                CardPlugin {
                    name: "示例工具".to_owned(),
                    metrics: vec![
                        metric("绑定 UID", "2.9 万", None),
                        metric("登录账号", "1.8 万", None),
                        metric("今日签到", "3,027", None),
                    ],
                    unavailable: false,
                    status: None,
                },
                CardPlugin {
                    name: "社区签到".to_owned(),
                    metrics: vec![
                        metric("开启签到", "2,934", None),
                        metric("今日成功", "2,871", None),
                        metric("今日失败", "12", None),
                    ],
                    unavailable: false,
                    status: None,
                },
                CardPlugin {
                    name: "四项指标示例".to_owned(),
                    metrics: (1..=4)
                        .map(|index| {
                            metric(
                                &format!("公开指标 {index}"),
                                &format!("{}", index * 137),
                                None,
                            )
                        })
                        .collect(),
                    unavailable: false,
                    status: None,
                },
                CardPlugin {
                    name: "统计超时示例".to_owned(),
                    metrics: Vec::new(),
                    unavailable: true,
                    status: Some("统计超时".to_owned()),
                },
            ],
            notes: vec![
                "本图为模拟数据，仅用于验证状态卡片排版。".to_owned(),
                "每日记录来自数据库；处理概况仅统计本次进程内的插件回调。".to_owned(),
                "插件只显示主动声明的公开指标，不展示账号与凭据。".to_owned(),
            ],
        }
    }

    fn test_canvas() -> Canvas {
        Canvas {
            image: RgbaImage::from_pixel(400, 200, BACKGROUND),
            font: FontArc::try_from_slice(FONT_BYTES).unwrap(),
        }
    }

    #[test]
    fn layout_caps_untrusted_collection_sizes() {
        let mut card = preview_card();
        card.plugins = vec![CardPlugin::default(); 10_000];
        card.notes = vec!["long note".to_owned(); 10_000];
        let layout = Layout::new(&card);
        assert_eq!(layout.plugin_count, MAX_PLUGINS);
        assert_eq!(layout.note_count, MAX_NOTES);
        assert!(layout.height <= 4_700, "height was {}", layout.height);
        assert!(layout.footer + 25.0 < layout.height as f32);
    }

    #[test]
    fn long_strings_are_truncated_to_pixel_width_and_missing_glyphs_fall_back() {
        let canvas = test_canvas();
        let fitted = canvas.fit_text(
            &"很长的插件名称与统计值 1234567890".repeat(100),
            20.0,
            125.0,
        );
        assert!(canvas.text_width(&fitted, 20.0) <= 125.0);
        assert!(fitted.ends_with('…') || fitted.ends_with("..."));
        let fallback = canvas.fit_text("\u{10ffff}\n", 20.0, 125.0);
        assert!(!fallback.contains('\n'));
        assert!(
            fallback
                .chars()
                .all(|character| canvas.font.glyph_id(character) != GlyphId(0))
        );
    }

    #[test]
    fn chart_scale_covers_values_and_single_day_is_centered() {
        for value in [0, 1, 9, 17, 775, 1_234, 100_000, u64::MAX] {
            assert!(nice_max(value) >= value.max(1) as f64);
        }
        assert_eq!(chart_x(0, 1, 100.0, 900.0), 500.0);
        assert_eq!(chart_x(0, 14, 100.0, 900.0), 100.0);
        assert_eq!(chart_x(13, 14, 100.0, 900.0), 900.0);
    }

    #[test]
    fn renderer_handles_empty_data_and_non_finite_percentages() {
        let card = StatusCard {
            system: vec![
                metric("CPU", "未知", Some(f32::NAN)),
                metric("内存", "未知", Some(f32::INFINITY)),
            ],
            ..StatusCard::default()
        };
        let image = render(&card).unwrap().into_rgba8();
        assert_eq!(image.width(), WIDTH);
        assert_eq!(image.height(), Layout::new(&card).height);
        assert_eq!(*image.get_pixel(45, 140), WHITE);
        assert_eq!(*image.get_pixel(0, 0), BACKGROUND);
        assert!(image.pixels().all(|pixel| pixel.0[3] == 255));
    }

    /// 配合环境变量 `NUONO_STATUS_PREVIEW_PATH=/absolute/path/status.png` 与 `--ignored` 运行，
    /// 可生成供人工视觉检查的状态卡片预览图。
    #[test]
    #[ignore = "writes a simulated visual review fixture when explicitly requested"]
    fn write_simulated_status_preview() {
        let path = std::env::var_os("NUONO_STATUS_PREVIEW_PATH")
            .expect("set NUONO_STATUS_PREVIEW_PATH to the preview PNG path");
        render(&preview_card()).unwrap().save(path).unwrap();
    }
}

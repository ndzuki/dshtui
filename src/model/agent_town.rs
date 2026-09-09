//! Agent Town 场景模型（REQ-009 §5，Step 4）：`agent-monitor.html` 绘制算法
//! 与行为状态的 Rust 移植（960×540、Uint32Array → `Vec<u32>` RGBA、四季色板、
//! 昼夜光影、STAGE→职业建筑、A* 寻路、装饰居民）。
//!
//! 移植口径（2026-09-07 Step 4 Prototype 已验证）：固定时间戳下静态场景
//! （21 建筑 + 水面/花田/农田/树木/喷泉）与 `agent-monitor.html` 同函数输出
//! **逐字节一致**（PPM 双跑比对）；release 单帧绘制 ≈0.9ms（预算 <33ms）。
//!
//! 模型层纯同步、不依赖 reqwest/ratatui（Step 4 是绘制正确性 seam）；
//! 碰撞网格 12px/格（80×45）与 `agent-monitor.html` 同口径。

use std::collections::HashMap;

use crate::api::types::SessionId;
use crate::model::agent_roster::{stage_meta, AgentRosterEntry, AgentStatus};
use crate::model::sprite_tables::{NPC_TABLES, PASSER_TABLES};

/// 逻辑画布尺寸（design-spec §3）。
pub const TOWN_W: usize = 960;
pub const TOWN_H: usize = 540;
/// 一天 10 分钟（design-spec §4）。
pub const DAY_MS: f64 = 600_000.0;
/// 一季 8 天（design-spec §4）。
pub const SEASON_DAYS: f64 = 8.0;
/// 起始 07:30（design-spec §4）。
const TIME_OFFSET: f64 = (7.5 / 24.0) * DAY_MS;
/// 碰撞网格粒度（design-spec §10）。
const TILE: i32 = 12;
const GW: usize = TOWN_W.div_ceil(TILE as usize); // 80
const GH: usize = TOWN_H.div_ceil(TILE as usize); // 45

// ============================ 四季色板（agent-monitor.html L228-233） ============================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeasonKind {
    Spring,
    Summer,
    Autumn,
    Winter,
}

impl SeasonKind {
    pub fn name(&self) -> &'static str {
        match self {
            SeasonKind::Spring => "春",
            SeasonKind::Summer => "夏",
            SeasonKind::Autumn => "秋",
            SeasonKind::Winter => "冬",
        }
    }
    pub fn emoji(&self) -> &'static str {
        match self {
            SeasonKind::Spring => "🌸",
            SeasonKind::Summer => "☀️",
            SeasonKind::Autumn => "🍂",
            SeasonKind::Winter => "❄️",
        }
    }
    pub fn particle(&self) -> ParticleKind {
        match self {
            SeasonKind::Spring => ParticleKind::Petal,
            SeasonKind::Summer => ParticleKind::Rain,
            SeasonKind::Autumn => ParticleKind::Leaf,
            SeasonKind::Winter => ParticleKind::Snow,
        }
    }
}

pub const SEASON_KINDS: [SeasonKind; 4] = [
    SeasonKind::Spring,
    SeasonKind::Summer,
    SeasonKind::Autumn,
    SeasonKind::Winter,
];

struct Season {
    kind: SeasonKind,
    /// 保留 agent-monitor.html 色板全集（天空渐变字段在静态场景未使用，
    /// 仅作 1:1 移植完整性保留）。
    #[allow(dead_code)]
    sky: &'static str,
    #[allow(dead_code)]
    sky2: &'static str,
    m: &'static str,
    l: &'static str,
    h: &'static str,
    w: &'static str,
    y: &'static str,
    d: &'static str,
    b: &'static str,
    g: &'static str,
    k: &'static str,
    r: &'static str,
    p: &'static str,
}

const SEASONS: [Season; 4] = [
    Season {
        kind: SeasonKind::Spring,
        sky: "#78c8f0",
        sky2: "#b8ecff",
        m: "#48a848",
        l: "#7bc86a",
        h: "#e8c880",
        w: "#fff8ec",
        y: "#ffd858",
        d: "#1a1c2c",
        b: "#5888e0",
        g: "#58b858",
        k: "#6b4423",
        r: "#e85040",
        p: "#f48fb0",
    },
    Season {
        kind: SeasonKind::Summer,
        sky: "#60b0f0",
        sky2: "#a8e0ff",
        m: "#3c9c40",
        l: "#78d05e",
        h: "#f0d080",
        w: "#fffaf0",
        y: "#ffd24a",
        d: "#1a1c2c",
        b: "#4a8ee8",
        g: "#58b858",
        k: "#6b4423",
        r: "#e85040",
        p: "#f07098",
    },
    Season {
        kind: SeasonKind::Autumn,
        sky: "#a8c8e0",
        sky2: "#f0e0b8",
        m: "#a08040",
        l: "#d0b058",
        h: "#e8c880",
        w: "#faf0da",
        y: "#e8a828",
        d: "#2a2418",
        b: "#9fb8d8",
        g: "#8a8a4a",
        k: "#6e4a1f",
        r: "#d06040",
        p: "#e09070",
    },
    Season {
        kind: SeasonKind::Winter,
        sky: "#90b8d8",
        sky2: "#e0ecf8",
        m: "#78a0a8",
        l: "#d0e0e8",
        h: "#f0e8e0",
        w: "#ffffff",
        y: "#f2c94c",
        d: "#2c4458",
        b: "#7fa4d8",
        g: "#8ab8c8",
        k: "#5d4a35",
        r: "#b85048",
        p: "#e0b0c0",
    },
];

fn hex_rgb(hex: &str) -> (u8, u8, u8) {
    (
        u8::from_str_radix(&hex[1..3], 16).unwrap(),
        u8::from_str_radix(&hex[3..5], 16).unwrap(),
        u8::from_str_radix(&hex[5..7], 16).unwrap(),
    )
}

fn mix(c: f64, t: f64, f: f64) -> f64 {
    c + (t - c) * f
}

/// `shade()`（agent-monitor.html:237-247）逐行移植。
fn shade(hex: &str, bright: f64, night_f: f64, dusk_f: f64, warm_f: f64) -> u32 {
    let (r0, g0, b0) = hex_rgb(hex);
    let (mut r, mut g, mut b) = (r0 as f64, g0 as f64, b0 as f64);
    let nt = night_f * 0.5;
    let ot = dusk_f * 0.25;
    let wt = warm_f * 0.35;
    r = mix(r, 24.0, nt);
    g = mix(g, 34.0, nt);
    b = mix(b, 64.0, nt);
    r = mix(r, 224.0, ot);
    g = mix(g, 132.0, ot);
    b = mix(b, 60.0, ot);
    r = mix(r, 232.0, wt);
    g = mix(g, 144.0, wt);
    b = mix(b, 72.0, wt);
    r = (r * bright).round().min(255.0);
    g = (g * bright).round().min(255.0);
    b = (b * bright).round().min(255.0);
    0xff00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

fn daylight(min: f64) -> f64 {
    if !(330.0..=1050.0).contains(&min) {
        0.0
    } else {
        (((min - 330.0) / 720.0) * std::f64::consts::PI).sin()
    }
}

/// 时段名（agent-monitor.html phaseName L1926）。
pub fn phase_name(min: f64) -> &'static str {
    if !(330.0..1170.0).contains(&min) {
        "深夜"
    } else if min < 450.0 {
        "清晨"
    } else if min < 1050.0 {
        "白天"
    } else if min < 1170.0 {
        "黄昏"
    } else {
        "夜晚"
    }
}

/// 当前光照调色（`palNow` L674-703）。
#[derive(Clone, Copy)]
pub struct Pal {
    pub season: SeasonKind,
    pub bright: f64,
    pub night_f: f64,
    pub dusk_f: f64,
    pub min_of_day: f64,
    pub sun_az: f64,
    pub shadow_dx: f64,
    pub shadow_dy: f64,
    shadow_color: u32,
    m: u32,
    l: u32,
    h: u32,
    /// HTML pal 字段全集保留（w/d/b 静态场景未引用，1:1 移植完整性）。
    #[allow(dead_code)]
    w: u32,
    y: u32,
    #[allow(dead_code)]
    d: u32,
    #[allow(dead_code)]
    b: u32,
    g: u32,
    k: u32,
    r: u32,
    p: u32,
}

pub fn pal_now(now_ms: f64) -> Pal {
    let t = now_ms + TIME_OFFSET;
    let day = (t / DAY_MS).floor();
    let min = ((t % DAY_MS) / DAY_MS) * 1440.0;
    let season = &SEASONS[(day / SEASON_DAYS).floor() as usize % SEASONS.len()];
    let day_f = daylight(min);
    let bright = 0.55 + 0.45 * day_f.max(0.0);
    let dusk_f = if day_f < 0.30 {
        (0.30 - day_f) / 0.30
    } else {
        0.0
    };
    let night_f = if day_f < 0.10 {
        (0.10 - day_f) / 0.10
    } else {
        0.0
    };
    let warm_f = if (300.0..480.0).contains(&min) {
        0.5 - (((min - 390.0) / 180.0 - 0.5).abs() * 0.4)
    } else if (960.0..1140.0).contains(&min) {
        0.5 - (((min - 1050.0) / 180.0 - 0.5).abs() * 0.4)
    } else {
        0.0
    };
    let day_prog = ((min - 330.0) / 720.0).clamp(0.0, 1.0);
    let sun_elev = day_f.max(0.0);
    let sun_az = day_prog * std::f64::consts::PI;
    let shadow_len = (1.55 - sun_elev * 0.9) * 24.0;
    let shadow_dx = -sun_az.cos() * shadow_len;
    let shadow_dy = -sun_az.sin() * shadow_len * 0.42;
    let sh = |h: &str| shade(h, bright, night_f, dusk_f, warm_f);
    let shadow_color = shade(
        "#1d2130",
        (bright - 0.35).max(0.45),
        night_f,
        dusk_f,
        warm_f,
    );
    Pal {
        season: season.kind,
        bright,
        night_f,
        dusk_f,
        min_of_day: min,
        sun_az,
        shadow_dx,
        shadow_dy,
        shadow_color,
        m: sh(season.m),
        l: sh(season.l),
        h: sh(season.h),
        w: sh(season.w),
        y: sh(season.y),
        d: sh(season.d),
        b: sh(season.b),
        g: sh(season.g),
        k: sh(season.k),
        r: sh(season.r),
        p: sh(season.p),
    }
}

// ============================ 绘制基元 ============================

fn px(b: &mut [u32], x: i32, y: i32, c: u32) {
    if x >= 0 && x < TOWN_W as i32 && y >= 0 && y < TOWN_H as i32 {
        b[y as usize * TOWN_W + x as usize] = c;
    }
}

fn rect(b: &mut [u32], x: i32, y: i32, w: i32, h: i32, c: u32) {
    for j in 0..h {
        for i in 0..w {
            px(b, x + i, y + j, c);
        }
    }
}

fn fill_ellipse(b: &mut [u32], cx: f64, cy: f64, rx: f64, ry: f64, c: u32) {
    let j_start = -ry.ceil() as i32;
    let j_end = ry.ceil() as i32;
    for j in j_start..=j_end {
        let jf = j as f64;
        let t = (1.0 - (jf * jf) / (ry * ry)).max(0.0);
        let w = (rx * t.sqrt()).round() as i32;
        for i in -w..=w {
            px(
                b,
                (cx + i as f64).round() as i32,
                (cy + jf).round() as i32,
                c,
            );
        }
    }
}

/// `spr()`（agent-monitor.html:261-269）：像素小人贴图，flip 水平翻转。
fn spr(b: &mut [u32], rows: &[&str], x: i32, y: i32, colors: &[(u8, u32)], flip: bool) {
    let w = rows.first().map_or(0, |r| r.len());
    for (j, row) in rows.iter().enumerate() {
        for (i, ch) in row.bytes().enumerate() {
            if ch == b'.' || ch == b' ' {
                continue;
            }
            let c = colors
                .iter()
                .find(|(k, _)| *k == ch)
                .map(|(_, v)| *v)
                .unwrap_or(0xff1a_1c2c);
            let px_x = if flip {
                x + (w - 1 - i) as i32
            } else {
                x + i as i32
            };
            px(b, px_x, y + j as i32, c);
        }
    }
}

fn sprite_color(base: &str, pal: &Pal) -> u32 {
    shade(base, pal.bright, pal.night_f, pal.dusk_f, 0.0)
}

// ============================ 碰撞网格 / A*（agent-monitor.html L705-917） ============================

#[derive(Clone, Copy)]
enum Door {
    South,
    North,
    East,
    West,
}

#[derive(Clone, Copy)]
struct BuildingDef {
    key: &'static str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    roof: &'static str,
    wall: &'static str,
    door: Door,
}

const BUILDING_DEFS: [BuildingDef; 21] = [
    BuildingDef {
        key: "refining",
        x: 40.0,
        y: 62.0,
        w: 86.0,
        h: 60.0,
        roof: "#a78bfa",
        wall: "#f2edf8",
        door: Door::South,
    },
    BuildingDef {
        key: "planning",
        x: 142.0,
        y: 62.0,
        w: 84.0,
        h: 60.0,
        roof: "#38bdf8",
        wall: "#e8f4fb",
        door: Door::South,
    },
    BuildingDef {
        key: "plan-review",
        x: 242.0,
        y: 62.0,
        w: 82.0,
        h: 60.0,
        roof: "#facc15",
        wall: "#fbf5e2",
        door: Door::South,
    },
    BuildingDef {
        key: "design",
        x: 342.0,
        y: 62.0,
        w: 80.0,
        h: 58.0,
        roof: "#c084fc",
        wall: "#f4ecf8",
        door: Door::South,
    },
    BuildingDef {
        key: "conventions",
        x: 560.0,
        y: 62.0,
        w: 72.0,
        h: 58.0,
        roof: "#34d399",
        wall: "#eaf8f0",
        door: Door::South,
    },
    BuildingDef {
        key: "knowledge",
        x: 650.0,
        y: 62.0,
        w: 82.0,
        h: 60.0,
        roof: "#60a5fa",
        wall: "#ecf3fb",
        door: Door::South,
    },
    BuildingDef {
        key: "priority",
        x: 760.0,
        y: 62.0,
        w: 72.0,
        h: 58.0,
        roof: "#f87171",
        wall: "#fcecec",
        door: Door::South,
    },
    BuildingDef {
        key: "pm",
        x: 860.0,
        y: 62.0,
        w: 70.0,
        h: 58.0,
        roof: "#f472b6",
        wall: "#fcedf5",
        door: Door::South,
    },
    BuildingDef {
        key: "split",
        x: 580.0,
        y: 140.0,
        w: 70.0,
        h: 56.0,
        roof: "#14b8a6",
        wall: "#e8faf8",
        door: Door::West,
    },
    BuildingDef {
        key: "closed",
        x: 680.0,
        y: 140.0,
        w: 70.0,
        h: 56.0,
        roof: "#64748b",
        wall: "#eef1f5",
        door: Door::West,
    },
    BuildingDef {
        key: "ready",
        x: 640.0,
        y: 200.0,
        w: 56.0,
        h: 36.0,
        roof: "#94a3b8",
        wall: "#f1f4f8",
        door: Door::South,
    },
    BuildingDef {
        key: "audit",
        x: 40.0,
        y: 300.0,
        w: 70.0,
        h: 78.0,
        roof: "#fb923c",
        wall: "#fdf1e7",
        door: Door::East,
    },
    BuildingDef {
        key: "needs-grilling",
        x: 130.0,
        y: 330.0,
        w: 82.0,
        h: 60.0,
        roof: "#facc15",
        wall: "#fdf6e0",
        door: Door::North,
    },
    BuildingDef {
        key: "idle",
        x: 40.0,
        y: 430.0,
        w: 90.0,
        h: 58.0,
        roof: "#84cc16",
        wall: "#f1faec",
        door: Door::North,
    },
    BuildingDef {
        key: "implementing",
        x: 580.0,
        y: 310.0,
        w: 120.0,
        h: 88.0,
        roof: "#3b82f6",
        wall: "#e8eef7",
        door: Door::West,
    },
    BuildingDef {
        key: "review",
        x: 710.0,
        y: 310.0,
        w: 84.0,
        h: 64.0,
        roof: "#4ade80",
        wall: "#ebfaeb",
        door: Door::West,
    },
    BuildingDef {
        key: "merge",
        x: 820.0,
        y: 310.0,
        w: 72.0,
        h: 64.0,
        roof: "#06b6d4",
        wall: "#e8fafb",
        door: Door::West,
    },
    BuildingDef {
        key: "conflict",
        x: 580.0,
        y: 430.0,
        w: 92.0,
        h: 64.0,
        roof: "#f43f5e",
        wall: "#fdecef",
        door: Door::North,
    },
    BuildingDef {
        key: "done",
        x: 700.0,
        y: 430.0,
        w: 70.0,
        h: 44.0,
        roof: "#84cc16",
        wall: "#f1faec",
        door: Door::South,
    },
    BuildingDef {
        key: "working",
        x: 810.0,
        y: 430.0,
        w: 56.0,
        h: 42.0,
        roof: "#94a3b8",
        wall: "#f1f4f8",
        door: Door::South,
    },
    BuildingDef {
        key: "blocked",
        x: 520.0,
        y: 430.0,
        w: 56.0,
        h: 38.0,
        roof: "#ef4444",
        wall: "#fce9e9",
        door: Door::North,
    },
];

const TREE_SPOTS: [(i32, i32); 35] = [
    (28, 44),
    (48, 36),
    (66, 48),
    (86, 30),
    (106, 44),
    (180, 34),
    (200, 28),
    (218, 44),
    (300, 32),
    (318, 42),
    (520, 30),
    (540, 42),
    (560, 28),
    (620, 34),
    (642, 46),
    (720, 32),
    (740, 40),
    (18, 160),
    (36, 172),
    (18, 220),
    (38, 228),
    (24, 380),
    (44, 392),
    (838, 218),
    (858, 208),
    (878, 226),
    (898, 168),
    (918, 178),
    (900, 470),
    (918, 482),
    (230, 352),
    (250, 344),
    (268, 356),
    (400, 392),
    (420, 384),
];

const NATURE_RECTS: [(f64, f64, f64, f64); 7] = [
    (160.0, 460.0, 130.0, 52.0),
    (850.0, 8.0, 86.0, 44.0),
    (220.0, 390.0, 120.0, 44.0),
    (300.0, 300.0, 76.0, 40.0),
    (790.0, 150.0, 76.0, 44.0),
    (360.0, 420.0, 90.0, 46.0),
    (460.0, 248.0, 40.0, 40.0),
];

fn gx(x: f64) -> usize {
    (x / TILE as f64).floor() as usize
}
fn gy(y: f64) -> usize {
    (y / TILE as f64).floor() as usize
}
fn mark_rect(grid: &mut [u8], x: f64, y: f64, w: f64, h: f64, val: u8) {
    let (x0, y0) = (gx(x), gy(y));
    let (x1, y1) = (gx(x + w - 1.0), gy(y + h - 1.0));
    for j in y0..=y1 {
        for i in x0..=x1 {
            grid[j * GW + i] = val;
        }
    }
}
fn is_walk_px(grid: &[u8], x: i32, y: i32) -> bool {
    grid[gy(y as f64) * GW + gx(x as f64)] == 1
}

fn building_door(bd: &BuildingDef) -> (f64, f64) {
    let cx = bd.x + bd.w / 2.0;
    let cy = bd.y + bd.h / 2.0;
    match bd.door {
        Door::South => (cx.round(), (bd.y + bd.h - 2.0).round()),
        Door::North => (cx.round(), (bd.y + 2.0).round()),
        Door::East => ((bd.x + bd.w - 2.0).round(), cy.round()),
        Door::West => ((bd.x + 2.0).round(), cy.round()),
    }
}

/// 建筑入口（STATION_POS，agent 上工目标点）。
pub fn station_pos(key: &str) -> (f64, f64) {
    for bd in BUILDING_DEFS.iter() {
        if bd.key == key {
            return building_door(bd);
        }
    }
    building_door(&BUILDING_DEFS[19]) // working 综合工位
}

fn build_station_map(grid: &mut [u8]) {
    for bd in BUILDING_DEFS.iter() {
        mark_rect(grid, bd.x - 3.0, bd.y - 3.0, bd.w + 6.0, bd.h + 6.0, 0);
        let (dx, dy) = building_door(bd);
        match bd.door {
            Door::South => mark_rect(grid, dx - 7.0, dy + 1.0, 14.0, 8.0, 1),
            Door::North => mark_rect(grid, dx - 7.0, dy - 9.0, 14.0, 8.0, 1),
            Door::East => mark_rect(grid, dx + 1.0, dy - 7.0, 8.0, 14.0, 1),
            Door::West => mark_rect(grid, dx - 9.0, dy - 7.0, 8.0, 14.0, 1),
        }
    }
}

fn build_nature_map(grid: &mut [u8]) {
    for (x, y, w, h) in NATURE_RECTS {
        mark_rect(grid, x - 2.0, y - 2.0, w + 4.0, h + 4.0, 0);
    }
    for (tx, ty) in TREE_SPOTS {
        mark_rect(grid, tx as f64 - 9.0, ty as f64 - 6.0, 18.0, 14.0, 0);
    }
}

/// `nearestWalkable()`（L823-831）：最近的可行走格中心。
fn nearest_walkable(grid: &[u8], x: f64, y: f64) -> (f64, f64) {
    let (x0, y0) = (gx(x), gy(y));
    let mut best: Option<(usize, usize)> = None;
    let mut best_d = f64::MAX;
    for j in 0..GH {
        for i in 0..GW {
            if grid[j * GW + i] == 0 {
                continue;
            }
            let d = (i as f64 - x0 as f64).powi(2) + (j as f64 - y0 as f64).powi(2);
            if d < best_d {
                best_d = d;
                best = Some((i, j));
            }
        }
    }
    match best {
        Some((i, j)) => (
            i as f64 * TILE as f64 + TILE as f64 / 2.0,
            j as f64 * TILE as f64 + TILE as f64 / 2.0,
        ),
        None => (TOWN_W as f64 / 2.0, TOWN_H as f64 / 2.0),
    }
}

/// A* `findPath()`（L866-917）：8 向、对角需两邻可走、迭代上限 12000。
pub fn find_path(grid: &[u8], sx: f64, sy: f64, tx: f64, ty: f64) -> Vec<(f64, f64)> {
    let sx = sx.clamp(0.0, TOWN_W as f64 - 1.0);
    let sy = sy.clamp(0.0, TOWN_H as f64 - 1.0);
    let tx = tx.clamp(0.0, TOWN_W as f64 - 1.0);
    let ty = ty.clamp(0.0, TOWN_H as f64 - 1.0);
    let (mut gsx, mut gsy) = (gx(sx), gy(sy));
    let (mut gtx, mut gty) = (gx(tx), gy(ty));
    if grid[gsy * GW + gsx] == 0 {
        let n = nearest_walkable(grid, sx, sy);
        gsx = gx(n.0);
        gsy = gy(n.1);
    }
    if grid[gty * GW + gtx] == 0 {
        let n = nearest_walkable(grid, tx, ty);
        gtx = gx(n.0);
        gty = gy(n.1);
    }
    let key = |x: usize, y: usize| y * GW + x;
    let mut came = vec![-1i32; GW * GH];
    let mut g = vec![f64::INFINITY; GW * GH];
    let mut f = vec![f64::INFINITY; GW * GH];
    let mut closed = vec![0u8; GW * GH];
    // 二叉最小堆（MinHeap L835-864）。
    let mut open: std::collections::BinaryHeap<HeapNode> = std::collections::BinaryHeap::new();
    open.push(HeapNode {
        f: (gtx.abs_diff(gsx) + gty.abs_diff(gsy)) as f64,
        x: gsx,
        y: gsy,
        k: key(gsx, gsy),
    });
    g[key(gsx, gsy)] = 0.0;
    f[key(gsx, gsy)] = open.peek().map(|n| n.f).unwrap_or(0.0);
    const DIRS: [(i32, i32); 8] = [
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
    ];
    let mut iter = 0;
    while let Some(cur) = open.pop() {
        if closed[cur.k] != 0 {
            continue;
        }
        if cur.x == gtx && cur.y == gty {
            break;
        }
        closed[cur.k] = 1;
        iter += 1;
        if iter > 12_000 {
            break;
        }
        for (dx, dy) in DIRS {
            let nx_i = cur.x as i32 + dx;
            let ny_i = cur.y as i32 + dy;
            if nx_i < 0 || nx_i >= GW as i32 || ny_i < 0 || ny_i >= GH as i32 {
                continue;
            }
            let (nx, ny) = (nx_i as usize, ny_i as usize);
            let nk = key(nx, ny);
            if closed[nk] != 0 || grid[nk] == 0 {
                continue;
            }
            if dx != 0
                && dy != 0
                && (grid[key((cur.x as i32 + dx) as usize, cur.y)] == 0
                    || grid[key(cur.x, (cur.y as i32 + dy) as usize)] == 0)
            {
                continue;
            }
            let ng = g[cur.k] + if dx != 0 && dy != 0 { 1.414 } else { 1.0 };
            if ng < g[nk] {
                came[nk] = cur.k as i32;
                g[nk] = ng;
                f[nk] = ng + (gtx.abs_diff(nx) + gty.abs_diff(ny)) as f64;
                open.push(HeapNode {
                    f: f[nk],
                    x: nx,
                    y: ny,
                    k: nk,
                });
            }
        }
    }
    if came[key(gtx, gty)] == -1 && !(gsx == gtx && gsy == gty) {
        return Vec::new();
    }
    let mut steps = Vec::new();
    let mut ck = key(gtx, gty) as i32;
    while ck != -1 {
        let ck_u = ck as usize;
        let cx = ck_u % GW;
        let cy = ck_u / GW;
        steps.push((
            cx as f64 * TILE as f64 + TILE as f64 / 2.0,
            cy as f64 * TILE as f64 + TILE as f64 / 2.0,
        ));
        if cx == gsx && cy == gsy {
            break;
        }
        ck = came[ck_u];
    }
    steps.reverse();
    steps
}

/// BinaryHeap 节点（Rust BinaryHeap 是最大堆，比较取反实现最小堆）。
struct HeapNode {
    f: f64,
    x: usize,
    y: usize,
    k: usize,
}
impl PartialEq for HeapNode {
    fn eq(&self, other: &Self) -> bool {
        self.f == other.f
    }
}
impl Eq for HeapNode {}
impl PartialOrd for HeapNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .f
            .partial_cmp(&self.f)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

// ============================ 静态场景绘制（Prototype 验证口径） ============================

fn draw_shadow(b: &mut [u32], x: f64, y: f64, w: f64, h: f64, pal: &Pal, scale: f64) {
    let sx = (x + pal.shadow_dx * scale).round() as i32;
    let sy = (y + pal.shadow_dy * scale).round() as i32;
    rect(b, sx, sy, w as i32, h as i32, pal.shadow_color);
}

fn draw_tree(b: &mut [u32], pal: &Pal, x: i32, y: i32) {
    let v = (x * 7 + y * 13).rem_euclid(3);
    rect(b, x - 2, y - 2, 4, 8, pal.k);
    draw_shadow(b, x as f64 - 8.0, y as f64 + 3.0, 18.0, 8.0, pal, 0.55);
    let hi = sprite_color("#a8e092", pal);
    let drk = sprite_color("#2f6a34", pal);
    if v == 0 {
        rect(b, x - 9, y - 7, 18, 12, pal.g);
        rect(b, x - 7, y - 12, 14, 9, pal.l);
        rect(b, x - 4, y - 16, 8, 6, hi);
        px(b, x - 8, y - 4, drk);
        px(b, x + 8, y - 4, drk);
        if pal.season != SeasonKind::Winter {
            px(b, x - 2, y - 11, pal.r);
        }
    } else if v == 1 {
        rect(b, x - 11, y - 9, 22, 13, pal.g);
        rect(b, x - 9, y - 15, 18, 8, pal.l);
        rect(b, x - 5, y - 19, 10, 5, hi);
        px(b, x - 10, y - 5, drk);
        px(b, x + 10, y - 5, drk);
        if pal.season != SeasonKind::Winter {
            px(b, x - 3, y - 13, pal.r);
            px(b, x + 3, y - 10, pal.r);
        }
    } else {
        rect(b, x - 7, y - 4, 14, 7, pal.g);
        rect(b, x - 6, y - 10, 12, 7, pal.l);
        rect(b, x - 4, y - 16, 8, 7, pal.l);
        rect(b, x - 2, y - 21, 4, 6, hi);
        px(b, x - 7, y - 5, drk);
        px(b, x + 7, y - 5, drk);
    }
}

fn draw_pond(b: &mut [u32], pal: &Pal, x: f64, y: f64, w: f64, h: f64, t: f64) {
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    let rx = w / 2.0;
    let ry = h / 2.0;
    let sand = sprite_color("#d8b26a", pal);
    let deep = sprite_color("#3f6d9c", pal);
    let water = sprite_color("#6aa8f0", pal);
    let light = sprite_color("#8cc8f8", pal);
    fill_ellipse(b, cx, cy, rx + 3.0, ry + 3.0, sand);
    fill_ellipse(b, cx, cy, rx + 1.0, ry + 1.0, sprite_color("#a5c9f0", pal));
    fill_ellipse(b, cx, cy, rx - 1.0, ry - 1.0, deep);
    fill_ellipse(b, cx, cy - 1.0, rx - 4.0, ry - 4.0, water);
    fill_ellipse(
        b,
        cx - 2.0,
        cy - 3.0,
        (rx - 8.0).max(3.0),
        (ry - 7.0).max(2.0),
        light,
    );
    let glow = (t * 1.7).sin() * rx * 0.35;
    px(
        b,
        (cx + glow).round() as i32,
        (cy - ry / 4.0).round() as i32,
        0xffff_f0c0,
    );
    px(
        b,
        (cx - rx / 3.0 + glow * 0.5).round() as i32,
        (cy + ry / 3.0).round() as i32,
        0xffff_f0c0,
    );
    px(
        b,
        (cx + rx / 3.0 - glow * 0.7).round() as i32,
        (cy - ry / 5.0).round() as i32,
        0xffff_f0c0,
    );
    let line_n: i32 = 3;
    let line_color: u32 = 0xbbff_ffff;
    for i in 0..line_n {
        let base_y = cy
            + ((i as f64 - line_n as f64 / 2.0) / line_n as f64) * ry * 1.3
            + (t * 0.9 + i as f64 * 1.7).sin() * 2.0;
        let mut xo = -rx + 5.0;
        while xo < rx - 5.0 {
            let wob = (xo * 0.16 + t * 1.8 + i as f64).sin() * 1.6;
            let py = (base_y + wob).round();
            if py > cy - ry && py < cy + ry {
                px(b, (cx + xo).round() as i32, py as i32, line_color);
            }
            xo += 3.0;
        }
    }
    for i in 0..3 {
        px(
            b,
            (cx + (t * 1.3 + i as f64 * 2.1).sin() * rx * 0.5).round() as i32,
            (cy + (t * 1.1 + i as f64).cos() * ry * 0.5).round() as i32,
            0xffff_f0c0,
        );
    }
    fill_ellipse(
        b,
        (cx - rx * 0.45).round(),
        (cy + ry * 0.25).round(),
        3.0,
        2.0,
        sprite_color("#2f8a3f", pal),
    );
    fill_ellipse(
        b,
        (cx + rx * 0.35).round(),
        (cy - ry * 0.35).round(),
        2.0,
        1.5,
        sprite_color("#2f8a3f", pal),
    );
}

fn draw_grass_tuft(b: &mut [u32], x: i32, y: i32, c1: u32, c2: u32, c3: u32) {
    px(b, x, y, c1);
    px(b, x, y - 1, c1);
    px(b, x + 1, y, c2);
    px(b, x + 2, y - 1, c2);
    px(b, x - 1, y - 2, c3);
    px(b, x + 1, y - 3, c3);
}

fn draw_flower(b: &mut [u32], x: i32, y: i32, petal: u32, center: u32, stem: u32) {
    px(b, x, y + 1, stem);
    px(b, x - 1, y, petal);
    px(b, x + 1, y, petal);
    px(b, x, y - 1, petal);
    px(b, x, y + 1, stem);
    px(b, x, y, center);
}

fn draw_flower_field(b: &mut [u32], pal: &Pal, x: f64, y: f64, w: f64, h: f64) {
    let bg = sprite_color("#3c8c4c", pal);
    let dark = sprite_color("#2c6c3c", pal);
    rect(b, x as i32, y as i32, w as i32, h as i32, bg);
    let petals = [
        pal.p,
        pal.y,
        sprite_color("#ff9fb0", pal),
        sprite_color("#c8a0ff", pal),
        sprite_color("#a0e8ff", pal),
    ];
    let n = (w * h / 24.0).ceil() as i32; // JS: i < w*h/24
    for i in 0..n {
        let fx = x + 3.0 + ((i as f64 * 53.0) % (w - 6.0));
        let fy = y + 3.0 + ((i as f64 * 31.0) % (h - 6.0));
        let petal = petals[i as usize % petals.len()];
        let center = if i % 2 != 0 {
            sprite_color("#fff0a0", pal)
        } else {
            sprite_color("#c05a2a", pal)
        };
        draw_flower(
            b,
            fx.round() as i32,
            fy.round() as i32,
            petal,
            center,
            pal.g,
        );
        if i % 4 == 0 {
            draw_grass_tuft(
                b,
                fx.round() as i32 + 4,
                fy.round() as i32 + 8,
                pal.h,
                pal.g,
                dark,
            );
        }
    }
    for i in 0..(w / 26.0).ceil() as i32 {
        draw_grass_tuft(
            b,
            (x + 4.0 + i as f64 * 26.0) as i32,
            (y + h - 8.0) as i32,
            pal.g,
            pal.h,
            dark,
        );
    }
}

fn draw_farm(b: &mut [u32], pal: &Pal, x: f64, y: f64, w: f64, h: f64) {
    rect(
        b,
        x as i32,
        y as i32,
        w as i32,
        h as i32,
        sprite_color("#a57a42", pal),
    );
    let mut j = 4.0;
    while j < h - 4.0 {
        rect(
            b,
            (x + 3.0) as i32,
            (y + j) as i32,
            (w - 6.0) as i32,
            2,
            sprite_color("#8a5a2f", pal),
        );
        let mut i = 4.0;
        while i < w - 6.0 {
            if (i + j) as i32 % 3 == 0 {
                px(b, (x + i) as i32, (y + j - 3.0) as i32, pal.l);
            }
            i += 9.0;
        }
        j += 7.0;
    }
}

fn draw_building(b: &mut [u32], bd: &BuildingDef, pal: &Pal) {
    let roof = sprite_color(bd.roof, pal);
    let wall = sprite_color(bd.wall, pal);
    let dark = sprite_color("#1a1c2c", pal);
    let roof_dark = sprite_color("#1d2430", pal);
    let win = sprite_color("#ffd858", pal);
    let door = sprite_color("#6b4423", pal);
    let wood = sprite_color("#8a5a2f", pal);
    let glass = sprite_color("#b8e0f0", pal);
    let cx = bd.x + bd.w / 2.0;
    let key = bd.key;
    let bw = bd.w;
    let bh = bd.h;
    let bx = bd.x;
    let by = bd.y;

    draw_shadow(b, bx + 3.0, by + bh - 7.0, bw - 6.0, 12.0, pal, 0.3);

    match key {
        "refining" => {
            rect(
                b,
                (bx + 1.0) as i32,
                (by + 14.0) as i32,
                (bw - 2.0) as i32,
                (bh - 14.0) as i32,
                wall,
            );
            rect(
                b,
                (bx - 4.0) as i32,
                (by + 10.0) as i32,
                10,
                (bh - 10.0) as i32,
                wall,
            );
            rect(
                b,
                (bx + bw - 6.0) as i32,
                (by + 10.0) as i32,
                10,
                (bh - 10.0) as i32,
                wall,
            );
            let rh = 12;
            for i in 0..rh {
                let half = ((bw / 2.0) * ((i + 1) as f64 / rh as f64)).round().max(2.0);
                rect(
                    b,
                    (cx - half).round() as i32,
                    (by + i as f64) as i32,
                    (half * 2.0) as i32,
                    1,
                    if i % 2 != 0 { roof } else { roof_dark },
                );
            }
            rect(
                b,
                (bx + 1.0) as i32,
                (by + 12.0) as i32,
                (bw - 2.0) as i32,
                3,
                roof_dark,
            );
            rect(
                b,
                (bx + 16.0) as i32,
                (by + 20.0) as i32,
                5,
                (bh - 22.0) as i32,
                wood,
            );
            rect(
                b,
                (bx + 30.0) as i32,
                (by + 20.0) as i32,
                5,
                (bh - 22.0) as i32,
                wood,
            );
            rect(
                b,
                (bx + bw - 21.0) as i32,
                (by + 20.0) as i32,
                5,
                (bh - 22.0) as i32,
                wood,
            );
            rect(b, (bx + 8.0) as i32, (by + 24.0) as i32, 8, 6, win);
            rect(b, (bx + bw - 18.0) as i32, (by + 24.0) as i32, 8, 6, win);
        }
        "planning" => {
            fill_ellipse(
                b,
                cx,
                by + (bh * 0.58).round(),
                (bw / 2.0 - 3.0).round(),
                (bh * 0.42).round(),
                wall,
            );
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 10.0) as i32,
                (bw - 4.0) as i32,
                (bh - 10.0) as i32,
                wall,
            );
            rect(
                b,
                (cx - 1.0) as i32,
                (by + 2.0) as i32,
                2,
                (bh - 8.0) as i32,
                dark,
            );
            for i in 0..8 {
                let xa = bx + 12.0 + i as f64 * (bw - 24.0) / 7.0;
                let ya = by + 12.0 + ((i as f64 * 5.0) % 9.0);
                rect(b, xa.round() as i32, ya.round() as i32, 6, 3, glass);
            }
            for i in 0..8 {
                rect(
                    b,
                    (cx - i as f64 * 0.4).round() as i32,
                    (by - 6.0 + i as f64) as i32,
                    2,
                    1,
                    if i < 4 { win } else { roof },
                );
            }
            rect(b, (cx - 1.0).round() as i32, (by - 10.0) as i32, 2, 5, win);
        }
        "plan-review" => {
            rect(
                b,
                bx as i32,
                (by + 10.0) as i32,
                bw as i32,
                (bh - 10.0) as i32,
                wall,
            );
            let rh = 10;
            for i in 0..rh {
                let half = ((bw / 2.0) * ((i + 1) as f64 / rh as f64)).round().max(2.0);
                rect(
                    b,
                    (cx - half).round() as i32,
                    (by + i as f64) as i32,
                    (half * 2.0) as i32,
                    1,
                    if i % 2 != 0 { roof } else { roof_dark },
                );
            }
            rect(
                b,
                (cx - 1.0) as i32,
                (by + 16.0) as i32,
                2,
                (bh - 18.0) as i32,
                wood,
            );
            rect(b, (cx - 8.0) as i32, (by + 18.0) as i32, 16, 2, wood);
            rect(b, (cx - 7.0) as i32, (by + 22.0) as i32, 6, 4, wood);
            rect(b, (cx + 1.0) as i32, (by + 22.0) as i32, 6, 4, wood);
            rect(b, (bx + 8.0) as i32, (by + 26.0) as i32, 8, 6, win);
            rect(b, (bx + bw - 16.0) as i32, (by + 26.0) as i32, 8, 6, win);
        }
        "design" => {
            let rh = (bh * 0.42).round().min(18.0) as i32;
            for i in 0..rh {
                let half = ((bw / 2.0) * ((i + 1) as f64 / rh as f64)).round().max(2.0);
                rect(
                    b,
                    (cx - half).round() as i32,
                    (by + i as f64) as i32,
                    (half * 2.0) as i32,
                    1,
                    if i % 2 != 0 { roof } else { roof_dark },
                );
            }
            for i in 0..(bh - rh as f64) as i32 {
                let half = ((bw / 2.0) * ((bh - rh as f64 - i as f64) / (bh - rh as f64)))
                    .round()
                    .max(2.0);
                rect(
                    b,
                    (cx - half).round() as i32,
                    (by + rh as f64 + i as f64) as i32,
                    (half * 2.0) as i32,
                    1,
                    wall,
                );
            }
            rect(
                b,
                (bx + 6.0) as i32,
                (by + 24.0) as i32,
                (bw - 12.0) as i32,
                10,
                glass,
            );
            px(b, (bx + 10.0) as i32, (by + 28.0) as i32, 0xffff_5050);
            px(b, (bx + 16.0) as i32, (by + 28.0) as i32, 0xff58_a0e8);
            px(b, (bx + 22.0) as i32, (by + 28.0) as i32, 0xff58_d858);
            px(b, (bx + 28.0) as i32, (by + 28.0) as i32, 0xffff_d858);
        }
        "conventions" => {
            rect(
                b,
                bx as i32,
                (by + 10.0) as i32,
                bw as i32,
                (bh - 10.0) as i32,
                wall,
            );
            let rh = 9;
            for i in 0..rh {
                let half = ((bw / 2.0) * ((i + 1) as f64 / rh as f64)).round().max(2.0);
                rect(
                    b,
                    (cx - half).round() as i32,
                    (by + i as f64) as i32,
                    (half * 2.0) as i32,
                    1,
                    if i % 2 != 0 { roof } else { roof_dark },
                );
            }
            for i in 0..4 {
                rect(
                    b,
                    (bx + 6.0 + i as f64 * (bw - 12.0) / 3.0) as i32,
                    (by + 18.0) as i32,
                    6,
                    (bh - 22.0) as i32,
                    wood,
                );
            }
            rect(
                b,
                (bx + 8.0) as i32,
                (by + 22.0) as i32,
                (bw - 16.0) as i32,
                6,
                win,
            );
        }
        "knowledge" => {
            rect(
                b,
                (bx + 4.0) as i32,
                (by + 12.0) as i32,
                (bw - 8.0) as i32,
                (bh - 12.0) as i32,
                wall,
            );
            fill_ellipse(b, cx, by + 8.0, (bw / 2.0 - 2.0).round(), 7.0, pal.g);
            fill_ellipse(b, cx - 6.0, by + 5.0, 5.0, 4.0, pal.l);
            fill_ellipse(b, cx + 6.0, by + 5.0, 5.0, 4.0, pal.l);
            fill_ellipse(b, cx, by + 2.0, 3.0, 3.0, roof);
            rect(
                b,
                (cx - 4.0) as i32,
                (by + 20.0) as i32,
                8,
                (bh - 22.0) as i32,
                wood,
            );
            rect(b, (bx + 14.0) as i32, (by + 24.0) as i32, 8, 6, win);
            rect(b, (bx + bw - 22.0) as i32, (by + 24.0) as i32, 8, 6, win);
        }
        "priority" => {
            for i in 0..bh as i32 {
                let half = (bw / 2.0 * (1.1 - 0.4 * i as f64 / bh)).round();
                let hw = half.max(3.0);
                let w2 = (half * 2.0).max(6.0);
                rect(
                    b,
                    (cx - hw).round() as i32,
                    (by + i as f64) as i32,
                    w2 as i32,
                    1,
                    if i % 6 == 0 {
                        wall
                    } else {
                        sprite_color(bd.wall, pal)
                    },
                );
            }
            rect(b, (cx - 1.0) as i32, (by + 4.0) as i32, 2, 14, dark);
            rect(b, (cx - 1.0) as i32, (by + 4.0) as i32, 14, 6, roof);
            px(b, (cx + 13.0) as i32, (by + 6.0) as i32, roof_dark);
            rect(b, (bx + 10.0) as i32, (by + 24.0) as i32, 8, 6, win);
            rect(b, (bx + bw - 18.0) as i32, (by + 24.0) as i32, 8, 6, win);
        }
        "pm" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 14.0) as i32,
                (bw - 4.0) as i32,
                (bh - 14.0) as i32,
                wall,
            );
            fill_ellipse(b, cx, by + 12.0, (bw / 2.0 - 2.0).round(), 12.0, roof);
            fill_ellipse(b, cx, by + 12.0, (bw / 2.0 - 5.0).round(), 7.0, roof_dark);
            rect(b, (cx - 2.0) as i32, (by - 2.0) as i32, 4, 2, roof_dark);
            rect(b, (bx + 8.0) as i32, (by + 24.0) as i32, 8, 6, win);
            rect(b, (bx + bw - 16.0) as i32, (by + 24.0) as i32, 8, 6, win);
        }
        "split" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 14.0) as i32,
                (bw - 4.0) as i32,
                (bh - 14.0) as i32,
                wall,
            );
            for i in 0..12 {
                rect(
                    b,
                    (bx + 2.0 + i as f64) as i32,
                    (by + 4.0 + (i / 2) as f64) as i32,
                    1,
                    12 - (i / 2),
                    if i % 2 != 0 { roof } else { roof_dark },
                );
                rect(
                    b,
                    (bx + bw - 3.0 - i as f64) as i32,
                    (by + 4.0 + (i / 2) as f64) as i32,
                    1,
                    12 - (i / 2),
                    if i % 2 != 0 { roof_dark } else { roof },
                );
            }
            rect(b, (bx + 8.0) as i32, (by + 22.0) as i32, 8, 6, win);
            rect(b, (bx + bw - 16.0) as i32, (by + 22.0) as i32, 8, 6, win);
        }
        "closed" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 12.0) as i32,
                (bw - 4.0) as i32,
                (bh - 12.0) as i32,
                wall,
            );
            fill_ellipse(b, cx, by + 28.0, (bw / 2.0 - 8.0).round(), 20.0, dark);
            fill_ellipse(b, cx, by + 28.0, (bw / 2.0 - 12.0).round(), 17.0, roof);
            let rings = ["#8a5a2f", "#b07a46", "#e0b06a"];
            for (r, ring) in rings.iter().enumerate() {
                fill_ellipse(
                    b,
                    cx,
                    by + 28.0,
                    ((bw / 2.0 - 12.0) * (r as f64 + 1.0) / 3.2).round(),
                    (17.0 * (r as f64 + 1.0) / 3.4).round(),
                    sprite_color(ring, pal),
                );
            }
        }
        "ready" => {
            rect(
                b,
                (bx + 4.0) as i32,
                (by + 12.0) as i32,
                (bw - 8.0) as i32,
                (bh - 12.0) as i32,
                wall,
            );
            fill_ellipse(b, cx, by + 8.0, (bw / 4.0).round(), 8.0, roof);
            rect(b, (cx - 1.0) as i32, (by - 8.0) as i32, 2, 10, dark);
            px(b, cx as i32, (by - 10.0) as i32, 0xffff_e0a0);
            rect(b, (bx + 10.0) as i32, (by + 24.0) as i32, 8, 6, win);
            rect(b, (bx + bw - 18.0) as i32, (by + 24.0) as i32, 8, 6, win);
        }
        "audit" => {
            let top_y = by + 6.0;
            for i in 0..bh as i32 {
                let t = i as f64 / bh;
                let r = 3.0 + ((bw / 2.0 - 4.0) * (1.0 - (t - 0.5).abs() * 1.4)).round();
                rect(
                    b,
                    (cx - r).round() as i32,
                    (by + i as f64) as i32,
                    (r * 2.0) as i32,
                    1,
                    if i % 3 == 0 { roof_dark } else { wall },
                );
            }
            rect(
                b,
                (cx - 1.0).round() as i32,
                (top_y - 16.0) as i32,
                2,
                18,
                dark,
            );
            px(b, cx as i32, (top_y - 18.0) as i32, 0xffff_e0a0);
            for i in 0..4 {
                rect(
                    b,
                    (cx - 5.0).round() as i32,
                    (by + 18.0 + i as f64 * 12.0) as i32,
                    10,
                    2,
                    glass,
                );
            }
        }
        "needs-grilling" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 14.0) as i32,
                (bw - 4.0) as i32,
                (bh - 14.0) as i32,
                wall,
            );
            fill_ellipse(b, cx, by + 8.0, (bw / 2.0 - 2.0).round(), 8.0, roof);
            fill_ellipse(b, cx, by + 8.0, (bw / 2.0 - 5.0).round(), 4.0, roof_dark);
            rect(
                b,
                (bx + 6.0) as i32,
                (by + 18.0) as i32,
                (bw - 12.0) as i32,
                10,
                glass,
            );
            px(b, (bx + 12.0) as i32, (by + 22.0) as i32, 0xff58_a0e8);
            px(b, (bx + 20.0) as i32, (by + 22.0) as i32, 0xffff_d858);
            px(b, (bx + 28.0) as i32, (by + 22.0) as i32, 0xffff_5050);
            rect(
                b,
                (bx + 14.0) as i32,
                (by + bh - 14.0) as i32,
                (bw - 28.0) as i32,
                4,
                wood,
            );
            rect(b, (bx + 10.0) as i32, (by + bh - 12.0) as i32, 5, 4, wood);
            rect(
                b,
                (bx + bw - 15.0) as i32,
                (by + bh - 12.0) as i32,
                5,
                4,
                wood,
            );
        }
        "idle" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 12.0) as i32,
                (bw - 4.0) as i32,
                (bh - 12.0) as i32,
                wall,
            );
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 2.0) as i32,
                (bw - 4.0) as i32,
                12,
                roof,
            );
            let mut i = 0.0;
            while i < bw - 4.0 {
                px(b, (bx + 2.0 + i) as i32, (by + 1.0) as i32, 0xffff_5050);
                px(
                    b,
                    (bx + 2.0 + i + 3.0) as i32,
                    (by + 1.0) as i32,
                    0xffff_d858,
                );
                px(b, (bx + 2.0 + i) as i32, (by + 7.0) as i32, 0xffff_d858);
                px(
                    b,
                    (bx + 2.0 + i + 3.0) as i32,
                    (by + 7.0) as i32,
                    0xffff_5050,
                );
                i += 6.0;
            }
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 14.0) as i32,
                (bw - 4.0) as i32,
                2,
                roof_dark,
            );
            rect(b, (bx + 8.0) as i32, (by + 20.0) as i32, 8, 6, wood);
            px(b, (bx + 11.0) as i32, (by + 18.0) as i32, win);
            rect(
                b,
                (bx + 14.0) as i32,
                (by + bh - 14.0) as i32,
                (bw - 28.0) as i32,
                4,
                wood,
            );
            rect(b, (bx + 10.0) as i32, (by + bh - 12.0) as i32, 4, 3, wood);
            rect(
                b,
                (bx + bw - 14.0) as i32,
                (by + bh - 12.0) as i32,
                4,
                3,
                wood,
            );
        }
        "implementing" => {
            let n = (bw / 12.0).ceil() as i32;
            for i in 0..n {
                let sx = bx + 2.0 + i as f64 * 12.0;
                rect(b, sx as i32, (by + 2.0) as i32, 10, 3, roof);
                rect(b, (sx + 5.0) as i32, (by + 2.0) as i32, 5, 1, roof_dark);
                rect(b, (sx + 10.0) as i32, (by + 5.0) as i32, 1, 4, roof_dark);
                rect(b, sx as i32, (by + 5.0) as i32, 10, 2, roof_dark);
            }
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 8.0) as i32,
                (bw - 4.0) as i32,
                (bh - 8.0) as i32,
                wall,
            );
            rect(b, (bx + 12.0) as i32, (by - 8.0) as i32, 8, 12, roof_dark);
            rect(b, (bx + 13.0) as i32, (by - 10.0) as i32, 6, 3, roof);
            px(b, (bx + 14.0) as i32, (by - 9.0) as i32, 0xffdd_dddd);
            rect(
                b,
                (bx + 10.0) as i32,
                (by + bh - 10.0) as i32,
                (bw - 20.0) as i32,
                4,
                dark,
            );
            for i in 0..((bw - 20.0) / 10.0) as i32 {
                rect(
                    b,
                    (bx + 12.0 + i as f64 * 10.0) as i32,
                    (by + bh - 9.0) as i32,
                    2,
                    2,
                    roof,
                );
            }
            fill_ellipse(b, bx + 14.0, by + 22.0, 7.0, 7.0, pal.y);
            px(b, (bx + 7.0) as i32, (by + 22.0) as i32, pal.y);
            px(b, (bx + 21.0) as i32, (by + 22.0) as i32, pal.y);
            px(b, (bx + 14.0) as i32, (by + 15.0) as i32, pal.y);
            px(b, (bx + 14.0) as i32, (by + 29.0) as i32, pal.y);
            px(b, (bx + 9.0) as i32, (by + 17.0) as i32, pal.y);
            px(b, (bx + 19.0) as i32, (by + 17.0) as i32, pal.y);
            px(b, (bx + 9.0) as i32, (by + 27.0) as i32, pal.y);
            px(b, (bx + 19.0) as i32, (by + 27.0) as i32, pal.y);
            fill_ellipse(b, bx + 14.0, by + 22.0, 3.0, 3.0, win);
        }
        "review" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 12.0) as i32,
                (bw - 4.0) as i32,
                (bh - 12.0) as i32,
                wall,
            );
            fill_ellipse(b, cx, by + 28.0, (bw / 2.0 - 16.0).round(), 18.0, pal.g);
            fill_ellipse(b, cx, by + 28.0, (bw / 2.0 - 20.0).round(), 14.0, win);
            fill_ellipse(b, cx, by + 28.0, (bw / 2.0 - 24.0).round(), 10.0, pal.g);
            fill_ellipse(b, cx, by + 28.0, 3.0, 3.0, dark);
            rect(b, (bx + 10.0) as i32, (by + 24.0) as i32, 8, 6, win);
        }
        "merge" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 12.0) as i32,
                (bw - 4.0) as i32,
                (bh - 12.0) as i32,
                wall,
            );
            for i in 0..10 {
                rect(
                    b,
                    (bx + 4.0 + i as f64) as i32,
                    (by + 3.0 + (i / 2) as f64) as i32,
                    (bw - 8.0 - i as f64 * 2.0) as i32,
                    1,
                    if i % 2 != 0 { roof } else { roof_dark },
                );
            }
            rect(b, (cx - 8.0) as i32, (by + 14.0) as i32, 16, 14, glass);
            rect(b, (cx - 10.0) as i32, (by + 28.0) as i32, 20, 5, dark);
            rect(b, (bx + 4.0) as i32, (by + 20.0) as i32, 10, 4, roof);
            rect(b, (bx + bw - 14.0) as i32, (by + 20.0) as i32, 10, 4, roof);
        }
        "conflict" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 16.0) as i32,
                (bw - 4.0) as i32,
                (bh - 16.0) as i32,
                wall,
            );
            for i in 0..10 {
                let half = ((bw / 2.0) * ((i + 1) as f64 / 10.0)).round().max(2.0);
                rect(
                    b,
                    (cx - half).round() as i32,
                    (by + i as f64) as i32,
                    (half * 2.0) as i32,
                    1,
                    if i % 2 != 0 { roof } else { roof_dark },
                );
            }
            px(b, cx as i32, (by + 6.0) as i32, win);
            px(b, cx as i32, (by + 10.0) as i32, win);
            px(b, cx as i32, (by + 14.0) as i32, win);
            rect(b, (bx + 8.0) as i32, (by + 26.0) as i32, 8, 6, win);
            rect(b, (bx + bw - 16.0) as i32, (by + 26.0) as i32, 8, 6, win);
        }
        "done" => {
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 12.0) as i32,
                10,
                (bh - 12.0) as i32,
                wall,
            );
            rect(
                b,
                (bx + bw - 12.0) as i32,
                (by + 12.0) as i32,
                10,
                (bh - 12.0) as i32,
                wall,
            );
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 12.0) as i32,
                (bw - 4.0) as i32,
                12,
                roof,
            );
            fill_ellipse(b, cx, by + 12.0, (bw / 2.0 - 2.0).round(), 7.0, roof);
            fill_ellipse(b, cx, by + 18.0, (bw / 2.0 - 14.0).round(), 6.0, wall);
            rect(
                b,
                (cx - 7.0) as i32,
                (by + 26.0) as i32,
                14,
                (bh - 28.0) as i32,
                wall,
            );
        }
        "working" => {
            rect(
                b,
                (bx + 4.0) as i32,
                (by + 12.0) as i32,
                (bw - 8.0) as i32,
                (bh - 12.0) as i32,
                wall,
            );
            fill_ellipse(b, cx, by + 8.0, (bw / 4.0).round(), 6.0, roof);
            fill_ellipse(b, cx, by + 25.0, (bw / 4.0 - 1.0).round(), 8.0, pal.y);
            fill_ellipse(b, cx, by + 25.0, 3.0, 4.0, wall);
            px(b, (cx - 7.0) as i32, (by + 25.0) as i32, pal.y);
            px(b, (cx + 7.0) as i32, (by + 25.0) as i32, pal.y);
            px(b, cx as i32, (by + 18.0) as i32, pal.y);
            px(b, cx as i32, (by + 32.0) as i32, pal.y);
        }
        "blocked" => {
            let fx = bx + 2.0;
            let fy = by + 2.0;
            let fw = bw - 4.0;
            let fh = bh - 4.0;
            for i in 0..(fw / 10.0).ceil() as i32 {
                rect(
                    b,
                    (fx + i as f64 * 10.0) as i32,
                    fy as i32,
                    3,
                    fh as i32,
                    roof_dark,
                );
                rect(b, (fx + i as f64 * 10.0) as i32, fy as i32, 3, 4, roof);
            }
            rect(b, fx as i32, (fy + 2.0) as i32, fw as i32, 2, roof_dark);
            rect(
                b,
                fx as i32,
                (fy + fh - 4.0) as i32,
                fw as i32,
                2,
                roof_dark,
            );
            let mut i = 0;
            while i < fw as i32 {
                px(b, (fx + i as f64) as i32, (fy + 2.0) as i32, 0xffff_5050);
                px(
                    b,
                    (fx + i as f64 + 2.0) as i32,
                    (fy + 2.0) as i32,
                    0xffff_d858,
                );
                px(
                    b,
                    (fx + i as f64) as i32,
                    (fy + fh - 4.0) as i32,
                    0xffff_d858,
                );
                px(
                    b,
                    (fx + i as f64 + 2.0) as i32,
                    (fy + fh - 4.0) as i32,
                    0xffff_5050,
                );
                i += 6;
            }
        }
        _ => {
            rect(b, bx as i32, by as i32, bw as i32, bh as i32, wall);
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 2.0) as i32,
                (bw - 4.0) as i32,
                12,
                roof,
            );
            rect(
                b,
                (bx + 2.0) as i32,
                (by + 14.0) as i32,
                (bw - 4.0) as i32,
                3,
                roof_dark,
            );
        }
    }

    if key != "blocked" {
        match bd.door {
            Door::South => rect(b, (cx - 5.0) as i32, (by + bh - 13.0) as i32, 10, 12, door),
            Door::North => rect(b, (cx - 5.0) as i32, (by + 1.0) as i32, 10, 12, door),
            Door::East => rect(
                b,
                (bx + bw - 12.0) as i32,
                (by + bh / 2.0 - 5.0) as i32,
                11,
                10,
                door,
            ),
            Door::West => rect(
                b,
                (bx + 1.0) as i32,
                (by + bh / 2.0 - 5.0) as i32,
                11,
                10,
                door,
            ),
        }
    }
}

fn draw_fountain(b: &mut [u32], pal: &Pal, cx: f64, cy: f64, t: f64) {
    let stone = sprite_color("#cbb89a", pal);
    let stone_dark = sprite_color("#a08b70", pal);
    let water = sprite_color("#6aa8f0", pal);
    let water_light = sprite_color("#8cc8f8", pal);
    let pedestal = sprite_color("#b8a88a", pal);
    let rim = sprite_color("#d8d0c0", pal);
    fill_ellipse(b, cx, cy, 27.0, 21.0, stone);
    fill_ellipse(b, cx, cy, 24.0, 18.0, stone_dark);
    fill_ellipse(b, cx, cy - 1.0, 21.0, 14.0, water);
    fill_ellipse(b, cx, cy - 3.0, 16.0, 10.0, water_light);
    let wob = (t * 2.2).sin() * 2.0;
    px(
        b,
        (cx + 6.0 + wob).round() as i32,
        (cy - 2.0).round() as i32,
        0xffff_ffff,
    );
    px(
        b,
        (cx - 6.0 - wob).round() as i32,
        (cy + 3.0).round() as i32,
        0xffff_ffff,
    );
    rect(b, (cx - 2.0) as i32, (cy - 19.0) as i32, 4, 14, water_light);
    rect(b, (cx - 1.0) as i32, (cy - 26.0) as i32, 2, 9, water);
    rect(b, cx as i32, (cy - 28.0) as i32, 1, 5, 0xffff_ffff);
    for i in 0..8 {
        let ph = (t * 1.1 + i as f64 * 0.31) % 1.0;
        let dir = if i % 2 != 0 { 1.0 } else { -1.0 };
        let dx = dir * (3.0 + ph * 13.0);
        let dy = -22.0 + (ph * std::f64::consts::PI).sin() * 20.0;
        let size = if ph < 0.45 { 1 } else { 0 };
        if size != 0 {
            let c = if i % 3 != 0 { 0xffff_ffff } else { 0xffdf_f5ff };
            px(b, (cx + dx).round() as i32, (cy + dy).round() as i32, c);
            if i % 2 != 0 {
                px(
                    b,
                    (cx + dx + 1.0).round() as i32,
                    (cy + dy).round() as i32,
                    0xffff_ffff,
                );
            }
        }
    }
    rect(b, (cx - 3.0) as i32, (cy - 7.0) as i32, 6, 9, pedestal);
    rect(b, (cx - 2.0) as i32, (cy - 13.0) as i32, 4, 7, rim);
    for i in 0..3 {
        let ph = (t * 0.7 + i as f64 * 0.37) % 1.0;
        let rr = 3.0 + ph * 18.0;
        let alpha = if ph < 0.8 { 0xffff_ffffu32 } else { 0 };
        if alpha != 0 {
            px(b, (cx + rr).round() as i32, (cy - 2.0) as i32, alpha);
            px(b, (cx - rr).round() as i32, (cy - 2.0) as i32, alpha);
            px(b, cx as i32, (cy - 2.0 + rr * 0.45).round() as i32, alpha);
            px(b, cx as i32, (cy - 2.0 - rr * 0.45).round() as i32, alpha);
        }
    }
}

fn draw_scene(b: &mut [u32], grid: &[u8], pal: &Pal, t: f64) {
    rect(b, 0, 0, TOWN_W as i32, TOWN_H as i32, pal.m);
    let grass_dark = sprite_color("#3c8c4c", pal);
    let grass_light = sprite_color("#8cd890", pal);
    for i in 0..32 {
        let gx2 = (i * 83) % (TOWN_W as i32 - 30);
        let gy2 = 10 + ((i * 67) % (TOWN_H as i32 - 20));
        rect(
            b,
            gx2,
            gy2,
            22 + (i % 3) * 7,
            7 + (i % 2) * 4,
            if i % 2 != 0 { grass_dark } else { grass_light },
        );
    }
    for i in 0..140 {
        let gx2 = (i * 73) % TOWN_W as i32;
        let gy2 = 8 + ((i * 41) % (TOWN_H as i32 - 16));
        if !is_walk_px(grid, gx2, gy2) {
            continue;
        }
        let c1 = if i % 3 == 0 { pal.h } else { pal.l };
        let c2 = if i % 4 == 0 { pal.g } else { pal.l };
        draw_grass_tuft(b, gx2, gy2, c1, c2, pal.h);
    }
    let road = sprite_color("#c9b18e", pal);
    let road_dark = sprite_color("#8f7352", pal);
    rect(b, 0, 248, TOWN_W as i32, 34, road);
    for i in 0..170 {
        let rx = (i * 37) % TOWN_W as i32;
        let ry = 252 + ((i * 17) % 26);
        px(b, rx, ry, road_dark);
    }
    rect(b, 456, 0, 48, TOWN_H as i32, road);
    for i in 0..140 {
        let rx = 460 + ((i * 23) % 40);
        let ry = (i * 53) % TOWN_H as i32;
        px(b, rx, ry, road_dark);
    }
    rect(b, 396, 180, 168, 176, road);
    for i in 0..90 {
        let rx = 400 + ((i * 29) % 160);
        let ry = 184 + ((i * 47) % 168);
        px(b, rx, ry, road_dark);
    }
    draw_fountain(b, pal, 480.0, 268.0, t);
    draw_pond(b, pal, 160.0, 460.0, 130.0, 52.0, t);
    draw_pond(b, pal, 850.0, 8.0, 86.0, 44.0, t);
    draw_flower_field(b, pal, 220.0, 390.0, 120.0, 44.0);
    draw_flower_field(b, pal, 300.0, 300.0, 76.0, 40.0);
    draw_flower_field(b, pal, 790.0, 150.0, 76.0, 44.0);
    draw_farm(b, pal, 360.0, 420.0, 90.0, 46.0);
    for (tx, ty) in TREE_SPOTS {
        draw_tree(b, pal, tx, ty);
    }
    for i in 0..90 {
        let fx = (i * 89) % TOWN_W as i32;
        let fy = 10 + ((i * 59) % (TOWN_H as i32 - 20));
        if is_walk_px(grid, fx, fy) {
            let petal = if i % 3 == 0 {
                pal.p
            } else if i % 3 == 1 {
                pal.y
            } else {
                sprite_color("#ff9fb0", pal)
            };
            let center = if i % 2 != 0 {
                sprite_color("#fff0a0", pal)
            } else {
                sprite_color("#c05a2a", pal)
            };
            draw_flower(b, fx, fy, petal, center, pal.l);
        }
    }
}

fn draw_stations(b: &mut [u32], pal: &Pal) {
    for bd in BUILDING_DEFS.iter() {
        draw_building(b, bd, pal);
    }
}

// ============================ NPC 状态与行为（agent-monitor.html L1668-1895） ============================

pub const VILLAGER_PALETTES: [&str; 10] = [
    "#e85d3a", "#f4a7b9", "#58a858", "#5c8af0", "#f0c858", "#b06ad0", "#4ad0b0", "#e07050",
    "#70c8e0", "#d080c0",
];
pub const SCARF_COLORS: [&str; 8] = [
    "#e85d3a", "#4a8ee8", "#58b858", "#f0c858", "#d06ad0", "#e07050", "#70c8e0", "#d080c0",
];
pub const HAIR_COLORS: [&str; 5] = ["#5a3a26", "#2a2a3a", "#e0a050", "#000000", "#6a3a50"];
pub const PANTS_COLORS: [&str; 5] = ["#2f3a4a", "#4a3a5a", "#3a5a3a", "#5a3a3a", "#3a3a6a"];
pub const SHOE_COLORS: [&str; 4] = ["#20242b", "#402020", "#204020", "#202040"];

/// 真实 agent NPC 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpcState {
    Idle,
    Walk,
    ToWork,
    Working,
}

impl NpcState {
    /// 详情 pane「动作」字段（agent-monitor.html showDetail 口径）。
    pub fn action_text(&self) -> &'static str {
        match self {
            NpcState::Working => "正在工位",
            NpcState::ToWork => "前往工位",
            NpcState::Walk => "小镇游走",
            NpcState::Idle => "空闲",
        }
    }
}

/// 真实 agent NPC（`/agents` 条目映射 + 位置/行为状态）。
#[derive(Debug, Clone)]
pub struct TownNpc {
    pub sid: SessionId,
    pub key: String,
    pub entry: AgentRosterEntry,
    pub cx: f64,
    pub cy: f64,
    pub tx: f64,
    pub ty: f64,
    pub path: Vec<(f64, f64)>,
    pub frame: f64,
    pub moving: bool,
    pub working: bool,
    pub cheering: bool,
    /// 加油动效复位时刻（HTML cheer 900ms setTimeout 口径；0=不活动）。
    pub cheer_until_ms: f64,
    pub assigned: bool,
    pub state: NpcState,
    pub state_t: f64,
    pub dir: f64,
    pub palette: usize,
    pub hat_style: usize,
    pub scarf_color: usize,
    pub hat_color: usize,
    pub hair_color: usize,
    pub pants_color: usize,
    pub shoe_color: usize,
}

/// 装饰角色类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasserKind {
    Human,
    Pokemon,
    Bird,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasserState {
    Idle,
    Walk,
    Chat,
    Wave,
    Hop,
    Follow,
    Sparkle,
    Fly,
}

#[derive(Debug, Clone)]
pub struct Passer {
    pub kind: PasserKind,
    pub ptype: &'static str,
    pub name: &'static str,
    pub speed: f64,
    pub cx: f64,
    pub cy: f64,
    pub tx: f64,
    pub ty: f64,
    pub path: Vec<(f64, f64)>,
    pub frame: f64,
    pub state: PasserState,
    pub state_t: f64,
    pub dir: f64,
    pub hop_t: f64,
    pub chat_with: Option<usize>,
    pub clothes: usize,
    pub hair: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticleKind {
    Petal,
    Rain,
    Leaf,
    Snow,
}

#[derive(Debug, Clone)]
struct Particle {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    tw: f64,
    kind: ParticleKind,
}

/// 确定性 xorshift64（不新增 rand 依赖；seed 固定 → 装饰居民布局可复现）。
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn usize(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

// ============================ TownScene ============================

/// 小镇场景状态（REQ-009 §5 `TownScene`/`NpcSprite`）。
pub struct TownScene {
    pub walk_grid: Vec<u8>,
    pub npcs: Vec<TownNpc>,
    pub passers: Vec<Passer>,
    particles: Vec<Particle>,
    last_season: Option<SeasonKind>,
    pub now_ms: f64,
    last_tick_ms: f64,
    rng: Rng,
}

impl TownScene {
    pub fn new(seed: u64, now_ms: f64) -> Self {
        let mut walk_grid = vec![1u8; GW * GH];
        build_station_map(&mut walk_grid);
        build_nature_map(&mut walk_grid);
        let rng = Rng(seed.max(1));
        let mut scene = Self {
            walk_grid,
            npcs: Vec::new(),
            passers: Vec::new(),
            particles: Vec::new(),
            last_season: None,
            now_ms,
            last_tick_ms: now_ms,
            rng,
        };
        scene.spawn_passers();
        scene
    }

    /// 装饰居民（design-spec §2/§7：16–20 名，5 职业路人 + 2 小孩 +
    /// 7 小精灵/动物 + 3 鸟 + 1 猫 = 18）。
    fn spawn_passers(&mut self) {
        self.passers.clear();
        let humans: [(&'static str, &'static str, &'static str, f64); 7] = [
            ("human", "adult", "木匠阿良", 26.0),
            ("human", "adult", "花匠小满", 24.0),
            ("human", "adult", "铁匠大锤", 23.0),
            ("human", "adult", "渔夫老黄", 27.0),
            ("human", "adult", "药师小铃", 25.0),
            ("human", "child", "豆豆", 30.0),
            ("human", "child", "米粒", 28.0),
        ];
        let spirits: [(&'static str, &'static str); 7] = [
            ("pokemon1", "皮皮"),
            ("pokemon2", "水灵"),
            ("pokemon3", "草芽"),
            ("pokemon4", "虫虫"),
            ("pokemon5", "雪球"),
            ("pokemon6", "啾啾"),
            ("cat", "橘猫"),
        ];
        for (kind, ptype, name, speed) in humans {
            let start = self.pick_town_point();
            let mut p = Passer {
                kind: PasserKind::Human,
                ptype,
                name,
                speed,
                cx: start.0,
                cy: start.1,
                tx: start.0,
                ty: start.1,
                path: Vec::new(),
                frame: 0.0,
                state: PasserState::Idle,
                state_t: 1.0 + self.rng.f64() * 2.0,
                dir: 1.0,
                hop_t: 0.0,
                chat_with: None,
                clothes: self.rng.usize(VILLAGER_PALETTES.len()),
                hair: self.rng.usize(HAIR_COLORS.len()),
            };
            let _ = kind;
            p.state_t = 1.0 + self.rng.f64() * 2.0;
            self.passers.push(p);
        }
        for (ptype, name) in spirits {
            let start = self.pick_town_point();
            self.passers.push(Passer {
                kind: PasserKind::Pokemon,
                ptype,
                name,
                speed: 26.0,
                cx: start.0,
                cy: start.1,
                tx: start.0,
                ty: start.1,
                path: Vec::new(),
                frame: 0.0,
                state: PasserState::Idle,
                state_t: 1.0 + self.rng.f64() * 2.0,
                dir: 1.0,
                hop_t: 0.0,
                chat_with: None,
                clothes: 0,
                hair: 0,
            });
        }
        for i in 0..3 {
            self.passers.push(Passer {
                kind: PasserKind::Bird,
                ptype: "bird",
                name: "小鸟",
                speed: 50.0,
                cx: -20.0,
                cy: 10.0 + i as f64 * 12.0,
                tx: 0.0,
                ty: 0.0,
                path: Vec::new(),
                frame: i as f64,
                state: PasserState::Fly,
                state_t: 0.0,
                dir: 1.0,
                hop_t: 0.0,
                chat_with: None,
                clothes: 0,
                hair: 0,
            });
        }
    }

    /// `pickTownPoint()`（L1706-1711）。
    fn pick_town_point(&mut self) -> (f64, f64) {
        let (x, y) = {
            let grid = &self.walk_grid;
            Self::pick_town_point_static(grid, &mut self.rng)
        };
        (x, y)
    }

    /// `/agents` 轮询 → 场景 NPC 同步（poll() L2022-2037 口径）：
    /// 新会话入镇（出生在中央广场）、同 sessionId 原位更新、消失移除。
    /// 阶段变更重置上工目标。
    pub fn sync_agents(&mut self, entries: &[AgentRosterEntry]) {
        let mut seen: std::collections::HashSet<SessionId> = std::collections::HashSet::new();
        let mut keys: HashMap<SessionId, String> = HashMap::new();
        for entry in entries {
            seen.insert(entry.session_id.clone());
            let key = entry.stage_key().to_string();
            let npc = match self.npcs.iter_mut().find(|n| n.sid == entry.session_id) {
                Some(n) => n,
                None => {
                    let palette = self.rng.usize(VILLAGER_PALETTES.len());
                    let npc = TownNpc {
                        sid: entry.session_id.clone(),
                        key: key.clone(),
                        entry: entry.clone(),
                        cx: 480.0,
                        cy: 300.0,
                        tx: 480.0,
                        ty: 300.0,
                        path: Vec::new(),
                        frame: 0.0,
                        moving: false,
                        working: false,
                        cheering: false,
                        cheer_until_ms: 0.0,
                        assigned: false,
                        state: NpcState::Idle,
                        state_t: 0.0,
                        dir: 1.0,
                        palette,
                        hat_style: self.rng.usize(3),
                        scarf_color: self.rng.usize(SCARF_COLORS.len()),
                        hat_color: self.rng.usize(SCARF_COLORS.len()),
                        hair_color: self.rng.usize(HAIR_COLORS.len()),
                        pants_color: self.rng.usize(PANTS_COLORS.len()),
                        shoe_color: self.rng.usize(SHOE_COLORS.len()),
                    };
                    self.npcs.push(npc);
                    self.npcs.last_mut().unwrap()
                }
            };
            if npc.key != key {
                npc.key = key.clone();
                npc.assigned = false; // 阶段变更 → 重新规划上工路线
            }
            npc.entry = entry.clone();
            keys.insert(entry.session_id.clone(), key);
        }
        self.npcs.retain(|n| seen.contains(&n.sid));
        let _ = keys;
    }

    /// 按 sessionId 查找 NPC。
    pub fn npc(&self, sid: &SessionId) -> Option<&TownNpc> {
        self.npcs.iter().find(|n| &n.sid == sid)
    }

    pub fn npc_mut(&mut self, sid: &SessionId) -> Option<&mut TownNpc> {
        self.npcs.iter_mut().find(|n| &n.sid == sid)
    }

    /// 当前季节（palNow 口径）。
    pub fn season(&self) -> SeasonKind {
        let t = self.now_ms + TIME_OFFSET;
        let day = (t / DAY_MS).floor();
        SEASONS[(day / SEASON_DAYS).floor() as usize % SEASONS.len()].kind
    }

    /// 当前分钟数（时段展示）。
    pub fn min_of_day(&self) -> f64 {
        let t = self.now_ms + TIME_OFFSET;
        ((t % DAY_MS) / DAY_MS) * 1440.0
    }

    /// 推进时间并更新行为（tick 的 update 半段）。
    pub fn advance(&mut self, now_ms: f64) {
        let dt = ((now_ms - self.last_tick_ms) / 1000.0).clamp(0.0, 0.05);
        self.last_tick_ms = now_ms;
        self.now_ms = now_ms;
        Self::update_npcs(
            &mut self.npcs,
            &self.walk_grid,
            &mut self.rng,
            dt,
            self.now_ms,
        );
        Self::update_passers_impl(
            &mut self.passers,
            &self.walk_grid,
            &self.npcs,
            &mut self.rng,
            dt,
        );
        self.separate_all();
        self.advance_particles(dt);
    }

    /// `updatePassers()`（L1738-1787）。静态函数：字段分离借用（passers/grid/
    /// npcs/rng 各自独立），避免 &mut self 嵌套借用冲突。
    fn update_passers_impl(
        passers: &mut [Passer],
        grid: &[u8],
        npcs: &[TownNpc],
        rng: &mut Rng,
        dt: f64,
    ) {
        for i in 0..passers.len() {
            // 快照其他角色（避借用冲突）：(kind, state, cx, cy)。
            let others: Vec<(PasserKind, PasserState, f64, f64)> = passers
                .iter()
                .enumerate()
                .filter(|(q, _)| *q != i)
                .map(|(_, p)| (p.kind, p.state, p.cx, p.cy))
                .collect();
            let agents_pos: Vec<(f64, f64)> = npcs.iter().map(|a| (a.cx, a.cy)).collect();
            let p = &mut passers[i];
            match p.kind {
                PasserKind::Bird => {
                    p.frame += dt * 6.0;
                    if p.dir > 0.0 {
                        p.cx += p.speed * dt;
                        if p.cx > TOWN_W as f64 + 20.0 {
                            p.cx = -20.0;
                            p.cy = 8.0 + rng.f64() * 30.0;
                            p.dir = -1.0;
                        }
                    } else {
                        p.cx -= p.speed * dt;
                        if p.cx < -20.0 {
                            p.cx = TOWN_W as f64 + 20.0;
                            p.cy = 8.0 + rng.f64() * 30.0;
                            p.dir = 1.0;
                        }
                    }
                    p.cy = p.cy.clamp(4.0, 42.0) + (p.frame * 2.0).sin() * dt * 3.0;
                    p.cy = p.cy.clamp(4.0, 42.0);
                }
                PasserKind::Human => {
                    p.state_t -= dt;
                    if p.state == PasserState::Walk && p.path.is_empty() {
                        p.state = PasserState::Idle;
                        p.state_t = 1.5 + rng.f64() * 3.0;
                    }
                    if p.state == PasserState::Walk {
                        move_along_path_passer(p, dt, p.speed);
                        if p.path.is_empty() {
                            p.state = PasserState::Idle;
                            p.state_t = 1.5 + rng.f64() * 3.0;
                        }
                    } else if p.state == PasserState::Idle && p.state_t <= 0.0 {
                        let target = Self::pick_town_point_static(grid, rng);
                        let path = find_path(grid, p.cx, p.cy, target.0, target.1);
                        if !path.is_empty() {
                            p.state = PasserState::Walk;
                            p.state_t = 4.0 + rng.f64() * 5.0;
                            p.tx = target.0;
                            p.ty = target.1;
                            p.path = path;
                        } else {
                            p.state_t = 1.0 + rng.f64() * 2.0;
                        }
                    } else if p.state == PasserState::Idle {
                        let near = others.iter().any(|(k, st, x, y)| {
                            *k == PasserKind::Human
                                && *st == PasserState::Idle
                                && ((x - p.cx).powi(2) + (y - p.cy).powi(2)).sqrt() < 20.0
                        });
                        if near && rng.f64() < dt * 0.25 {
                            p.state = PasserState::Chat;
                            p.state_t = 1.6 + rng.f64() * 1.2;
                        }
                    } else if p.state == PasserState::Chat && p.state_t <= 0.0 {
                        p.state = PasserState::Idle;
                        p.state_t = 1.0 + rng.f64() * 2.0;
                    } else if p.state == PasserState::Wave && p.state_t <= 0.0 {
                        p.state = PasserState::Idle;
                    }
                }
                PasserKind::Pokemon => {
                    if p.state == PasserState::Sparkle {
                        p.state_t -= dt;
                        p.hop_t -= dt;
                        if p.state_t <= 0.0 {
                            p.state = PasserState::Idle;
                        }
                        continue;
                    }
                    // 跟随最近的人类/真实 agent（70px 半径）。
                    let mut near: Option<(f64, f64)> = None;
                    let mut near_d = 70.0f64;
                    for (k, _st, x, y) in others.iter() {
                        if *k != PasserKind::Human {
                            continue;
                        }
                        let d = ((x - p.cx).powi(2) + (y - p.cy).powi(2)).sqrt();
                        if d < near_d {
                            near = Some((*x, *y));
                            near_d = d;
                        }
                    }
                    for (ax, ay) in agents_pos.iter() {
                        let d = ((ax - p.cx).powi(2) + (ay - p.cy).powi(2)).sqrt();
                        if d < near_d {
                            near = Some((*ax, *ay));
                            near_d = d;
                        }
                    }
                    if let Some((nx, ny)) = near {
                        let dx = nx - p.cx;
                        let dy = ny - p.cy;
                        let dist = (dx * dx + dy * dy).sqrt();
                        if dist > 18.0 {
                            let step = (p.speed * 2.0).min(dist) * dt;
                            p.cx += (dx / dist) * step;
                            p.cy += (dy / dist) * step;
                            p.frame += dt * 6.0;
                            p.dir = if dx >= 0.0 { 1.0 } else { -1.0 };
                            p.state = PasserState::Follow;
                        } else {
                            p.state = PasserState::Idle;
                            p.frame += dt * 4.0;
                        }
                    } else {
                        p.state_t -= dt;
                        if p.state_t <= 0.0 {
                            p.state_t = 1.0 + rng.f64() * 2.0;
                            p.state = PasserState::Hop;
                            p.hop_t = 0.35;
                        }
                        if p.state == PasserState::Hop {
                            p.hop_t -= dt;
                            if p.hop_t <= 0.0 {
                                p.state = PasserState::Idle;
                            }
                        }
                    }
                }
            }
        }
    }

    /// `updateAgent()`（L1839-1871）静态版。
    fn update_npcs(npcs: &mut [TownNpc], grid: &[u8], rng: &mut Rng, dt: f64, now_ms: f64) {
        for npc in npcs.iter_mut() {
            // 加油动效 900ms 复位（HTML cheer setTimeout 口径）。
            if npc.cheering && npc.cheer_until_ms > 0.0 && now_ms >= npc.cheer_until_ms {
                npc.cheering = false;
                npc.cheer_until_ms = 0.0;
            }
            npc.working = npc.entry.status != AgentStatus::Idle && npc.key != "idle";
            if npc.working && !npc.assigned {
                let pos = station_pos(&npc.key);
                npc.tx = pos.0;
                npc.ty = pos.1;
                npc.path = find_path(grid, npc.cx, npc.cy, npc.tx, npc.ty);
                npc.assigned = true;
                npc.state = NpcState::ToWork;
                if npc.path.is_empty() {
                    npc.state = NpcState::Working;
                }
            }
            if npc.working && npc.assigned {
                if npc.state != NpcState::Working && !npc.path.is_empty() {
                    move_along_path(npc, dt, 30.0);
                    if npc.path.is_empty() {
                        npc.state = NpcState::Working;
                        npc.moving = false;
                        npc.cx = npc.tx;
                        npc.cy = npc.ty;
                    }
                } else {
                    npc.state = NpcState::Working;
                    npc.moving = false;
                }
            } else {
                npc.assigned = false;
                npc.path = Vec::new();
                npc.state_t -= dt;
                if npc.state == NpcState::ToWork || npc.state == NpcState::Working {
                    npc.state = NpcState::Idle;
                    npc.state_t = 1.0 + rng.f64() * 2.0;
                }
                if npc.state == NpcState::Idle && npc.state_t <= 0.0 {
                    let target = Self::pick_town_point_static(grid, rng);
                    npc.path = find_path(grid, npc.cx, npc.cy, target.0, target.1);
                    if !npc.path.is_empty() {
                        npc.state = NpcState::Walk;
                        npc.state_t = 5.0 + rng.f64() * 4.0;
                        npc.tx = target.0;
                        npc.ty = target.1;
                    } else {
                        npc.state_t = 1.0 + rng.f64() * 2.0;
                    }
                } else if npc.state == NpcState::Walk {
                    if !npc.path.is_empty() {
                        move_along_path(npc, dt, 26.0);
                    } else {
                        npc.state = NpcState::Idle;
                        npc.state_t = 1.5 + rng.f64() * 3.0;
                    }
                }
            }
        }
    }

    /// `pickTownPoint()`（L1706-1711）静态版。
    fn pick_town_point_static(grid: &[u8], rng: &mut Rng) -> (f64, f64) {
        for _ in 0..40 {
            let x = 30.0 + rng.f64() * (TOWN_W as f64 - 60.0);
            let y = 40.0 + rng.f64() * (TOWN_H as f64 - 70.0);
            if is_walk_px(grid, x as i32, y as i32) {
                return (x, y);
            }
        }
        (TOWN_W as f64 / 2.0, TOWN_H as f64 / 2.0)
    }

    /// `separateGround()`（L1723-1736）：8px 分离力（design-spec §7），
    /// 地面角色（路人/小精灵/agent）互不重叠且不挤进建筑。
    fn separate_all(&mut self) {
        let mut moved = Vec::new();
        for i in 0..self.npcs.len() {
            moved.push((self.npcs[i].cx, self.npcs[i].cy));
        }
        // NPC 之间 + NPC↔路人
        for i in 0..self.npcs.len() {
            for (j, (ox, oy)) in moved.iter().copied().enumerate() {
                if i == j {
                    continue;
                }
                separate(&mut self.npcs[i], &self.walk_grid, ox, oy);
            }
            for p in self.passers.iter() {
                if p.kind == PasserKind::Bird {
                    continue;
                }
                separate(&mut self.npcs[i], &self.walk_grid, p.cx, p.cy);
            }
        }
        for i in 0..self.passers.len() {
            if self.passers[i].kind == PasserKind::Bird {
                continue;
            }
            for j in 0..self.passers.len() {
                if i == j || self.passers[j].kind == PasserKind::Bird {
                    continue;
                }
                let (ox, oy) = (self.passers[j].cx, self.passers[j].cy);
                let p = &mut self.passers[i];
                separate_pass(p, &self.walk_grid, ox, oy);
            }
            for a in self.npcs.iter() {
                let p = &mut self.passers[i];
                separate_pass(p, &self.walk_grid, a.cx, a.cy);
            }
        }
    }

    /// 粒子：推进 + 越界回卷。
    fn advance_particles(&mut self, dt: f64) {
        let season = self.season();
        if self.last_season != Some(season) {
            self.particles.clear();
            for _ in 0..30 {
                self.particles.push(Particle {
                    x: self.rng.f64() * TOWN_W as f64,
                    y: self.rng.f64() * TOWN_H as f64,
                    vx: (self.rng.f64() - 0.5) * 0.3,
                    vy: 0.3 + self.rng.f64() * 0.5,
                    tw: self.rng.f64() * std::f64::consts::PI * 2.0,
                    kind: season.particle(),
                });
            }
            self.last_season = Some(season);
        }
        for p in self.particles.iter_mut() {
            p.y += p.vy * dt * 60.0; // 60fps 帧基准：每帧 vy px（L1913）
            p.x += p.vx * dt * 60.0;
            if p.y > TOWN_H as f64 + 4.0 {
                p.y = -4.0;
                p.x = self.rng.f64() * TOWN_W as f64;
            }
            if p.x < -4.0 {
                p.x = TOWN_W as f64 + 4.0;
            }
            if p.x > TOWN_W as f64 + 4.0 {
                p.x = -4.0;
            }
        }
    }

    /// 渲染完整一帧到 RGBA 缓冲（`tick` L1942-1970 的绘制半段）：
    /// 场景 → 建筑 → 真实 agent → 装饰居民 → 粒子（雨默认关）。
    pub fn render(&self, buf: &mut [u32]) {
        debug_assert_eq!(buf.len(), TOWN_W * TOWN_H);
        let pal = pal_now(self.now_ms);
        let t = self.now_ms / 1000.0;
        draw_scene(buf, &self.walk_grid, &pal, t);
        draw_stations(buf, &pal);
        for npc in self.npcs.iter() {
            draw_npc(buf, npc, &pal, self.now_ms);
        }
        for p in self.passers.iter() {
            draw_passer(buf, p, &pal, self.now_ms);
        }
        draw_particles(buf, &self.particles, t);
    }

    /// 场景是否包含真实 agent（空态判定用）。
    pub fn has_agents(&self) -> bool {
        !self.npcs.is_empty()
    }
}

/// `moveAlongPath()`（L1713-1721）。
fn move_along_path(e: &mut TownNpc, dt: f64, speed: f64) {
    if e.path.is_empty() {
        e.moving = false;
        return;
    }
    let wp = e.path[0];
    let dx = wp.0 - e.cx;
    let dy = wp.1 - e.cy;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 3.0 {
        e.cx = wp.0;
        e.cy = wp.1;
        e.path.remove(0);
        e.moving = false;
        return;
    }
    let step = speed * dt;
    e.cx += (dx / dist) * step;
    e.cy += (dy / dist) * step;
    e.moving = true;
    e.dir = if dx >= 0.0 { 1.0 } else { -1.0 };
    e.frame += dt * 8.0;
}

fn move_along_path_passer(e: &mut Passer, dt: f64, speed: f64) {
    if e.path.is_empty() {
        return;
    }
    let wp = e.path[0];
    let dx = wp.0 - e.cx;
    let dy = wp.1 - e.cy;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 3.0 {
        e.cx = wp.0;
        e.cy = wp.1;
        e.path.remove(0);
        return;
    }
    let step = speed * dt;
    e.cx += (dx / dist) * step;
    e.cy += (dy / dist) * step;
    e.dir = if dx >= 0.0 { 1.0 } else { -1.0 };
    e.frame += dt * 8.0;
}

/// `separateGround()` 单对分离（14px 距离内 8px 分离力，L1729-1734）。
fn separate(e: &mut TownNpc, grid: &[u8], qx: f64, qy: f64) {
    let dx = e.cx - qx;
    let dy = e.cy - qy;
    let d = (dx * dx + dy * dy).sqrt();
    if d > 0.0 && d < 14.0 {
        let f = (14.0 - d) * 0.45;
        let mut nx = e.cx + dx / d * f;
        let mut ny = e.cy + dy / d * f;
        if !is_walk_px(grid, nx as i32, ny as i32) {
            nx = e.cx + dx / d * f * 0.5;
            ny = e.cy + dy / d * f * 0.5;
        }
        if is_walk_px(grid, nx as i32, ny as i32) {
            e.cx = nx;
            e.cy = ny;
        }
    }
}

fn separate_pass(e: &mut Passer, grid: &[u8], qx: f64, qy: f64) {
    let dx = e.cx - qx;
    let dy = e.cy - qy;
    let d = (dx * dx + dy * dy).sqrt();
    if d > 0.0 && d < 14.0 {
        let f = (14.0 - d) * 0.45;
        let mut nx = e.cx + dx / d * f;
        let mut ny = e.cy + dy / d * f;
        if !is_walk_px(grid, nx as i32, ny as i32) {
            nx = e.cx + dx / d * f * 0.5;
            ny = e.cy + dy / d * f * 0.5;
        }
        if is_walk_px(grid, nx as i32, ny as i32) {
            e.cx = nx;
            e.cy = ny;
        }
    }
}

// ============================ NPC 绘制 ============================

fn npc_rows(pose: &str, hat: usize) -> Option<&'static [&'static str; 24]> {
    let hat_key = match hat {
        0 => "hat0",
        1 => "hat1",
        _ => "hat2",
    };
    NPC_TABLES
        .iter()
        .find(|(p, h, _)| *p == pose && *h == hat_key)
        .map(|(_, _, rows)| *rows)
}

fn passer_rows(name: &str) -> Option<&'static [&'static str]> {
    PASSER_TABLES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, rows)| *rows)
}

/// `drawNPC()`（L1872-1895）。
fn draw_npc(b: &mut [u32], a: &TownNpc, pal: &Pal, now_ms: f64) {
    let meta = stage_meta(&a.key);
    let x = a.cx as i32;
    let y = a.cy as i32;
    let moving = a.state == NpcState::Walk || a.state == NpcState::ToWork;
    let frame = (a.frame as i32) % 2;
    let pose = if a.cheering {
        "cheer"
    } else if moving {
        if frame != 0 {
            "walk1"
        } else {
            "walk2"
        }
    } else {
        "stand"
    };
    let rows = npc_rows(pose, a.hat_style).unwrap_or_else(|| npc_rows("stand", 0).unwrap());
    let flip = a.dir < 0.0;
    let shirt = if a.working {
        sprite_color(meta.color, pal)
    } else {
        sprite_color(VILLAGER_PALETTES[a.palette], pal)
    };
    let hat = sprite_color(SCARF_COLORS[a.hat_color], pal);
    let hair = sprite_color(HAIR_COLORS[a.hair_color], pal);
    let face = sprite_color("#f2c6a0", pal);
    let pants = sprite_color(PANTS_COLORS[a.pants_color], pal);
    let shoe = sprite_color(SHOE_COLORS[a.shoe_color], pal);
    draw_shadow(b, x as f64 - 9.0, y as f64 - 1.0, 18.0, 3.0, pal, 0.2);
    let cl: Vec<(u8, u32)> = vec![
        (b'R', hat),
        (b'H', hair),
        (b'F', face),
        (b'E', 0xff20_242b),
        (b'S', shirt),
        (b'T', pants),
        (b'B', shoe),
        (b'A', sprite_color("#20242b", pal)),
        (b'N', sprite_color(SCARF_COLORS[a.scarf_color], pal)),
    ];
    spr(b, rows, x - 9, y - 25, &cl, flip);
    if a.working && !moving {
        // 工位小动画（L1890-1893）：sin(now/120) 相位微动。
        let bob = (now_ms / 120.0).sin() > 0.0;
        if bob {
            spr(b, rows, x - 9, y - 26, &cl, flip);
        }
    }
}

/// `drawPasser()`（L1788-1828）。
fn draw_passer(b: &mut [u32], p: &Passer, pal: &Pal, now_ms: f64) {
    match p.kind {
        PasserKind::Bird => {
            let frame = (p.frame as i32) % 2;
            if let Some(rows) = passer_rows("bird") {
                let cl = [(b'W', 0xffe0_e0e0)];
                spr(
                    b,
                    rows,
                    (p.cx - 4.0).round() as i32,
                    (p.cy - 3.0 + frame as f64).round() as i32,
                    &cl,
                    false,
                );
            }
        }
        PasserKind::Human => {
            let x = p.cx.round() as i32;
            let y = p.cy.round() as i32;
            let is_walk = p.state == PasserState::Walk;
            let frame = (p.frame as i32) % 2;
            let table = if is_walk {
                if frame != 0 {
                    "adult_walk1"
                } else {
                    "adult_walk2"
                }
            } else {
                "adult_stand"
            };
            let table = if p.ptype == "child" {
                match (is_walk, frame) {
                    (true, 0) => "child_walk2",
                    (true, _) => "child_walk1",
                    (false, _) => "child_stand",
                }
            } else {
                table
            };
            if let Some(rows) = passer_rows(table) {
                let cl: Vec<(u8, u32)> = vec![
                    (b'R', sprite_color(VILLAGER_PALETTES[p.clothes], pal)),
                    (b'H', sprite_color(HAIR_COLORS[p.hair], pal)),
                    (b'F', sprite_color("#f2c6a0", pal)),
                    (b'E', 0xff20_242b),
                    (b'S', sprite_color(VILLAGER_PALETTES[p.clothes], pal)),
                    (b'T', sprite_color("#2f3a4a", pal)),
                    (b'B', sprite_color("#20242b", pal)),
                    (b'A', sprite_color("#20242b", pal)),
                ];
                draw_shadow(b, p.cx - 6.0, p.cy, 12.0, 3.0, pal, 0.2);
                let lift = if p.state == PasserState::Wave {
                    if (now_ms / 80.0).sin() > 0.0 {
                        -2
                    } else {
                        0
                    }
                } else if p.state == PasserState::Chat {
                    1
                } else {
                    0
                };
                spr(
                    b,
                    rows,
                    x - 9,
                    y - rows.len() as i32 + 1 + lift,
                    &cl,
                    p.dir < 0.0,
                );
            }
        }
        PasserKind::Pokemon => {
            let x = p.cx.round() as i32;
            let y = p.cy.round() as i32;
            if let Some(rows) = passer_rows(p.ptype) {
                let cl: Vec<(u8, u32)> = match p.ptype {
                    "pokemon1" => vec![
                        (b'Y', 0xfff0_d050),
                        (b'E', 0xff20_242b),
                        (b'R', 0xffc0_4040),
                    ],
                    "pokemon2" => vec![(b'B', 0xff58_a0e8), (b'E', 0xff20_242b)],
                    "pokemon3" => vec![(b'G', 0xff58_c058), (b'E', 0xff20_242b)],
                    "pokemon4" => vec![(b'G', 0xff88_c050), (b'E', 0xff20_242b)],
                    "pokemon5" => vec![(b'R', 0xffe0_a0a0), (b'E', 0xff20_242b)],
                    "pokemon6" => vec![(b'B', 0xff70_a8e0), (b'E', 0xff20_242b)],
                    _ => vec![(b'O', 0xffd0_8050), (b'E', 0xff20_242b)],
                };
                draw_shadow(b, p.cx - 4.0, p.cy, 8.0, 3.0, pal, 0.2);
                let hop = if p.state == PasserState::Hop || p.state == PasserState::Sparkle {
                    -(p.hop_t.max(0.4) * 5.0).max(0.0)
                } else {
                    0.0
                };
                spr(
                    b,
                    rows,
                    x - 5,
                    y - rows.len() as i32 + 1 + hop as i32,
                    &cl,
                    p.dir < 0.0,
                );
                if p.state == PasserState::Sparkle {
                    px(b, x + 4, y - 12, 0xffff_f0a0);
                    px(b, x - 6, y - 9, 0xffff_f0a0);
                    px(b, x, y - 16, 0xffff_f0a0);
                }
            }
        }
    }
}

/// `drawParticles()`（L1907-1923）：四季粒子（花瓣/雨/叶/雪）。
fn draw_particles(b: &mut [u32], particles: &[Particle], t: f64) {
    for p in particles {
        match p.kind {
            ParticleKind::Petal => {
                let _ = (t * 3.0 + p.tw).sin(); // 相位保留（HTML 用透明度，RGBA 全不透明 → 直接绘制）
                rect(b, p.x as i32, p.y as i32, 2, 1, 0xfff4_8fb0);
            }
            ParticleKind::Rain => rect(b, p.x as i32, p.y as i32, 1, 3, 0xff9f_d8ef),
            ParticleKind::Leaf => rect(b, p.x as i32, p.y as i32, 2, 1, 0xffe8_a828),
            ParticleKind::Snow => px(b, p.x as i32, p.y as i32, 0xffff_ffff),
        }
    }
}

// ============================ 测试（绘制正确性 seam） ============================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::monitor::WireAgent;
    use crate::model::agent_roster::AgentRosterEntry;

    #[test]
    fn pal_now_fixed_timestamp_is_deterministic() {
        // 07:30（TIME_OFFSET）：白天、春夏之交第 0 天 → 春。
        let pal = pal_now(0.0);
        assert_eq!(pal.season, SeasonKind::Spring);
        assert_eq!(pal.min_of_day, 450.0);
        let pal2 = pal_now(0.0);
        assert_eq!(pal.bright, pal2.bright);
        assert_eq!(pal.m, pal2.m);
        // 冬：8*3 天后进入冬季（day 24 → season idx 0? 24/8=3 %4=3 冬）。
        let pal_w = pal_now(24.0 * DAY_MS - TIME_OFFSET);
        assert_eq!(pal_w.season, SeasonKind::Winter);
    }

    #[test]
    fn phase_name_boundaries() {
        assert_eq!(phase_name(329.0), "深夜");
        assert_eq!(phase_name(330.0), "清晨");
        assert_eq!(phase_name(450.0), "白天");
        assert_eq!(phase_name(1050.0), "黄昏");
        assert_eq!(phase_name(1170.0), "深夜");
    }

    #[test]
    fn static_scene_key_pixels_match_prototype_reference() {
        // 关键像素采样（Step 4 Prototype 逐字节比对通过后的回归锚点）：
        // 固定时间 now=0 下建筑墙/水面/道路颜色断言（防移植回归）。
        let mut grid = vec![1u8; GW * GH];
        build_station_map(&mut grid);
        build_nature_map(&mut grid);
        let mut b = vec![0u32; TOWN_W * TOWN_H];
        let pal = pal_now(0.0);
        draw_scene(&mut b, &grid, &pal, 0.0);
        draw_stations(&mut b, &pal);

        // 道路（横主街 y≈248..280 中心 (100, 265) 附近应为 road 色）。
        let road = sprite_color("#c9b18e", &pal);
        assert_eq!(b[265 * TOWN_W + 100], road, "横主街应为道路色");
        // 竖主街 (460..504)。
        assert_eq!(b[100 * TOWN_W + 480], road, "竖主街应为道路色");
        // 水面深水环（南侧池塘 cx=225,cy=486；water 椭圆 rx-4=61 → x=225+63
        // 在 deep 环内、water 环外）。
        let deep = sprite_color("#3f6d9c", &pal);
        assert_eq!(b[486 * TOWN_W + 288], deep, "南侧池塘深水环应为深水色");
        // 开发工厂墙体（implementing x=580,y=310；墙区 582..697 × 318..397
        // → 内部点 (620, 360)）。
        let wall = sprite_color("#e8eef7", &pal);
        assert_eq!(b[360 * TOWN_W + 620], wall, "开发工厂墙体色");
        // 建筑区域应为障碍（walk grid 0）。
        assert!(!is_walk_px(&grid, 700, 360));
        // 道路可走。
        assert!(is_walk_px(&grid, 100, 265));
    }

    #[test]
    fn collision_grid_buildings_and_nature_are_blocked() {
        let mut grid = vec![1u8; GW * GH];
        build_station_map(&mut grid);
        build_nature_map(&mut grid);
        // 建筑内部（refining 中心 83, 92）。
        assert!(!is_walk_px(&grid, 83, 92));
        // 门前 apron 可走（refining door south: (83, 120)，apron y 121..128）。
        assert!(is_walk_px(&grid, 83, 122));
        // 池塘（850,8,86,44 中心 893,30）不可走。
        assert!(!is_walk_px(&grid, 893, 30));
        // 树（48,36）附近不可走。
        assert!(!is_walk_px(&grid, 48, 36));
        // 中央喷泉（460,248,40,40）不可走。
        assert!(!is_walk_px(&grid, 480, 268));
    }

    #[test]
    fn find_path_reaches_goal_and_avoids_obstacles() {
        let mut grid = vec![1u8; GW * GH];
        build_station_map(&mut grid);
        build_nature_map(&mut grid);
        // 从广场 (480,300) 到 implementing 门口（west: x=578, cy=354）。
        let path = find_path(&grid, 480.0, 300.0, 578.0, 354.0);
        assert!(!path.is_empty(), "广场→开发工厂应有路径");
        let last = *path.last().unwrap();
        assert_eq!((gx(last.0), gy(last.1)), (gx(578.0), gy(354.0)));
        // 路径全程可行走。
        for (x, y) in &path {
            assert!(
                is_walk_px(&grid, *x as i32, *y as i32),
                "路径点 ({x},{y}) 不可走"
            );
        }
        // 不可达点：建筑内部 → 目标自动吸附最近可走格（不崩溃）。
        let path2 = find_path(&grid, 480.0, 300.0, 83.0, 92.0);
        assert!(!path2.is_empty());
    }

    #[test]
    fn find_path_same_cell_is_trivial() {
        let grid = vec![1u8; GW * GH];
        let path = find_path(&grid, 100.0, 100.0, 100.0, 100.0);
        assert_eq!(path.len(), 1);
    }

    #[test]
    fn station_pos_matches_building_doors() {
        assert_eq!(station_pos("refining"), (83.0, 120.0));
        assert_eq!(station_pos("implementing"), (582.0, 354.0));
        assert_eq!(station_pos("audit"), (108.0, 339.0));
        // 未知 stage → working 工位。
        assert_eq!(station_pos("nope"), station_pos("working"));
    }

    #[test]
    fn sync_agents_adds_updates_removes() {
        let mut scene = TownScene::new(7, 0.0);
        assert!(!scene.has_agents());
        let e1 = entry("session-a", "implementing", "working");
        let e2 = entry("session-b", "", "idle");
        scene.sync_agents(&[e1.clone(), e2.clone()]);
        assert_eq!(scene.npcs.len(), 2);
        // 出生在中央广场。
        assert_eq!(scene.npcs[0].cx, 480.0);
        assert_eq!(scene.npcs[0].key, "implementing");
        // 同 sessionId 重复 poll 幂等（不新增 NPC）。
        scene.sync_agents(&[e1.clone(), e2.clone()]);
        assert_eq!(scene.npcs.len(), 2);
        // 字段更新 + 阶段变更重置上工。
        let mut e1b = e1.clone();
        e1b.seq = 9;
        scene.sync_agents(&[e1b]);
        assert_eq!(scene.npcs[0].entry.seq, 9);
        assert_eq!(scene.npcs.len(), 1, "消失 agent 移除");
        // 恢复路径：重新出现 → 新 NPC。
        scene.sync_agents(&[e1.clone(), e2.clone()]);
        assert_eq!(scene.npcs.len(), 2);
    }

    #[test]
    fn npc_working_walks_to_station_over_time() {
        let mut scene = TownScene::new(11, 0.0);
        let e = entry("session-w", "implementing", "working");
        scene.sync_agents(&[e]);
        // 长时间推进后应到达工位（或至少 assigned 且状态在 toWork/working）。
        for ms in 1..=60_000 {
            scene.advance(ms as f64);
        }
        let npc = &scene.npcs[0];
        assert!(npc.assigned);
        assert!(
            npc.state == NpcState::Working || npc.state == NpcState::ToWork,
            "working agent 状态应为 toWork/working，实际 {:?}",
            npc.state
        );
        // 路径点全部可行走。
        for (x, y) in &npc.path {
            assert!(is_walk_px(&scene.walk_grid, *x as i32, *y as i32));
        }
    }

    #[test]
    fn idle_agent_roams_and_never_leaves_walkable() {
        let mut scene = TownScene::new(13, 0.0);
        let e = entry("session-i", "", "idle");
        scene.sync_agents(&[e]);
        for ms in (0..=120_000).step_by(1000) {
            scene.advance(ms as f64);
            for npc in scene.npcs.iter() {
                assert!(is_walk_px(&scene.walk_grid, npc.cx as i32, npc.cy as i32));
            }
        }
    }

    #[test]
    fn render_produces_full_frame_with_agents() {
        let mut scene = TownScene::new(17, 0.0);
        let e = entry("session-r", "review", "working");
        scene.sync_agents(&[e]);
        scene.advance(1000.0);
        let mut b = vec![0u32; TOWN_W * TOWN_H];
        scene.render(&mut b);
        // 非空（天空/草地已绘制）。
        assert!(b.iter().any(|v| *v != 0));
        // 状态条口径字段（ADR-008 不自算，仅透传）。
        assert_eq!(scene.npcs[0].entry.session_id.get(), "session-r");
    }

    fn entry(sid: &str, task_status: &str, status: &str) -> AgentRosterEntry {
        AgentRosterEntry::from_wire(&WireAgent {
            session_id: sid.into(),
            phase: "".into(),
            task: "任务X".into(),
            project: "proj".into(),
            task_id: "TASK-1".into(),
            status: status.into(),
            task_status: task_status.into(),
            elapsed: 100,
            last_event_at: 0,
            seq: 1,
            label: "".into(),
            kind: "session".into(),
            parent_session_id: "".into(),
            delegation_depth: 0,
            provider: "p".into(),
            model: "m".into(),
        })
    }
}

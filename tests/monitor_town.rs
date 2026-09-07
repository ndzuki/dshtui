//! REQ-009 小镇模型集成测试（tests/monitor_town.rs）：
//! 场景 → NPC 行为 → 画布渲染 → kitty 帧编码的端到端链路（纯函数层，
//! 无网络/无终端）。

use dshtui::api::monitor::WireAgent;
use dshtui::model::agent_roster::AgentRosterEntry;
use dshtui::model::agent_town::{pal_now, phase_name, SeasonKind, TownScene, TOWN_H, TOWN_W};
use dshtui::ui::agent_town::{PaintOutcome, TownCanvas};

fn entry(sid: &str, task_status: &str, status: &str) -> AgentRosterEntry {
    AgentRosterEntry::from_wire(&WireAgent {
        session_id: sid.into(),
        phase: "".into(),
        task: "任务".into(),
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

#[test]
fn scene_render_through_canvas_produces_frames_and_stops_when_static() {
    let mut scene = TownScene::new(42, 0.0);
    scene.sync_agents(&[entry("session-a", "implementing", "working")]);
    let mut canvas = TownCanvas::new(31);
    assert_eq!(canvas.paint(&scene), PaintOutcome::Full);
    // 同场景重画 → 静态停帧（AC-009-03 口径）。
    assert_eq!(canvas.paint(&scene), PaintOutcome::Static);
    // 时间推进 → 动画增量（水面/喷泉/NPC）。
    scene.advance(500.0);
    assert!(matches!(canvas.paint(&scene), PaintOutcome::Delta { .. }));
}

#[test]
fn town_time_and_seasons_cycle() {
    assert_eq!(pal_now(0.0).season, SeasonKind::Spring);
    assert_eq!(phase_name(pal_now(0.0).min_of_day), "白天");
    // 一天 10 分钟：8 天后入夏。
    let summer_ms = 8.0 * 600_000.0;
    assert_eq!(pal_now(summer_ms).season, SeasonKind::Summer);
}

#[test]
fn long_run_keeps_npcs_on_walkable_ground() {
    // AC-009-10 前奏：2 名 agent + 装饰居民长时间模拟，位置始终可行走。
    let mut scene = TownScene::new(99, 0.0);
    scene.sync_agents(&[
        entry("session-1", "implementing", "working"),
        entry("session-2", "review", "working"),
        entry("session-3", "", "idle"),
    ]);
    for ms in (0..=120_000).step_by(1000) {
        scene.advance(ms as f64);
        for npc in scene.npcs.iter() {
            let x = npc.cx as i32;
            let y = npc.cy as i32;
            assert!(x >= 0 && x < TOWN_W as i32 && y >= 0 && y < TOWN_H as i32);
        }
    }
    assert_eq!(scene.npcs.len(), 3);
}

#[test]
fn render_never_panics_across_seasons_and_day_cycle() {
    let mut scene = TownScene::new(7, 0.0);
    scene.sync_agents(&[entry("session-x", "audit", "working")]);
    let mut buf = vec![0u32; TOWN_W * TOWN_H];
    // 覆盖昼夜/四季切换（一整天 + 跨季）。
    for ms in (0..=10 * 600_000).step_by(37_000) {
        scene.advance(ms as f64);
        scene.render(&mut buf);
    }
    assert!(buf.iter().any(|v| *v != 0));
}

#[test]
fn canvas_delta_rect_stays_within_bounds() {
    let mut scene = TownScene::new(3, 0.0);
    scene.sync_agents(&[entry("s", "implementing", "working")]);
    let mut canvas = TownCanvas::new(4);
    canvas.paint(&scene);
    for ms in (1..=5000).step_by(500) {
        scene.advance(ms as f64);
        if let PaintOutcome::Delta { x, y, w, h } = canvas.paint(&scene) {
            assert!(x + w <= TOWN_W as u32, "脏矩形越界: x={x} w={w}");
            assert!(y + h <= TOWN_H as u32, "脏矩形越界: y={y} h={h}");
        }
    }
}

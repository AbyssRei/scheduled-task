//! 调度器。
//!
//! 基于 jiff:1s tick 读本地墙钟(`Zoned`);times 在模型中为 `civil::Time`
//! (RON 中仍是 `HH:MM:SS`,加载即校验)。monthly 用 `civil::Date` 做真实月历
//! 判断:当月无该日则跳过。超时/去抖用 `std::time::Instant`(单调钟),
//! 不占时间库。

use std::collections::HashSet;

use jiff::{Timestamp, Zoned, civil::Time};

use crate::model::{TaskConfig, TaskId};

/// 一次调度触发的判定器。
///
/// 维护"今天已触发过哪些 (任务, 时刻)"去重;按天滚动。
/// 触发规则:每天每个时刻至多触发一次;
/// monthly 在此之上要求"当天 == 指定日"(真实月历,当月无该日自然跳过)。
#[derive(Default)]
pub struct FireTracker {
    /// 已触发标记:`(TaskId, 时间, 当天日期)`
    fired: HashSet<(TaskId, Time, jiff::civil::Date)>,
}

impl FireTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 下次尚未处理的本地计划时刻；仅预览，不修改去重标记。
    pub fn next_fire(&self, task: &TaskConfig, now: &Zoned) -> Option<jiff::civil::DateTime> {
        if !task.enabled || task.schedule.times().is_empty() {
            return None;
        }
        let mut date = now.date();
        for _ in 0..=366 {
            if task
                .schedule
                .monthly_day()
                .is_none_or(|day| date.day() == day.get() as i8)
                && let Some(time) = task
                    .schedule
                    .times()
                    .iter()
                    .filter(|time| {
                        (date > now.date() || **time >= now.time())
                            && !self.fired.contains(&(task.id, **time, date))
                    })
                    .min()
            {
                return Some(date.to_datetime(*time));
            }
            date = date.tomorrow().ok()?;
        }
        None
    }

    /// 判断此刻该任务是否应触发。
    ///
    /// 规则:当前本地时刻 >= 某个配置时刻,且该 (任务, 时刻) 今天尚未触发。
    /// 采用 `>=` 而非 `==` 以容忍 1s tick 在负载下丢秒;
    /// 触发后立即登记,当天不再重复。
    pub fn should_fire(&mut self, task: &TaskConfig, now: &Zoned) -> bool {
        if !task.enabled {
            return false;
        }
        let today = now.date();
        let now_time = now.time();
        // monthly 真实月历:仅当天 == 指定日触发;当月无该日(如 31 日在 4 月)
        // 自然永不匹配,整月跳过
        let day_ok = match task.schedule.monthly_day() {
            Some(day) => today.day() == day.get() as i8,
            None => true,
        };
        if !day_ok {
            return false;
        }
        for t in task.schedule.times() {
            if now_time >= *t {
                let key = (task.id, *t, today);
                if !self.fired.contains(&key) {
                    self.fired.insert(key);
                    return true;
                }
            }
        }
        false
    }

    /// 配置变更后清空去重标记(重新调度)。
    pub fn clear(&mut self) {
        self.fired.clear();
    }

    /// 启动/重排时,把"今日已过"的时刻视为已触发。
    ///
    /// 过点任务启动/重排后今日不补跑,等待下一次计划时刻。
    /// 避免应用重启后立即执行当天所有已过时刻的任务。
    pub fn seed_passed(&mut self, tasks: &[TaskConfig], now: &Zoned) {
        let today = now.date();
        let now_time = now.time();
        for t in tasks {
            if !t.enabled {
                continue;
            }
            for time in t.schedule.times() {
                if *time < now_time {
                    self.fired.insert((t.id, *time, today));
                }
            }
        }
    }

    /// 配置变更(增删改)后保留仍存在的 (任务, 时刻) 标记:
    /// 已触发过的时间点今日不重复触发。
    pub fn retain_config(&mut self, tasks: &[TaskConfig]) {
        let live: HashSet<(TaskId, Time)> = tasks
            .iter()
            .flat_map(|t| t.schedule.times().iter().map(move |time| (t.id, *time)))
            .collect();
        self.fired
            .retain(|(id, time, _)| live.contains(&(*id, *time)));
    }

    /// 清理过期标记(非当天),防止长期运行内存增长。
    pub fn gc(&mut self, today: jiff::civil::Date) {
        self.fired.retain(|(.., d)| *d == today);
    }
}

/// 把 `Timestamp` 转成本地 `Zoned`(系统时区)。
pub fn local_now(ts: Timestamp) -> Zoned {
    ts.to_zoned(jiff::tz::TimeZone::system())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DayOfMonth, Schedule, ScriptKind, ScriptSource, ScriptSpec};

    fn task(id: u64, schedule: Schedule) -> TaskConfig {
        TaskConfig {
            id: TaskId(id),
            name: format!("t{id}"),
            script: ScriptSpec {
                kind: ScriptKind::Auto,
                source: ScriptSource::File("t.bat".into()),
                args: Vec::new(),
                working_dir: None,
                env: Default::default(),
                env_file: None,
                env_order: Vec::new(),
                env_files: Vec::new(),
            },
            schedule,
            retry: None,
            timeout: None,
            enabled: true,
            concurrency: Default::default(),
        }
    }

    fn zoned(y: i16, m: i8, d: i8, hh: i8, mm: i8, ss: i8) -> Zoned {
        jiff::civil::date(y, m, d)
            .at(hh, mm, ss, 0)
            .in_tz("Asia/Shanghai")
            .unwrap()
    }

    fn daily(times: &[&str]) -> Schedule {
        Schedule::Daily {
            times: times.iter().map(|t| t.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn 计划预览排序时刻并保留当日去重且跳过不存在的月日期() {
        let mut tracker = FireTracker::new();
        let t = task(1, daily(&["18:00:00", "08:00:00"]));
        let morning = zoned(2026, 10, 14, 7, 0, 0);
        assert_eq!(
            tracker.next_fire(&t, &morning).unwrap().time(),
            "08:00:00".parse::<Time>().unwrap()
        );
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 14, 8, 0, 0)));
        assert_eq!(
            tracker.next_fire(&t, &morning).unwrap().time(),
            "18:00:00".parse::<Time>().unwrap()
        );
        let monthly = task(
            2,
            Schedule::Monthly {
                day: DayOfMonth::new(31).unwrap(),
                times: vec!["08:00:00".parse().unwrap()],
            },
        );
        let next = tracker
            .next_fire(&monthly, &zoned(2026, 1, 31, 9, 0, 0))
            .unwrap();
        assert_eq!(next.date(), jiff::civil::date(2026, 3, 31));
        let mut disabled = t.clone();
        disabled.enabled = false;
        assert!(tracker.next_fire(&disabled, &morning).is_none());
    }

    #[test]
    fn fires_once_per_time_per_day() {
        let t = task(1, daily(&["08:00:00"]));
        let mut tracker = FireTracker::new();

        // 未到点不触发
        assert!(!tracker.should_fire(&t, &zoned(2026, 10, 14, 7, 59, 59)));
        // 到点(含丢秒容忍)触发
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 14, 8, 0, 1)));
        // 同一天不重复触发
        assert!(!tracker.should_fire(&t, &zoned(2026, 10, 14, 8, 0, 2)));
        // 次日重新触发
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 15, 8, 0, 0)));
    }

    #[test]
    fn multiple_times_fire_independently() {
        let t = task(1, daily(&["08:00:00", "12:00:00"]));
        let mut tracker = FireTracker::new();
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 14, 8, 0, 0)));
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 14, 12, 0, 0)));
        assert!(!tracker.should_fire(&t, &zoned(2026, 10, 14, 12, 0, 1)));
    }

    #[test]
    fn disabled_never_fires() {
        let mut t = task(1, daily(&["08:00:00"]));
        t.enabled = false;
        let mut tracker = FireTracker::new();
        assert!(!tracker.should_fire(&t, &zoned(2026, 10, 14, 8, 0, 0)));
    }

    #[test]
    fn monthly_only_fires_on_target_day() {
        let t = task(
            1,
            Schedule::Monthly {
                day: DayOfMonth::new(15).unwrap(),
                times: vec!["09:30:00".parse().unwrap()],
            },
        );
        let mut tracker = FireTracker::new();
        // 非目标日不触发
        assert!(!tracker.should_fire(&t, &zoned(2026, 10, 14, 9, 30, 0)));
        // 目标日触发
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 15, 9, 30, 0)));
    }

    #[test]
    fn monthly_skips_months_without_the_day() {
        // 31 日:4 月只有 30 天,整月不触发;5 月 31 日触发
        let t = task(
            1,
            Schedule::Monthly {
                day: DayOfMonth::new(31).unwrap(),
                times: vec!["09:30:00".parse().unwrap()],
            },
        );
        let mut tracker = FireTracker::new();
        assert!(!tracker.should_fire(&t, &zoned(2026, 4, 30, 9, 30, 0)));
        assert!(tracker.should_fire(&t, &zoned(2026, 5, 31, 9, 30, 0)));
    }

    #[test]
    fn custom_behaves_like_daily() {
        let t = task(
            1,
            Schedule::Custom {
                times: vec!["12:00:00".parse().unwrap(), "18:00:00".parse().unwrap()],
            },
        );
        let mut tracker = FireTracker::new();
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 14, 12, 0, 0)));
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 14, 18, 0, 0)));
    }

    #[test]
    fn past_times_do_not_fire_on_startup() {
        // 启动时已过点的时刻今日不补跑
        let t = task(1, daily(&["08:00:00"]));
        let mut tracker = FireTracker::new();
        let now = zoned(2026, 10, 14, 12, 0, 0);
        tracker.seed_passed(std::slice::from_ref(&t), &now);
        assert!(!tracker.should_fire(&t, &zoned(2026, 10, 14, 12, 0, 1)));
        // 次日照常触发
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 15, 8, 0, 0)));
    }

    #[test]
    fn future_times_still_fire_after_seeding() {
        // 未来的时刻不受 seed 影响:到点照常触发
        let t = task(1, daily(&["08:00:00", "23:30:00"]));
        let mut tracker = FireTracker::new();
        let now = zoned(2026, 10, 14, 12, 0, 0);
        tracker.seed_passed(std::slice::from_ref(&t), &now);
        assert!(!tracker.should_fire(&t, &zoned(2026, 10, 14, 12, 0, 1)));
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 14, 23, 30, 0)));
    }

    #[test]
    fn gc_drops_old_marks() {
        let t = task(1, daily(&["08:00:00"]));
        let mut tracker = FireTracker::new();
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 14, 8, 0, 0)));
        tracker.gc(zoned(2026, 10, 15, 0, 0, 0).date());
        // 过期标记清理后,同一天(已过去)的时刻不再受标记影响,但日期已变,
        // 这里验证 gc 后次日仍可触发
        assert!(tracker.should_fire(&t, &zoned(2026, 10, 15, 8, 0, 0)));
    }
}

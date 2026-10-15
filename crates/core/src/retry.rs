//! 自动重试判定引擎。
//!
//! 判定规则:
//! - 触发条件:以"当前行 + 最近 (retry_count-1) 条历史行"为窗口,窗口内每一行
//!   都必须命中**同一个**候选模式(各行命中候选集合的交集非空;不同行命中不同候选不算)。
//! - 候选模式:retry_pattern 按行拆分(每行一个候选,OR 语义);匹配前对候选与
//!   日志行做空白归一化(连续空白→单空格)后子串匹配。
//! - retry_pattern 为空时退化为"窗口内所有行核心内容一致":extract_log_core
//!   剥离常见时间戳与 after N ms / port N 等动态数字。

use std::sync::OnceLock;

use regex::Regex;

/// 检查是否满足重试条件。
///
/// 判定语义:在"当前行 + 最近的历史行"组成的窗口内,需存在至少一个候选模式
/// 被窗口内每一行命中(各行命中集合交集非空)才触发;即"同一候选模式连续命中"
/// 才计数,不同行命中不同候选不会触发。
///
/// - `line`:当前输出行
/// - `retry_pattern`:重试模式,每行一个候选(OR 语义),空白归一化后子串匹配;
///   为空或仅含空白行时,按"核心内容相同"判定
/// - `output_history`:历史行(不含当前行)
/// - `retry_count`:连续出现次数阈值(含当前行)
pub fn check_retry_condition(
    line: &str,
    retry_pattern: &str,
    output_history: &[String],
    retry_count: usize,
) -> bool {
    // 输出历史不足 retry_count-1 条时,尚未达到连续阈值
    if output_history.len() < retry_count.saturating_sub(1) {
        return false;
    }

    let candidates = parse_candidates(retry_pattern);

    if candidates.is_empty() {
        // 未提供有效候选模式:检查窗口内所有历史行与当前行的核心内容是否一致
        let current_core = extract_log_core(line);
        return output_history
            .iter()
            .all(|history_line| extract_log_core(history_line) == current_core);
    }

    // 提供了候选模式:要求窗口内每一行命中同一个候选(各行命中集合交集非空)
    let mut common: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, cand)| match_pattern(line, cand))
        .map(|(idx, _)| idx)
        .collect();
    if common.is_empty() {
        return false;
    }
    for history_line in output_history {
        common.retain(|idx| match_pattern(history_line, &candidates[*idx]));
        if common.is_empty() {
            return false;
        }
    }
    true
}

/// 将重试模式按行拆分为候选模式列表。
///
/// 每行一个候选,去除首尾空白后丢弃空行。候选内部的空白归一化交给
/// [`match_pattern`] 在匹配时处理,这里只负责切分。
pub fn parse_candidates(retry_pattern: &str) -> Vec<String> {
    retry_pattern
        .lines()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect()
}

/// 判断行是否包含某个候选模式所描述的连续多字段内容。
///
/// 对行与单个候选均做空白归一化后再做子串匹配;当候选不含空白时退化为
/// 使用普通子串匹配。
fn match_pattern(line: &str, candidate: &str) -> bool {
    let line = normalize_ws(line);
    let candidate = normalize_ws(candidate);
    !candidate.is_empty() && line.contains(&candidate)
}

/// 将连续空白(空格、制表符等)归一化为单个空格,并去除首尾空白。
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 提取日志的核心内容,去除时间戳等动态部分。
pub fn extract_log_core(log_line: &str) -> String {
    // 去除时间戳
    let mut core = timestamp_1().replace_all(log_line, "").into_owned();
    core = timestamp_2().replace_all(&core, "").into_owned();
    // 去除动态数字部分
    for re in dynamic_patterns() {
        core = re.replace_all(&core, "").into_owned();
    }
    core.trim().to_string()
}

static TIMESTAMP_1: OnceLock<Regex> = OnceLock::new();
static TIMESTAMP_2: OnceLock<Regex> = OnceLock::new();
static DYNAMIC_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

fn timestamp_1() -> &'static Regex {
    TIMESTAMP_1.get_or_init(|| Regex::new(r"\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\]").unwrap())
}

fn timestamp_2() -> &'static Regex {
    TIMESTAMP_2.get_or_init(|| Regex::new(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2},\d+").unwrap())
}

fn dynamic_patterns() -> &'static [Regex] {
    DYNAMIC_PATTERNS.get_or_init(|| {
        [
            r"after \d+ ms", // 如 "after 2040 ms"
            r"port \d+",     // 如 "port 443"
            r"\d+ ms",       // 如 "2040 ms"
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    })
}

#[cfg(test)]
mod tests {
    //! 重试窗口、候选交集、空白归一化与核心内容提取的单元测试。

    use super::*;

    // ---- 日志样本 ----
    // 三条 Curl error 7:时间戳与 after N ms / port N 均不同,核心内容一致
    const CURL_1: &str = "[Log][2026-02-03 16:44:25]Curl error 7: Failed to connect to cdp.cloud.unity3d.com port 443 after 2040 ms: Could not connect to server";
    const CURL_2: &str = "[Log][2026-02-03 16:45:02]Curl error 7: Failed to connect to cdp.cloud.unity3d.com port 443 after 2051 ms: Could not connect to server";
    const CURL_3: &str = "[Log][2026-02-03 16:46:01]Curl error 7: Failed to connect to cdp.cloud.unity3d.com port 443 after 2052 ms: Could not connect to server";
    // 一条 scanner error 7
    const SCANNER_1: &str = "[Log][2026-02-03 16:44:30]scanner error 7: scan failed after 2040 ms: device not reachable";
    // 同时命中 Curl 与 scanner 两个候选的混合行
    const MIX_1: &str =
        "[Log][2026-02-03 16:44:40]Curl error 7 and scanner error 7: mixed failure after 2040 ms";
    // 核心内容明显不同的行
    const SUCCESS: &str = "[Log][2026-02-03 16:47:00]Successfully connected to server";

    fn hist(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    fn check(line: &str, pattern: &str, history: &[&str], retry_count: usize) -> bool {
        check_retry_condition(line, pattern, &hist(history), retry_count)
    }

    #[test]
    fn test_extract_log_core_strips_dynamic_parts() {
        // 时间戳与动态数字被去除后,三条 Curl 行核心内容应一致
        assert_eq!(extract_log_core(CURL_1), extract_log_core(CURL_2));
        assert_eq!(extract_log_core(CURL_2), extract_log_core(CURL_3));
    }

    #[test]
    fn test_no_pattern_core_content_same_triggers() {
        // 无候选模式:窗口内核心内容一致 -> 触发
        assert!(check(CURL_3, "", &[CURL_1, CURL_2], 3));
    }

    #[test]
    fn test_no_pattern_core_content_differs_no_trigger() {
        // 无候选模式:当前行核心内容与历史不同 -> 不触发
        assert!(!check(SUCCESS, "", &[CURL_1, CURL_2], 3));
    }

    #[test]
    fn test_no_pattern_insufficient_history_no_trigger() {
        // retry_count=3,历史不足 2 条 -> 守卫拦截
        assert!(!check(CURL_2, "", &[CURL_1], 3));
    }

    #[test]
    fn test_single_candidate_all_match_triggers() {
        // 单候选:窗口内每行均包含该候选 -> 触发
        assert!(check(CURL_3, "Curl error 7", &[CURL_1, CURL_2], 3));
    }

    #[test]
    fn test_single_candidate_one_mismatch_no_trigger() {
        // 单候选:有一条历史不含候选 -> 不触发
        assert!(!check(CURL_3, "Curl error 7", &[CURL_1, SUCCESS], 3));
    }

    #[test]
    fn test_multi_candidate_same_candidate_consecutive_triggers() {
        // 多候选:窗口内每行都命中 Curl 这一个候选 -> 触发
        let pattern = "Curl error 7\nscanner error 7";
        assert!(check(CURL_3, pattern, &[CURL_1, CURL_2], 3));
    }

    #[test]
    fn test_multi_candidate_all_hit_scanner_triggers() {
        // 多候选:窗口内每行都命中 scanner 这一个候选 -> 触发
        let scanner_2 = SCANNER_1.replace("16:44:30", "16:44:31");
        let scanner_3 = SCANNER_1.replace("16:44:30", "16:44:32");
        let pattern = "Curl error 7\nscanner error 7";
        assert!(check(SCANNER_1, pattern, &[&scanner_2, &scanner_3], 3));
    }

    #[test]
    fn test_multi_candidate_alternating_no_trigger() {
        // 多候选:当前行命中 Curl,历史分别命中 scanner 与 Curl,
        // 没有任一候选被窗口内每一行命中 -> 不触发
        let pattern = "Curl error 7\nscanner error 7";
        assert!(!check(CURL_3, pattern, &[SCANNER_1, CURL_1], 3));
    }

    #[test]
    fn test_multi_candidate_one_line_hits_both_still_triggers() {
        // 混合行同时命中两个候选,历史只命中 Curl,交集仍含 Curl -> 触发
        let pattern = "Curl error 7\nscanner error 7";
        assert!(check(MIX_1, pattern, &[CURL_1, CURL_2], 3));
    }

    #[test]
    fn test_whitespace_normalization_within_candidate() {
        // 候选内部多余空格被归一化,仍能匹配日志中的 "Curl error 7"
        assert!(check(CURL_3, "Curl  error  7", &[CURL_1, CURL_2], 3));
    }

    #[test]
    fn test_whitespace_normalization_with_tab_in_log() {
        // 日志中出现制表符,归一化后仍能被候选匹配
        let tabbed = "[Log][2026-02-03 16:44:25]Curl\terror 7: Failed after 2040 ms";
        let tabbed_2 = tabbed.replace("16:44:25", "16:44:26");
        let tabbed_3 = tabbed.replace("16:44:25", "16:44:27");
        assert!(check(&tabbed_3, "Curl error 7", &[tabbed, &tabbed_2], 3));
    }

    #[test]
    fn test_parse_candidates_splits_by_line_and_strips() {
        // 按行拆分,去除首尾空白,丢弃空行
        assert_eq!(
            parse_candidates("Curl error 7\n\n scanner error 7 \n"),
            vec!["Curl error 7", "scanner error 7"]
        );
    }

    #[test]
    fn test_parse_candidates_empty_or_blank_only() {
        assert!(parse_candidates("").is_empty());
        assert!(parse_candidates("\n  \n\t\n").is_empty());
    }

    #[test]
    fn test_blank_lines_in_pattern_do_not_create_empty_candidate() {
        // 模式中夹空行:空行不会成为"永远命中"的空候选导致误触发
        let pattern = "Curl error 7\n\n\nscanner error 7";
        // 交替场景仍应不触发(空行被丢弃,真正的两个候选无公共命中)
        assert!(!check(CURL_3, pattern, &[SCANNER_1, CURL_1], 3));
    }

    #[test]
    fn test_whitespace_only_pattern_falls_back_to_core_content() {
        // 仅含空白行的模式视为"无有效候选",回退到核心内容判定
        let pattern = "\n  \n\t\n";
        // 核心内容一致 -> 触发
        assert!(check(CURL_3, pattern, &[CURL_1, CURL_2], 3));
        // 核心内容不同 -> 不触发
        assert!(!check(SUCCESS, pattern, &[CURL_1, CURL_2], 3));
    }

    #[test]
    fn test_retry_count_threshold_guard() {
        // retry_count=3 需至少 2 条历史;仅 1 条 -> 守卫拦截,即使全部命中也不触发
        assert!(!check(CURL_2, "Curl error 7", &[CURL_1], 3));
        // retry_count=2 需至少 1 条历史 -> 满足且命中则触发
        assert!(check(CURL_2, "Curl error 7", &[CURL_1], 2));
    }

    #[test]
    fn test_retry_count_with_multi_candidate() {
        // 多候选下阈值守卫同样生效:历史不足不触发
        let pattern = "Curl error 7\nscanner error 7";
        assert!(!check(CURL_2, pattern, &[CURL_1], 3));
        // 历史足够且同一候选连续 -> 触发
        assert!(check(CURL_3, pattern, &[CURL_1, CURL_2], 3));
    }

    #[test]
    fn test_production_path_triggers_on_exact_retry_count() {
        // 模拟生产路径:上游在回调前把当前行 append 进 output_history,
        // 应在第 retry_count 条输入触发,不能提前一格。
        let curl = "Curl error 7: Failed after 2040 ms";
        let pattern = "Curl error 7";

        for rc in [2usize, 3, 4] {
            let mut history: Vec<String> = Vec::new();
            let mut trigger_at = None;
            for i in 0..10 {
                history.push(curl.to_string()); // 先记录当前行。
                if history.len() > rc {
                    // 保留最近的重试窗口。
                    history.remove(0);
                }
                // 传入历史时排除末尾的当前行,避免重复计数。
                let sliced = &history[..history.len() - 1];
                if check_retry_condition(curl, pattern, sliced, rc) {
                    trigger_at = Some(i + 1);
                    break;
                }
            }
            assert_eq!(
                trigger_at,
                Some(rc),
                "retry_count={rc}: 期望第 {rc} 条触发,实际第 {trigger_at:?} 条"
            );
        }
    }

    #[test]
    fn test_production_path_multi_candidate_alternating_no_early_trigger() {
        // 多候选交替不满足同一候选连续命中,当前行只能计入窗口一次。
        let pattern = "Curl error 7\nscanner error 7";
        let mut history: Vec<String> = Vec::new();
        let mut triggered = false;
        for ln in [CURL_1, SCANNER_1, CURL_2] {
            history.push(ln.to_string());
            if history.len() > 3 {
                history.remove(0);
            }
            let sliced = &history[..history.len() - 1];
            if check_retry_condition(ln, pattern, sliced, 3) {
                triggered = true;
                break;
            }
        }
        assert!(!triggered, "交替命中不同候选不应触发");
    }
}

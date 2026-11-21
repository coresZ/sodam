//! 本地文件 → 汽水元数据的匹配。
//!
//! 使用场景：清单丢失 / 从别的机器迁移过来 / 用户手动放进下载目录的文件 ——
//! 这些文件只有文件名，没有元数据。「匹配」就是拿文件名去搜索，再按相似度
//! 挑一个**可信**的结果回来，写进清单。
//!
//! 设计要点：
//!
//! * **宁可漏，不可错**：匹配错了比匹配不上更糟（会把错的封面 / 歌名钉在文件上）。
//!   所以打分阈值偏高，达不到就返回 `None`，由界面提示「未找到匹配」。
//! * **时长是最强的防误判信号**：同名曲目（翻唱 / 不同版本）很多，
//!   但时长能对上说明大概率是同一首。
//! * **纯逻辑与网络分开**：打分是纯函数（可单测），搜索走 `Session`。

use crate::models::TrackItem;
use crate::session::Session;

/// 判定为「匹配上」的最低分。低于它返回 `None`。
pub const MATCH_THRESHOLD: i32 = 60;

/// 标题归一化全等的加分。
const SCORE_TITLE_EXACT: i32 = 50;
/// 标题互相包含（`某某` vs `某某 (Live)`）的加分。
const SCORE_TITLE_CONTAINS: i32 = 30;
/// 歌手命中加分。
const SCORE_ARTIST: i32 = 20;
/// 时长对得上（±2 秒）的加分 —— 最强的防误判信号。
const SCORE_DURATION: i32 = 30;
/// 时长差在 5 秒内的部分加分。
const SCORE_DURATION_LOOSE: i32 = 10;

/// 一次匹配的结果与依据（界面 / 日志用）。
#[derive(Clone, Debug, PartialEq)]
pub struct MatchOutcome {
    pub track: TrackItem,
    pub score: i32,
}

/// 从文件名里猜出 (歌手, 标题) 线索。
///
/// 按命名模板的常见形态拆：`{artist} - {title}` 优先，其次整串当标题。
/// 分隔符用半角 `-`（模板默认），两侧允许空格。
pub fn guess_artist_title(file_stem: &str) -> (Option<String>, String) {
    let stem = file_stem.trim();
    // 从右往左找第一个 ` - `：歌名里更可能有连字符，歌手名一般没有。
    if let Some((artist, title)) = stem.rsplit_once(" - ") {
        let artist = artist.trim();
        let title = title.trim();
        if !artist.is_empty() && !title.is_empty() {
            return (Some(artist.to_string()), title.to_string());
        }
    }
    (None, stem.to_string())
}

/// 归一化：小写 + 去掉常见修饰与标点空白。
///
/// `(Live)` / `【】` / `feat.` 这类差异不该让同一首歌判为不同。
fn normalize(value: &str) -> String {
    let mut text = value.to_lowercase();
    for noise in [
        "(live)",
        "（live）",
        "[live]",
        "(remaster)",
        "(remastered)",
        "(demo)",
        "(伴奏)",
        "(纯音乐)",
        "feat.",
        "ft.",
    ] {
        text = text.replace(noise, "");
    }
    text.chars()
        .filter(|character| !character.is_whitespace() && !is_punctuation(*character))
        .collect()
}

fn is_punctuation(character: char) -> bool {
    matches!(
        character,
        '-' | '_'
            | '.'
            | ','
            | '!'
            | '?'
            | '\''
            | '"'
            | ':'
            | ';'
            | '('
            | ')'
            | '['
            | ']'
            | '【'
            | '】'
            | '《'
            | '》'
            | '（'
            | '）'
            | '·'
            | '、'
            | '，'
            | '。'
            | '！'
            | '？'
    )
}

/// 给候选打分。纯函数，便于单测。
///
/// `duration_hint` 是本地文件探测出的时长（秒），`None` 表示未知 ——
/// 未知时不给时长分（而不是给满分），避免把不确定当成确定。
pub fn score_candidate(
    candidate: &TrackItem,
    title_hint: &str,
    artist_hint: Option<&str>,
    duration_hint: Option<f64>,
) -> i32 {
    let mut score = 0;
    let want = normalize(title_hint);
    let got = normalize(&candidate.title);
    if !want.is_empty() && want == got {
        score += SCORE_TITLE_EXACT;
    } else if !want.is_empty() && (got.contains(&want) || want.contains(&got)) {
        score += SCORE_TITLE_CONTAINS;
    } else {
        // 标题都对不上，直接判定不可信（避免「歌手对 + 时长对」把错歌凑过线）。
        return 0;
    }

    if let Some(artist) = artist_hint {
        let want = normalize(artist);
        let got = normalize(&candidate.artist);
        if !want.is_empty()
            && !got.is_empty()
            && (want == got || got.contains(&want) || want.contains(&got))
        {
            score += SCORE_ARTIST;
        }
    }

    if let Some(hint) = duration_hint.filter(|seconds| *seconds > 0.0) {
        let candidate_seconds = candidate.duration_seconds as f64;
        if candidate_seconds > 0.0 {
            let delta = (candidate_seconds - hint).abs();
            if delta <= 2.0 {
                score += SCORE_DURATION;
            } else if delta <= 5.0 {
                score += SCORE_DURATION_LOOSE;
            }
        }
    }

    score
}

/// 用文件名搜索并挑出最可信的一条。
///
/// 搜索关键词优先用**标题**（歌手名会干扰召回：汽水搜索对 `歌手 - 歌名` 整串
/// 的处理不稳定），必要时退化成整串。
///
/// 返回 `None` = 没找到够可信的（界面提示「未找到匹配」）。
pub fn match_track(
    session: &Session,
    file_stem: &str,
    duration_hint: Option<f64>,
) -> anyhow::Result<Option<MatchOutcome>> {
    let candidates = search_candidates(session, file_stem, duration_hint)?;
    Ok(candidates
        .into_iter()
        .find(|outcome| outcome.score >= MATCH_THRESHOLD))
}

/// 候选数量上限：界面只展示前几名，多取没意义。
pub const MAX_CANDIDATES: usize = 5;

/// 搜索并按分数**降序**返回前 [`MAX_CANDIDATES`] 个候选（**不过阈值**）。
///
/// 供「匹配失败时手动选择」用：自动匹配要求 `>= MATCH_THRESHOLD`，
/// 但用户可能一眼认出正确的那条（比如冷门翻唱，分数不够高）。
/// 所以这里返回分数最高的几个，让用户自己判断。
pub fn search_candidates(
    session: &Session,
    file_stem: &str,
    duration_hint: Option<f64>,
) -> anyhow::Result<Vec<MatchOutcome>> {
    let (artist_hint, title_hint) = guess_artist_title(file_stem);
    // 先用标题搜；标题太短（1 个字符）时用整串，避免召回一堆无关结果。
    let keyword = if title_hint.chars().count() >= 2 {
        title_hint.clone()
    } else {
        file_stem.to_string()
    };

    let candidates = session.search(&keyword)?;
    let mut scored: Vec<MatchOutcome> = candidates
        .into_iter()
        .map(|track| {
            let score = score_candidate(&track, &title_hint, artist_hint.as_deref(), duration_hint);
            MatchOutcome { track, score }
        })
        .filter(|outcome| outcome.score > 0)
        .collect();
    scored.sort_by_key(|outcome| std::cmp::Reverse(outcome.score));
    scored.truncate(MAX_CANDIDATES);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artist: &str, duration: i64) -> TrackItem {
        TrackItem {
            id: format!("id-{title}"),
            title: title.to_string(),
            artist: artist.to_string(),
            duration_seconds: duration,
            ..Default::default()
        }
    }

    #[test]
    fn guess_splits_artist_and_title() {
        assert_eq!(
            guess_artist_title("南风叙 - 两不知"),
            (Some("南风叙".to_string()), "两不知".to_string())
        );
        // 没有分隔符：整串当标题
        assert_eq!(guess_artist_title("Yang"), (None, "Yang".to_string()));
        // 歌名里有连字符：从右往左拆
        assert_eq!(
            guess_artist_title("某人 - A - B"),
            (Some("某人 - A".to_string()), "B".to_string())
        );
    }

    #[test]
    fn exact_title_with_matching_duration_scores_high() {
        let candidate = track("两不知", "南风叙", 211);
        let score = score_candidate(&candidate, "两不知", Some("南风叙"), Some(211.4));
        assert_eq!(score, 50 + 20 + 30, "标题全等 + 歌手 + 时长");
    }

    #[test]
    fn title_mismatch_never_passes() {
        // 歌手与时长都对，但标题完全不同 → 0 分（防「凑分」）
        let candidate = track("另一首歌", "南风叙", 211);
        assert_eq!(
            score_candidate(&candidate, "两不知", Some("南风叙"), Some(211.0)),
            0
        );
    }

    #[test]
    fn unknown_duration_does_not_grant_duration_points() {
        let candidate = track("两不知", "南风叙", 211);
        let with = score_candidate(&candidate, "两不知", None, Some(211.0));
        let without = score_candidate(&candidate, "两不知", None, None);
        assert!(without < with, "未知时长不该拿到时长分");
        assert_eq!(without, 50);
    }

    #[test]
    fn live_suffix_still_matches_exactly() {
        let candidate = track("两不知 (Live)", "南风叙", 211);
        let score = score_candidate(&candidate, "两不知", Some("南风叙"), Some(211.0));
        assert_eq!(score, 50 + 20 + 30, "归一化后 (Live) 应被忽略");
    }

    #[test]
    fn loose_duration_only_scores_partially() {
        let candidate = track("两不知", "南风叙", 216);
        let score = score_candidate(&candidate, "两不知", Some("南风叙"), Some(211.0));
        assert_eq!(score, 50 + 20 + 10, "差 5 秒只给部分分");
    }

    /// 候选列表按分数**降序**，且截断到 `MAX_CANDIDATES`。
    ///
    /// 用纯排序逻辑验证（不打网络）：构造一批候选，按同样的排序规则处理。
    #[test]
    fn candidates_sort_descending_and_truncate() {
        let mut scored: Vec<MatchOutcome> = vec![
            MatchOutcome {
                track: track("低分", "X", 100),
                score: 30,
            },
            MatchOutcome {
                track: track("高分", "X", 100),
                score: 80,
            },
            MatchOutcome {
                track: track("中分", "X", 100),
                score: 50,
            },
        ];
        scored.sort_by_key(|outcome| std::cmp::Reverse(outcome.score));
        scored.truncate(MAX_CANDIDATES);
        assert_eq!(scored[0].score, 80, "最高分在最前");
        assert_eq!(scored[1].score, 50);
        assert_eq!(scored[2].score, 30);
        assert!(scored.len() <= MAX_CANDIDATES);
    }
}

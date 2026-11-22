use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Speaker {
    #[serde(rename = "我")]
    Me,
    #[serde(rename = "对方")]
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Message {
    pub speaker: Speaker,
    pub text: String,
}

pub struct AttachedImage {
    pub message_number: usize,
    pub data_url: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HistorySource {
    Screen,
    WeFlowWithFallback,
    WeFlowOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HistoryLimit {
    Messages(usize),
    Rounds(usize),
}

impl HistoryLimit {
    pub fn count(self) -> usize {
        match self {
            Self::Messages(count) | Self::Rounds(count) => count,
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Self::Messages(_) => "条",
            Self::Rounds(_) => "轮",
        }
    }

    pub fn select(self, messages: &[Message]) -> (&[Message], usize) {
        match self {
            Self::Messages(count) => {
                let recent = &messages[messages.len().saturating_sub(count)..];
                (recent, recent.len())
            }
            Self::Rounds(count) => recent_rounds(messages, count),
        }
    }
}

pub fn parse_transcript(text: &str) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let entry = [("我", Speaker::Me), ("对方", Speaker::Other)]
            .into_iter()
            .find_map(|(label, speaker)| {
                let rest = line.strip_prefix(label)?;
                let rest = rest.strip_prefix(['：', ':'])?;
                Some((speaker, rest.trim()))
            });
        if let Some((speaker, text)) = entry {
            if !text.is_empty() {
                messages.push(Message {
                    speaker,
                    text: text.into(),
                });
            }
        } else if let Some(last) = messages.last_mut() {
            last.text.push('\n');
            last.text.push_str(line);
        }
    }
    messages
}

fn recent_rounds(messages: &[Message], limit: usize) -> (&[Message], usize) {
    if limit == 0 {
        return (&[], 0);
    }
    let mut groups = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if groups
            .last()
            .is_none_or(|&(_, speaker)| speaker != message.speaker)
        {
            groups.push((index, message.speaker));
        }
    }
    let mut rounds = 0;
    for pair in groups.windows(2).rev() {
        if pair[0].1 == Speaker::Other && pair[1].1 == Speaker::Me {
            rounds += 1;
            if rounds == limit {
                return (&messages[pair[0].0..], rounds);
            }
        }
    }
    (messages, rounds)
}

pub fn merge_visible(cache: &mut Vec<Message>, visible: &[Message]) {
    if visible.is_empty() || cache.ends_with(visible) {
        return;
    }
    let max_overlap = cache.len().min(visible.len());
    if let Some(overlap) = (1..=max_overlap)
        .rev()
        .find(|&count| cache.ends_with(&visible[..count]))
    {
        cache.extend_from_slice(&visible[overlap..]);
    } else if let Some(overlap) = (1..=max_overlap)
        .rev()
        .find(|&count| visible.ends_with(&cache[..count]))
    {
        cache.splice(..0, visible[..visible.len() - overlap].iter().cloned());
    } else if !cache.windows(visible.len()).any(|window| window == visible) {
        *cache = visible.to_vec();
    }
    if cache.len() > 2000 {
        cache.drain(..cache.len() - 2000);
    }
}

pub fn prepend_visible(cache: &mut Vec<Message>, older: &[Message]) -> bool {
    let overlap = (1..=cache.len().min(older.len()))
        .rev()
        .find(|&count| older.ends_with(&cache[..count]));
    if let Some(overlap) = overlap {
        cache.splice(..0, older[..older.len() - overlap].iter().cloned());
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_exchanges_and_preserves_pending_messages() {
        let messages = parse_transcript(
            "我：在吗\n对方：在\n对方：你说\n我：几点走\n对方：七点\n我：好\n我：门口见\n对方：带伞",
        );
        let (recent, count) = recent_rounds(&messages, 1);
        assert_eq!(count, 1);
        assert_eq!(recent, &messages[4..]);
        assert_eq!(recent_rounds(&messages, 20).1, 2);
    }

    #[test]
    fn message_limit_keeps_latest_fifty_in_order() {
        let messages: Vec<_> = (0..56)
            .map(|index| Message {
                speaker: if index < 30 {
                    Speaker::Me
                } else {
                    Speaker::Other
                },
                text: format!("消息 {index}"),
            })
            .collect();
        let limit = HistoryLimit::Messages(50);
        assert_eq!(limit.select(&messages), (&messages[6..], 50));
        assert_eq!(limit.select(&messages[..12]), (&messages[..12], 12));
    }

    #[test]
    fn preserves_repeated_bubbles_in_overlapping_viewports() {
        let all = parse_transcript("对方：几点\n我：七点\n对方：好\n对方：好\n我：到了");
        let mut cache = all[2..].to_vec();
        merge_visible(&mut cache, &all[..4]);
        merge_visible(&mut cache, &all[2..]);
        assert_eq!(cache, all);
    }

    #[test]
    fn ignores_unconnected_older_pages() {
        let all = parse_transcript("对方：第一条\n我：第二条\n对方：第三条\n我：第四条");
        let mut recent = all[2..].to_vec();
        assert!(!prepend_visible(&mut recent, &all[..2]));
        assert_eq!(recent, all[2..]);
        assert!(prepend_visible(&mut recent, &all[..3]));
        assert_eq!(recent, all);
    }
}

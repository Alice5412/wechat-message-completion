use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const SESSION_LIMIT: usize = 90;
pub const MESSAGES_PER_SESSION: usize = 200;
pub const PROMPT_LIMIT: usize = 1600;

const REPLIES: [&str; 16] = [
    "好",
    "好的",
    "嗯",
    "嗯嗯",
    "行",
    "可以",
    "收到",
    "没问题",
    "好嘞",
    "行啊",
    "对",
    "哈哈",
    "哈哈哈",
    "ok",
    "好呀",
    "是的",
];
const PARTICLES: [char; 6] = ['哈', '吧', '啊', '呀', '呢', '啦'];

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Profile {
    pub prompt: String,
    pub messages: usize,
    pub conversations: usize,
    pub skipped: usize,
    pub archive: bool,
}

impl Profile {
    pub fn description(&self) -> String {
        if self.messages == 0 {
            return "可从微信聊天学习，也可直接输入自己的风格要求".into();
        }
        let source = if self.archive {
            "历史扫描"
        } else {
            "近期采样"
        };
        let mut label = format!(
            "{source} · {} 个私聊 · {} 条本人文字",
            self.conversations, self.messages
        );
        if self.skipped > 0 {
            label.push_str(&format!(" · {} 个会话读取失败", self.skipped));
        }
        label
    }
}

#[derive(Default)]
pub struct Samples {
    lengths: Vec<usize>,
    unpunctuated: usize,
    multiline: usize,
    emoji: usize,
    question_marks: usize,
    exclamations: usize,
    replies: [usize; REPLIES.len()],
    particles: [usize; PARTICLES.len()],
}

impl Samples {
    pub fn count(&self) -> usize {
        self.lengths.len()
    }

    pub fn observe(&mut self, text: &str) {
        let text = text.trim();
        let length = text.chars().filter(|ch| !ch.is_whitespace()).count();
        if length == 0 || length > 500 || text.contains("://") || text.starts_with(['[', '<']) {
            return;
        }
        self.lengths.push(length);
        let ending = text.chars().next_back().unwrap();
        let punctuation = "。！？；：，、…～~.!?,;:".contains(ending);
        self.unpunctuated += usize::from(!punctuation);
        self.multiline += usize::from(text.lines().count() > 1);
        self.emoji += usize::from(
            text.chars()
                .any(|ch| matches!(ch as u32, 0x1f300..=0x1faff | 0x2600..=0x27bf)),
        );
        self.question_marks += usize::from(matches!(ending, '？' | '?'));
        self.exclamations += usize::from(matches!(ending, '！' | '!'));

        let spoken = text.trim_end_matches(|ch| "。！？；：，、…～~.!?,;:".contains(ch));
        if let Some(index) = REPLIES
            .iter()
            .position(|reply| spoken.eq_ignore_ascii_case(reply))
        {
            self.replies[index] += 1;
        } else if let Some(index) = spoken
            .chars()
            .next_back()
            .and_then(|last| PARTICLES.iter().position(|&particle| particle == last))
        {
            self.particles[index] += 1;
        }
    }

    pub fn profile(mut self, conversations: usize, skipped: usize) -> Result<Profile> {
        let messages = self.count();
        ensure!(messages > 0, "没有读到可用于学习的本人文字消息");
        self.lengths.sort_unstable();
        let median = self.lengths[(messages - 1) / 2];
        let typical_upper = self.lengths[(messages - 1) * 4 / 5];
        let mut lines = vec![format!(
            "平时的单条回复约 {median} 字，多数在 {typical_upper} 字以内。简单回应简短直接，需要解释时按当前问题展开。"
        )];
        if self.unpunctuated * 3 >= messages * 2 {
            lines.push("聊天时常省略句末标点，短句直接收尾。".into());
        }
        if self.question_marks > self.exclamations && self.question_marks * 10 >= messages {
            lines.push("询问时保留问号，语气平实。".into());
        }
        if self.multiline * 10 < messages {
            lines.push("单条回复里很少换行。".into());
        }
        if self.emoji * 12 < messages {
            lines.push("表情使用较少，语境合适时再加。".into());
        }
        let mut replies = REPLIES
            .into_iter()
            .zip(self.replies)
            .filter(|&(_, count)| count >= 3 && count * 100 >= messages)
            .collect::<Vec<_>>();
        replies.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
        let replies = replies
            .into_iter()
            .take(4)
            .map(|(word, _)| word)
            .collect::<Vec<_>>();
        if !replies.is_empty() {
            lines.push(format!(
                "口语短回复常用「{}」，自然使用。",
                replies.join("」「")
            ));
        }
        let particles = PARTICLES
            .into_iter()
            .zip(self.particles)
            .filter(|&(_, count)| count >= 4 && count * 25 >= messages)
            .map(|(particle, _)| particle.to_string())
            .collect::<Vec<_>>();
        if !particles.is_empty() {
            lines.push(format!(
                "句尾有时带「{}」这类语气词。",
                particles.join("」「")
            ));
        }
        Ok(Profile {
            prompt: lines.join("\n"),
            messages,
            conversations,
            skipped,
            archive: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_uses_aggregate_habits_without_quoting_messages() {
        let mut sample = Samples::default();
        for text in [
            "嗯嗯",
            "嗯嗯",
            "嗯嗯",
            "这是一条只出现一次的私人示例",
            "[文件] 文档.docx",
            "https://example.test",
        ] {
            sample.observe(text);
        }
        let profile = sample.profile(2, 0).unwrap();
        assert_eq!(profile.messages, 4);
        assert_eq!(profile.conversations, 2);
        assert!(profile.prompt.contains("嗯嗯"));
        assert!(!profile.prompt.contains("私人示例"));
        assert!(!profile.prompt.contains("example.test"));
    }
}

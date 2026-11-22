use roxmltree::{Document, Node, ParsingOptions};

const ENVELOPES: &[(&str, &str)] = &[
    ("msg", "[微信消息]"),
    ("appmsg", "[微信消息]"),
    ("sysmsg", "[系统消息]"),
    ("img", "[图片]"),
    ("emoji", "[表情包]"),
    ("voicemsg", "[语音]"),
    ("videomsg", "[视频]"),
    ("location", "[位置]"),
];

pub fn message_text(
    local_type: Option<i64>,
    parsed: Option<String>,
    raw: Option<String>,
) -> String {
    raw.as_deref()
        .and_then(|text| summarize_envelope(text, local_type))
        .or_else(|| {
            parsed
                .as_deref()
                .and_then(|text| summarize_envelope(text, local_type))
        })
        .or_else(|| parsed.filter(|text| !text.trim().is_empty()))
        .or_else(|| raw.filter(|text| !text.trim().is_empty()))
        .unwrap_or_else(|| "[消息]".into())
}

fn summarize_envelope(text: &str, local_type: Option<i64>) -> Option<String> {
    let text = text.trim().trim_start_matches('\u{feff}').trim_start();
    let body = match text.split_once(':') {
        Some((sender, body)) if sender.starts_with("wxid_") && body.starts_with(['\r', '\n']) => {
            body.trim_start()
        }
        _ => text,
    };
    let root_start = body
        .strip_prefix("<?xml")
        .and_then(|declaration| declaration.split_once("?>"))
        .map_or(body, |(_, root)| root.trim_start());
    let kind = local_type.unwrap_or_default();
    let is_file = (kind as u32) == 49 && (kind >> 32) == 6;
    let fallback = ENVELOPES
        .iter()
        .find_map(|&(tag, label)| starts_with_tag(root_start, tag).then_some(label))
        .or_else(|| {
            (is_file && root_start.starts_with('<')
                || has_attachment_fields(body)
                || ENVELOPES.iter().any(|(tag, _)| {
                    body.strip_prefix("&lt;")
                        .and_then(|text| text.strip_prefix(tag))
                        .is_some_and(|rest| rest.starts_with("&gt;") || rest.starts_with(' '))
                }))
            .then_some("[微信消息]")
        })?;
    let fallback = if is_file { "[文件]" } else { fallback };
    let options = ParsingOptions {
        nodes_limit: 2048,
        ..ParsingOptions::default()
    };
    let document = (body.len() <= 128 * 1024)
        .then(|| Document::parse_with_options(body, options).ok())
        .flatten();
    let Some(document) = document else {
        return Some(fallback.into());
    };
    let root = document.root_element();
    let app = if root.has_tag_name("appmsg") {
        Some(root)
    } else {
        root.children().find(|node| node.has_tag_name("appmsg"))
    };
    if let Some(app) = app {
        let (label, use_title) = match child_text(app, "type").trim() {
            "6" => ("[文件]", true),
            "57" => ("[引用]", false),
            "19" => ("[聊天记录]", false),
            "2000" | "2001" => ("[转账]", false),
            "33" | "36" => ("[小程序]", false),
            _ if is_file => ("[文件]", true),
            _ => ("[链接]", true),
        };
        return Some(if use_title {
            with_title(label, &child_text(app, "title"))
        } else {
            label.into()
        });
    }
    Some(
        ENVELOPES
            .iter()
            .skip(2)
            .find_map(|&(tag, label)| {
                root.children()
                    .any(|node| node.has_tag_name(tag))
                    .then_some(label)
            })
            .unwrap_or(fallback)
            .into(),
    )
}

fn starts_with_tag(text: &str, tag: &str) -> bool {
    text.strip_prefix('<')
        .and_then(|text| text.strip_prefix(tag))
        .and_then(|text| text.chars().next())
        .is_some_and(|ch| ch.is_ascii_whitespace() || ch == '>' || ch == '/')
}

fn has_attachment_fields(text: &str) -> bool {
    [
        "<aeskey",
        "<fileuploadtoken",
        "<cdnattachurl",
        "<appattach",
        "&lt;aeskey",
        "&lt;fileuploadtoken",
        "&lt;cdnattachurl",
        "&lt;appattach",
    ]
    .into_iter()
    .any(|tag| text.contains(tag))
}

fn child_text(node: Node<'_, '_>, tag: &str) -> String {
    node.children()
        .find(|child| child.has_tag_name(tag))
        .map(|child| {
            child
                .children()
                .filter(Node::is_text)
                .filter_map(|text| text.text())
                .collect()
        })
        .unwrap_or_default()
}

fn with_title(label: &str, title: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() || title.contains(['<', '>']) || has_attachment_fields(&title) {
        return label.into();
    }
    format!("{label} {}", title.chars().take(160).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_messages_keep_only_type_and_name() {
        for name in [
            "安排 &amp; 备注.docx",
            "数据.xlsx",
            "讲义.pptx",
            "材料.pdf",
            "规则.yaml",
        ] {
            let xml = format!(
                "<msg><appmsg><title>{name}</title><type>6</type><appattach><aeskey>fake-secret</aeskey><fileuploadtoken>fake-token</fileuploadtoken></appattach></appmsg></msg>"
            );
            let expected = format!("[文件] {}", name.replace("&amp;", "&"));
            assert_eq!(
                message_text(Some(49), Some("[链接]".into()), Some(xml.clone())),
                expected
            );
            assert_eq!(
                message_text(Some((6 << 32) | 49), Some(xml), None),
                expected
            );
        }
    }

    #[test]
    fn structured_and_malformed_messages_do_not_pass_through() {
        assert_eq!(
            message_text(
                Some((6 << 32) | 49),
                None,
                Some("<msg><appmsg><aeskey>fake".into())
            ),
            "[文件]"
        );
        assert_eq!(
            message_text(
                None,
                Some("<sysmsg><revokemsg>system data</revokemsg></sysmsg>".into()),
                None
            ),
            "[系统消息]"
        );
        assert_eq!(
            message_text(None, None, Some("wxid_sample:\n<msg><appmsg><type>57</type><refermsg>hidden</refermsg></appmsg></msg>".into())),
            "[引用]"
        );
        assert_eq!(
            message_text(
                None,
                None,
                Some("<broken><fileuploadtoken>fake</fileuploadtoken>".into())
            ),
            "[微信消息]"
        );
        assert_eq!(
            message_text(Some(1), None, Some("我在看 <h1> 的用法".into())),
            "我在看 <h1> 的用法"
        );
    }
}

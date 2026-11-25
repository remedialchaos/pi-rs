use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeTokenDescriptor {
    pub slot: ThemeTokenSlot,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeTokenSlot {
    Foreground,
    Background,
}

pub type ThemeTokenDescriptors = IndexMap<String, ThemeTokenDescriptor>;

pub fn define_theme_tokens<const T: usize>(
    tokens: [(&str, ThemeTokenDescriptor); T],
) -> ThemeTokenDescriptors {
    let mut map = IndexMap::new();
    for (name, descriptor) in tokens {
        map.insert(name.to_string(), descriptor);
    }
    map
}

pub type ThemeToken = String;

pub type ThemeColor = String;
pub type ThemeBg = String;

pub type ThemeColorValues<Value> = HashMap<String, Value>;
pub type ResolvedThemeColorValues<Value> = HashMap<String, Value>;
pub static THEME_TOKENS: std::sync::LazyLock<ThemeTokenDescriptors> = std::sync::LazyLock::new(
    || {
        define_theme_tokens([
("accent",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Primary accent color (logo, selected items, cursor)".into(),fallback:None}),
("border",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Normal borders".into(),fallback:None}),
("borderAccent",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Highlighted borders".into(),fallback:None}),
("borderMuted",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Subtle borders".into(),fallback:None}),
("success",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Success states".into(),fallback:None}),
("error",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Error states".into(),fallback:None}),
("warning",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Warning states".into(),fallback:None}),
("muted",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Secondary/dimmed text".into(),fallback:None}),
("dim",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Very dimmed text (more subtle than muted)".into(),fallback:None}),
("text",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Default text color (usually empty string)".into(),fallback:None}),
("thinkingText",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Thinking block text color".into(),fallback:None}),
("scrollbarTrack",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Fullscreen scrollbar track foreground (falls back to muted when omitted)".into(),fallback:Some("muted".into())}),
("scrollbarThumb",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Fullscreen scrollbar thumb foreground (falls back to text when omitted)".into(),fallback:Some("text".into())}),
("selectedBg",ThemeTokenDescriptor{slot:ThemeTokenSlot::Background,description:"Selected item background".into(),fallback:None}),
("searchMatchBg",ThemeTokenDescriptor{slot:ThemeTokenSlot::Background,description:"Transcript search match background and current-match text (falls back to selectedBg when omitted)".into(),fallback:Some("selectedBg".into())}),
("searchMatchText",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Transcript search match text and current-match background (falls back to text when omitted)".into(),fallback:Some("text".into())}),
("userMessageBg",ThemeTokenDescriptor{slot:ThemeTokenSlot::Background,description:"User message background".into(),fallback:None}),
("userMessageText",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"User message text color".into(),fallback:None}),
("customMessageBg",ThemeTokenDescriptor{slot:ThemeTokenSlot::Background,description:"Custom message background (hook-injected messages)".into(),fallback:None}),
("customMessageText",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Custom message text color".into(),fallback:None}),
("customMessageLabel",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Custom message type label color".into(),fallback:None}),
("toolPendingBg",ThemeTokenDescriptor{slot:ThemeTokenSlot::Background,description:"Tool execution box (pending state)".into(),fallback:None}),
("toolSuccessBg",ThemeTokenDescriptor{slot:ThemeTokenSlot::Background,description:"Tool execution box (success state)".into(),fallback:None}),
("toolErrorBg",ThemeTokenDescriptor{slot:ThemeTokenSlot::Background,description:"Tool execution box (error state)".into(),fallback:None}),
("toolTitle",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Tool execution box title color".into(),fallback:None}),
("toolOutput",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Tool execution box output text color".into(),fallback:None}),
("mdHeading",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown heading text".into(),fallback:None}),
("mdLink",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown link text".into(),fallback:None}),
("mdLinkUrl",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown link URL".into(),fallback:None}),
("mdCode",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown inline code".into(),fallback:None}),
("mdCodeBlock",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown code block content".into(),fallback:None}),
("mdCodeBlockBorder",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown code block fences".into(),fallback:None}),
("mdQuote",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown blockquote text".into(),fallback:None}),
("mdQuoteBorder",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown blockquote border".into(),fallback:None}),
("mdHr",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown horizontal rule".into(),fallback:None}),
("mdListBullet",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Markdown list bullets/numbers".into(),fallback:None}),
("toolDiffAdded",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Added lines in tool diffs".into(),fallback:None}),
("toolDiffRemoved",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Removed lines in tool diffs".into(),fallback:None}),
("toolDiffContext",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Context lines in tool diffs".into(),fallback:None}),
("syntaxComment",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: comments".into(),fallback:None}),
("syntaxKeyword",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: keywords".into(),fallback:None}),
("syntaxFunction",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: function names".into(),fallback:None}),
("syntaxVariable",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: variable names".into(),fallback:None}),
("syntaxString",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: string literals".into(),fallback:None}),
("syntaxNumber",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: number literals".into(),fallback:None}),
("syntaxType",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: type names".into(),fallback:None}),
("syntaxOperator",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: operators".into(),fallback:None}),
("syntaxPunctuation",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Syntax highlighting: punctuation".into(),fallback:None}),
("thinkingOff",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Thinking level border: off".into(),fallback:None}),
("thinkingMinimal",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Thinking level border: minimal".into(),fallback:None}),
("thinkingLow",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Thinking level border: low".into(),fallback:None}),
("thinkingMedium",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Thinking level border: medium".into(),fallback:None}),
("thinkingHigh",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Thinking level border: high".into(),fallback:None}),
("thinkingXhigh",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Thinking level border: xhigh".into(),fallback:None}),
("thinkingMax",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Thinking level border: max (falls back to thinkingXhigh when omitted)".into(),fallback:Some("thinkingXhigh".into())}),
("bashMode",ThemeTokenDescriptor{slot:ThemeTokenSlot::Foreground,description:"Editor border color in bash mode".into(),fallback:None})
])
    },
);

use crate::render_context::{RenderContext, line_text, safe_text};
use pi_rs_tui::{Component, Container, Markdown, Text};

#[derive(Debug, Clone)]
pub struct ParsedSkillBlock {
    pub name: String,
    pub location: String,
    pub content: String,
    pub user_message: Option<String>,
}

pub fn parse_skill_block(text: &str) -> Option<ParsedSkillBlock> {
    // Simple parser without regex
    if !text.starts_with("<skill name=") {
        return None;
    }
    // Find the closing </skill> tag
    let end_tag = "</skill>";
    let end_pos = text.find(end_tag)?;
    let skill_content = &text[..end_pos];

    // Parse name and location from opening tag
    let name_start = skill_content.find("name=\"")? + 6;
    let name_end = skill_content[name_start..].find("\"")? + name_start;
    let name = skill_content[name_start..name_end].to_string();

    let location_start = skill_content.find("location=\"")? + 10;
    let location_end = skill_content[location_start..].find("\"")? + location_start;
    let location = skill_content[location_start..location_end].to_string();

    // Get content after the first newline
    let content_start = skill_content.find("\n")? + 1;
    let content = skill_content[content_start..].trim().to_string();

    // Get user message after </skill>
    let user_message = if text.len() > end_pos + end_tag.len() {
        let after = &text[end_pos + end_tag.len()..];
        let trimmed = after.trim_start_matches('\n');
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    } else {
        None
    };

    Some(ParsedSkillBlock {
        name,
        location,
        content,
        user_message,
    })
}

pub struct SkillInvocationComponent {
    expanded: bool,
    block: ParsedSkillBlock,
    ctx: Box<dyn RenderContext>,
}

impl SkillInvocationComponent {
    pub fn new(block: ParsedSkillBlock, ctx: Box<dyn RenderContext>, _output_pad: usize) -> Self {
        Self {
            expanded: false,
            block,
            ctx,
        }
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        if expanded == self.expanded {
            return;
        }
        self.expanded = expanded;
    }

    fn display(&self) -> Container {
        let theme = self.ctx.theme().current();
        let mut content = Container::default();

        let label = theme.fg("customMessageLabel", &theme.bold("[skill]"));
        if self.expanded {
            content.add_child(pi_rs_tui::ComponentRef::new(Text::new(label, 0, 0, None)));
            content.add_child(pi_rs_tui::ComponentRef::new(Markdown::new(
                format!(
                    "**{}**\n\n{}",
                    safe_text(&self.block.name),
                    safe_text(&self.block.content)
                ),
                0,
                0,
                self.ctx.theme().markdown(),
                Default::default(),
                Default::default(),
            )));
        } else {
            content.add_child(pi_rs_tui::ComponentRef::new(Text::new(
                format!(
                    "{} {} {}",
                    label,
                    theme.fg("customMessageText", &line_text(&self.block.name)),
                    theme.fg(
                        "dim",
                        &format!(" ({} to expand)", self.ctx.key_text("app.tools.expand"))
                    )
                ),
                0,
                0,
                None,
            )));
        }

        content
    }
}

impl Component for SkillInvocationComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.display().render(width)
    }

    fn invalidate(&mut self) {}
}

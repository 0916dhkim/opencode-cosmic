use cosmic::Element;
use cosmic::iced::widget::rich_text;
use cosmic::iced::widget::text::Span;
use cosmic::iced::{Border, Length};
use cosmic::widget::{button, column, container, row, text};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::metrics::{em, space};
use crate::palette;

/// GTK's line heights for the transcript: `.message-plain-text` and
/// `.markdown-paragraph` are 1.45, `.markdown-heading` 1.2 and the code
/// content inherits `.message-content`'s 1.35.
fn body_line_height() -> cosmic::iced::core::text::LineHeight {
    crate::metrics::line_height(1.45)
}

fn heading_line_height() -> cosmic::iced::core::text::LineHeight {
    crate::metrics::line_height(1.2)
}

fn code_line_height() -> cosmic::iced::core::text::LineHeight {
    crate::metrics::line_height(1.35)
}

/// One styled run of inline text.
///
/// GTK built a Pango markup string from the same events (`<b>`, `<i>`,
/// `<span font_family="monospace">`, `<span strikethrough="true">`, links), and
/// its label rendered it. iced's `rich_text` takes spans, so the run keeps the
/// style instead of the markup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Inline {
    Plain,
    Bold,
    Italic,
    Strike,
    Code,
    Link,
}

#[derive(Clone, Debug)]
struct Run {
    text: String,
    inline: Inline,
}

/// Trailing/leading whitespace around a block is dropped, as GTK's
/// `current_text.trim()` did.
fn trim_runs(runs: &mut Vec<Run>) {
    if let Some(first) = runs.first_mut() {
        first.text = first.text.trim_start().to_string();
    }
    if let Some(last) = runs.last_mut() {
        last.text = last.text.trim_end().to_string();
    }
    runs.retain(|run| !run.text.is_empty());
}

fn runs_are_empty(runs: &[Run]) -> bool {
    runs.iter().all(|run| run.text.trim().is_empty())
}

#[derive(Clone, Debug)]
enum MarkdownBlock {
    Paragraph(Vec<Run>),
    Heading(u8, Vec<Run>),
    Code(Option<String>, String),
    List(Vec<Vec<Run>>),
    Blockquote(Vec<Run>),
    Rule,
}

fn parse_markdown(source: &str) -> Vec<MarkdownBlock> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(source, options);
    let mut blocks = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut styles: Vec<Inline> = Vec::new();
    let mut current_code_lang = None;
    let mut current_heading_level = None;
    let mut in_blockquote = false;
    let mut current_list_items: Vec<Vec<Run>> = Vec::new();
    let mut in_list = false;

    let push_run = |runs: &mut Vec<Run>, text: &str, styles: &[Inline]| {
        let inline = styles.last().copied().unwrap_or(Inline::Plain);
        match runs.last_mut() {
            // Adjacent runs with the same style render as one span.
            Some(last) if last.inline == inline => last.text.push_str(text),
            _ => runs.push(Run {
                text: text.to_string(),
                inline,
            }),
        }
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current_heading_level = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                });
                runs.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(level) = current_heading_level.take() {
                    trim_runs(&mut runs);
                    blocks.push(MarkdownBlock::Heading(level, std::mem::take(&mut runs)));
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                current_code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        let l = lang.trim();
                        if l.is_empty() {
                            None
                        } else {
                            Some(l.to_string())
                        }
                    }
                    CodeBlockKind::Indented => None,
                };
                // A fenced block's text arrives as Text events; collect it raw.
                runs.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                let lang = current_code_lang.take();
                let code = runs.iter().map(|run| run.text.as_str()).collect::<String>();
                blocks.push(MarkdownBlock::Code(lang, code));
                runs.clear();
            }
            Event::Start(Tag::List(_)) => {
                in_list = true;
                current_list_items.clear();
            }
            Event::End(TagEnd::List(_)) => {
                in_list = false;
                if !current_list_items.is_empty() {
                    blocks.push(MarkdownBlock::List(std::mem::take(&mut current_list_items)));
                }
            }
            Event::Start(Tag::Item) => {
                runs.clear();
            }
            Event::End(TagEnd::Item) => {
                trim_runs(&mut runs);
                if in_list && !runs.is_empty() {
                    current_list_items.push(std::mem::take(&mut runs));
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                in_blockquote = true;
                runs.clear();
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                in_blockquote = false;
                trim_runs(&mut runs);
                if !runs.is_empty() {
                    blocks.push(MarkdownBlock::Blockquote(std::mem::take(&mut runs)));
                }
            }
            Event::Start(Tag::Paragraph) => {
                runs.clear();
            }
            Event::End(TagEnd::Paragraph) => {
                if !in_list && !in_blockquote {
                    trim_runs(&mut runs);
                    if !runs.is_empty() {
                        blocks.push(MarkdownBlock::Paragraph(std::mem::take(&mut runs)));
                    } else {
                        runs.clear();
                    }
                }
            }
            Event::Start(Tag::Strong) => styles.push(Inline::Bold),
            Event::End(TagEnd::Strong) => {
                styles.pop();
            }
            Event::Start(Tag::Emphasis) => styles.push(Inline::Italic),
            Event::End(TagEnd::Emphasis) => {
                styles.pop();
            }
            Event::Start(Tag::Strikethrough) => styles.push(Inline::Strike),
            Event::End(TagEnd::Strikethrough) => {
                styles.pop();
            }
            Event::Start(Tag::Link { .. }) => styles.push(Inline::Link),
            Event::End(TagEnd::Link) => {
                styles.pop();
            }
            Event::Text(t) => push_run(&mut runs, &t, &styles),
            // GTK rendered an inline code span as monospace, with no backticks.
            Event::Code(c) => push_run(&mut runs, &c, &[Inline::Code]),
            Event::SoftBreak | Event::HardBreak => push_run(&mut runs, "\n", &styles),
            Event::Rule => {
                blocks.push(MarkdownBlock::Rule);
            }
            _ => {}
        }
    }

    trim_runs(&mut runs);
    if !runs.is_empty() {
        blocks.push(MarkdownBlock::Paragraph(runs));
    }

    blocks
}

/// One run as an iced span: GTK put these styles in Pango markup.
///
/// `base_bold` carries a block-level weight (GTK's `.markdown-heading` is 700,
/// which it applied to the whole label).
fn span<'a>(run: &Run, base_bold: bool) -> Span<'a, ()> {
    let mut span = Span::new(run.text.clone());
    match run.inline {
        Inline::Plain if base_bold => {
            span = span.font(cosmic::iced::Font {
                weight: cosmic::iced::font::Weight::Bold,
                ..cosmic::iced::Font::DEFAULT
            });
        }
        Inline::Plain => {}
        Inline::Bold => {
            span = span.font(cosmic::iced::Font {
                weight: cosmic::iced::font::Weight::Bold,
                ..cosmic::iced::Font::DEFAULT
            });
        }
        Inline::Italic => {
            span = span.font(cosmic::iced::Font {
                style: cosmic::iced::font::Style::Italic,
                ..cosmic::iced::Font::DEFAULT
            });
        }
        Inline::Strike => span = span.strikethrough(true),
        Inline::Code => span = span.font(cosmic::iced::Font::MONOSPACE),
        Inline::Link => {
            span = span.color(palette::current().link_text).underline(true);
        }
    }
    span
}

fn spans<'a>(runs: &[Run], base_bold: bool) -> Vec<Span<'a, ()>> {
    runs.iter().map(|run| span(run, base_bold)).collect()
}

pub fn render_markdown<'a, Message: Clone + 'static, F>(
    source: &str,
    on_copy: F,
    zoom: f32,
) -> Element<'a, Message>
where
    F: Fn(String) -> Message + Copy + 'static,
{
    let blocks = parse_markdown(source);
    let mut elements = Vec::with_capacity(blocks.len());

    for block in blocks {
        match block {
            MarkdownBlock::Paragraph(runs) => {
                elements.push(
                    rich_text(spans(&runs, false))
                        .size(em(0.96, zoom))
                        .line_height(body_line_height())
                        .into(),
                );
            }
            MarkdownBlock::Heading(level, runs) => {
                // GTK: .markdown-heading-1/2/3 = 1.45 / 1.28 / 1.14em.
                let size = match level {
                    1 => em(1.45, zoom),
                    2 => em(1.28, zoom),
                    3 => em(1.14, zoom),
                    _ => em(0.96, zoom),
                };
                elements.push(
                    rich_text(spans(&runs, false))
                        .size(size)
                        .line_height(heading_line_height())
                        .class(cosmic::theme::Text::Color(
                            palette::current().header_title_text,
                        ))
                        .into(),
                );
            }
            MarkdownBlock::Code(lang, code) => {
                let code_for_copy = code.clone();
                let lang_label = lang.unwrap_or_else(|| "code".to_string());
                let header = container(
                    row::with_children(vec![
                        text(lang_label)
                            .size(em(0.76, zoom))
                            .font(cosmic::iced::Font {
                                weight: cosmic::iced::font::Weight::Bold,
                                ..cosmic::iced::Font::DEFAULT
                            })
                            .width(Length::Fill)
                            .into(),
                        button::icon(crate::icons::copy())
                            // GTK's `button.markdown-code-copy` is 1.93em
                            // square (25.7px), which sets the header's height;
                            // the icon is 12px, so the padding carries the rest.
                            .padding([space(0.52, zoom) as u16, space(0.52, zoom) as u16])
                            .on_press(on_copy(code_for_copy))
                            .into(),
                    ])
                    .align_y(cosmic::iced::Alignment::Center),
                )
                .padding([space(0.3, zoom) as u16, space(0.59, zoom) as u16])
                .style(|_theme| container::Style {
                    background: Some(palette::current().code_header_bg.into()),
                    border: Border {
                        color: palette::current().panel_border,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    text_color: Some(palette::current().code_language_text),
                    ..Default::default()
                });

                let code_text = text(code)
                    .font(cosmic::iced::Font::MONOSPACE)
                    .size(em(0.92, zoom))
                    .line_height(code_line_height());

                let code_container = container(code_text)
                    // GTK's scrolled window around the code label: 10px above,
                    // 12px below and beside.
                    .padding([10.0, 12.0, 12.0, 12.0])
                    .width(Length::Fill)
                    .style(|_theme| container::Style {
                        text_color: Some(palette::current().code_content_text),
                        ..Default::default()
                    });

                let block_col = column::with_children(vec![header.into(), code_container.into()]);

                let code_block_container =
                    container(block_col)
                        .width(Length::Fill)
                        .style(|_theme| container::Style {
                            background: Some(palette::current().code_block_bg.into()),
                            border: Border {
                                color: palette::current().code_block_border,
                                width: 1.0,
                                radius: 6.0.into(),
                            },
                            ..Default::default()
                        });

                elements.push(code_block_container.into());
            }
            MarkdownBlock::List(items) => {
                // GTK appends each item as its own block in `.message-content`, so
                // the items sit the content box's 10px apart.
                let mut list_col = column::with_capacity(items.len()).spacing(space(0.75, zoom));
                // GTK's `.markdown-list-marker` is a `min-width: 1.33em` box —
                // 13px in the rendered app. The bullet sits at the box's left
                // edge and the item text starts at its right, so the marker is
                // a box, not a glyph followed by a space (which measured 8px
                // against GTK's 13).
                let marker_box = space(0.975, zoom);
                for item in items {
                    let bullet_item = row::with_children(vec![
                        container(
                            text("•")
                                .size(em(0.96, zoom))
                                .line_height(body_line_height())
                                .font(cosmic::iced::Font {
                                    weight: cosmic::iced::font::Weight::Bold,
                                    ..cosmic::iced::Font::DEFAULT
                                })
                                .class(cosmic::theme::Text::Color(palette::current().muted_text)),
                        )
                        .width(Length::Fixed(marker_box))
                        .into(),
                        rich_text(spans(&item, false))
                            .size(em(0.96, zoom))
                            .line_height(body_line_height())
                            .width(Length::Fill)
                            .into(),
                    ]);
                    list_col = list_col.push(bullet_item);
                }
                elements.push(
                    // GTK's list items are blocks with `margin-start: 28`, and no
                    // vertical padding of their own.
                    container(list_col)
                        // GTK's list is indented 1.01em (measured against its
                        // own render) rather than the 0.59em used here.
                        .padding([0.0, space(0.59, zoom), 0.0, space(1.01, zoom)])
                        .into(),
                );
            }
            MarkdownBlock::Blockquote(runs) => {
                let q = container(
                    rich_text(spans(&runs, false))
                        .size(em(0.96, zoom))
                        .line_height(body_line_height()),
                )
                .padding([space(0.4, zoom) as u16, space(0.7, zoom) as u16])
                .style(|_theme| container::Style {
                    background: Some(palette::current().overlay_bg.into()),
                    border: Border {
                        color: palette::current().quote_border,
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    text_color: Some(palette::current().quote_text),
                    ..Default::default()
                });
                elements.push(q.into());
            }
            MarkdownBlock::Rule => {
                let rule = container(text(""))
                    .height(1)
                    .width(Length::Fill)
                    .style(|_theme| container::Style {
                        background: Some(palette::current().panel_border.into()),
                        ..Default::default()
                    });
                elements.push(rule.into());
            }
        }
    }

    // GTK appends every markdown block to `.message-content`, whose own
    // spacing is 10px, so the blocks sit 10px apart.
    column::with_children(elements).spacing(10).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(source: &str) -> String {
        parse_markdown(source)
            .into_iter()
            .filter_map(|block| match block {
                MarkdownBlock::Paragraph(runs) | MarkdownBlock::Blockquote(runs) => {
                    Some(runs.iter().map(|run| run.text.as_str()).collect::<String>())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn inline_code_keeps_its_text_without_backticks() {
        let blocks = parse_markdown("stay `34×32` wide");
        let MarkdownBlock::Paragraph(runs) = &blocks[0] else {
            panic!("expected a paragraph");
        };
        assert_eq!(runs[1].text, "34×32");
        assert_eq!(runs[1].inline, Inline::Code);
        assert!(!plain("stay `34×32` wide").contains('`'));
    }

    #[test]
    fn emphasis_and_links_become_styled_runs() {
        let blocks = parse_markdown("Draw at **22px**, *slightly* [smaller](#)");
        let MarkdownBlock::Paragraph(runs) = &blocks[0] else {
            panic!("expected a paragraph");
        };
        let styles: Vec<Inline> = runs.iter().map(|run| run.inline).collect();
        assert!(styles.contains(&Inline::Bold));
        assert!(styles.contains(&Inline::Italic));
        assert!(styles.contains(&Inline::Link));
        let text: String = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(text, "Draw at 22px, slightly smaller");
    }
}

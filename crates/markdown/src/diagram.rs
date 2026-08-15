//! UI shared by the Mermaid and D2 diagram renderers.

use gpui::{AnyElement, ClipboardItem, Entity, StyledText};
use std::sync::Arc;
use std::time::Duration;
use ui::{CopyButton, TintColor, prelude::*};
use util::ResultExt;

use super::{CopyButtonVisibility, Markdown};

fn diagram_element_id(
    prefix: &'static str,
    markdown: &Entity<Markdown>,
    offset: usize,
) -> ElementId {
    ElementId::NamedChild(
        Arc::new(ElementId::from((prefix, markdown.entity_id()))),
        offset.to_string().into(),
    )
}

/// A diagram's source, shown by the Code tab and whenever rendering is
/// unavailable.
pub(crate) fn render_diagram_code_view(contents: &SharedString) -> AnyElement {
    div()
        .w_full()
        .child(StyledText::new(contents.clone()))
        .into_any_element()
}

/// The Preview/Code tab pair shown above a diagram. Diagrams are keyed by their
/// source offset, which is unique across every code block in the document.
pub(crate) fn render_diagram_tab_header(
    source_offset: usize,
    showing_code: bool,
    markdown: Entity<Markdown>,
    set_showing_code: impl Fn(&mut Markdown, usize, bool) + Clone + 'static,
) -> impl IntoElement {
    let tab = |label: &'static str, id_prefix: &'static str, show_code: bool| {
        let id = diagram_element_id(id_prefix, &markdown, source_offset);
        let markdown = markdown.clone();
        let set_showing_code = set_showing_code.clone();
        Button::new(id, label)
            .label_size(LabelSize::Small)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .toggle_state(showing_code == show_code)
            .on_click(move |_event, _window, cx| {
                // Clicking the tab that is already selected does nothing.
                if showing_code == show_code {
                    return;
                }
                markdown.update(cx, |markdown, cx| {
                    set_showing_code(markdown, source_offset, show_code);
                    cx.notify();
                });
            })
    };

    h_flex()
        .gap_0p5()
        .mb_2p5()
        .child(tab("Preview", "diagram-tab-preview", false))
        .child(tab("Code", "diagram-tab-code", true))
}

/// The copy button anchored to the top-right corner of a diagram.
pub(crate) fn render_diagram_copy_button(
    id_prefix: &'static str,
    source_offset: usize,
    code: String,
    markdown: Entity<Markdown>,
    visibility: CopyButtonVisibility,
) -> AnyElement {
    let id = diagram_element_id(id_prefix, &markdown, source_offset);

    let button = CopyButton::new(id.clone(), code.clone()).custom_on_click({
        move |_window, cx| {
            let id = id.clone();
            markdown.update(cx, |markdown, cx| {
                markdown.copied_code_blocks.insert(id.clone());
                cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    this.update(cx, |markdown, cx| {
                        markdown.copied_code_blocks.remove(&id);
                        cx.notify();
                    })
                    .log_err();
                })
                .detach();
            });
        }
    });

    match visibility {
        CopyButtonVisibility::VisibleOnHover => {
            button.visible_on_hover("code_block").into_any_element()
        }
        CopyButtonVisibility::AlwaysVisible | CopyButtonVisibility::Hidden => {
            button.into_any_element()
        }
    }
}

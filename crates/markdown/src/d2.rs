use anyhow::Result;
use collections::{HashMap, HashSet};
use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, Entity, ImageSource, RenderImage, Task, img,
    pulsating_between,
};
use settings::{RegisterSetting, Settings};
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use ui::prelude::*;
use util::ResultExt;

use crate::diagram::{
    render_diagram_code_view, render_diagram_copy_button, render_diagram_tab_header,
};
use crate::parser::{CodeBlockKind, MarkdownEvent, MarkdownTag};

use super::{CopyButtonVisibility, Markdown, MarkdownStyle, ParsedMarkdown};

/// D2's built-in Neutral Default light theme.
const LIGHT_THEME: u32 = 0;
/// D2's built-in Dark Mauve theme.
const DARK_THEME: u32 = 200;

type D2DiagramCache = HashMap<SharedString, Arc<CachedD2Diagram>>;

#[derive(Clone, Debug)]
pub(crate) struct ParsedMarkdownD2Diagram {
    pub(crate) content_range: Range<usize>,
    pub(crate) contents: SharedString,
}

#[derive(Default, Clone)]
pub(crate) struct D2State {
    cache: D2DiagramCache,
    order: Vec<SharedString>,
    /// The settings the cached diagrams were rendered with.
    settings: D2Settings,
    /// The `d2` binary the cached diagrams were rendered with, resolved from
    /// the settings. `None` means D2 isn't installed, so diagrams are left as
    /// ordinary code blocks.
    binary: Option<Arc<Path>>,
    /// Whether [`Self::binary`] has been resolved for the current settings.
    /// Resolving touches the filesystem, so it is only redone when the settings
    /// change.
    binary_resolved: bool,
}

struct CachedD2Diagram {
    render_image: Arc<OnceLock<Result<Arc<RenderImage>>>>,
    /// The previous raster shown while `render_image` is pending, so an edited
    /// diagram doesn't disappear while it re-renders.
    fallback_image: Option<Arc<RenderImage>>,
    _task: Task<()>,
}

impl D2State {
    pub(crate) fn clear(&mut self, cx: &mut App) {
        let mut dropped_images = HashSet::default();
        for cached in self.cache.values() {
            cached.drop_images(cx, &HashSet::default(), &mut dropped_images);
        }
        self.cache.clear();
        self.order.clear();
    }

    fn get_fallback_image(
        index: usize,
        old_order: &[SharedString],
        new_order_len: usize,
        cache: &D2DiagramCache,
    ) -> Option<Arc<RenderImage>> {
        if old_order.len() != new_order_len {
            return None;
        }

        old_order.get(index).and_then(|old_contents| {
            cache.get(old_contents).and_then(|old_cached| {
                old_cached
                    .render_image
                    .get()
                    .and_then(|result| result.as_ref().ok().cloned())
                    .or_else(|| old_cached.fallback_image.clone())
            })
        })
    }

    /// Whether a `d2` binary was found, and so whether diagrams should be
    /// rendered at all.
    pub(crate) fn is_available(&self) -> bool {
        self.binary.is_some()
    }

    pub(crate) fn update(&mut self, parsed: &ParsedMarkdown, cx: &mut Context<Markdown>) {
        let settings = D2Settings::get_global(cx).clone();
        if !self.binary_resolved || settings != self.settings {
            self.binary = d2_render::locate(settings.path.as_deref()).map(Arc::from);
            self.binary_resolved = true;
        }
        let Some(binary) = self.binary.clone() else {
            self.clear(cx);
            self.settings = settings;
            return;
        };

        let new_order = parsed
            .d2_diagrams
            .values()
            .map(|diagram| diagram.contents.clone())
            .collect::<Vec<_>>();

        for (index, contents) in new_order.iter().enumerate() {
            if !self.cache.contains_key(contents) {
                let fallback_image =
                    Self::get_fallback_image(index, &self.order, new_order.len(), &self.cache);
                self.cache.insert(
                    contents.clone(),
                    Arc::new(CachedD2Diagram::new(
                        contents.clone(),
                        fallback_image,
                        binary.clone(),
                        settings.arguments.clone(),
                        cx,
                    )),
                );
            }
        }

        let new_order_set = new_order.iter().cloned().collect::<HashSet<_>>();
        // A fallback image is shared with the entry it was taken from, so images
        // still reachable from a surviving entry must outlive the dropped ones.
        let protected_images = self
            .cache
            .iter()
            .filter(|(contents, _)| new_order_set.contains(*contents))
            .flat_map(|(_, cached)| cached.images())
            .collect::<HashSet<_>>();
        let mut dropped_images = HashSet::default();
        self.cache.retain(|contents, cached| {
            let keep = new_order_set.contains(contents);
            if !keep {
                cached.drop_images(cx, &protected_images, &mut dropped_images);
            }
            keep
        });
        self.order = new_order;
        self.settings = settings;
    }

    /// Whether the cached diagrams were rendered with settings that no longer
    /// apply, so that unrelated settings changes don't re-render every diagram.
    pub(crate) fn settings_are_stale(&self, cx: &App) -> bool {
        !self.cache.is_empty() && self.settings != *D2Settings::get_global(cx)
    }
}

impl CachedD2Diagram {
    fn new(
        contents: SharedString,
        fallback_image: Option<Arc<RenderImage>>,
        binary: Arc<Path>,
        arguments: Vec<String>,
        cx: &mut Context<Markdown>,
    ) -> Self {
        let render_image = Arc::new(OnceLock::new());
        let svg_renderer = cx.svg_renderer();
        let theme = if cx.theme().appearance.is_light() {
            LIGHT_THEME
        } else {
            DARK_THEME
        };

        let task = cx.spawn({
            let render_image = render_image.clone();
            let fallback_image = fallback_image.clone();
            async move |this, cx| {
                let result = cx
                    .background_spawn(async move {
                        let svg = d2_render::render(&binary, &arguments, &contents, theme).await?;
                        svg_renderer
                            .render_single_frame(svg.as_bytes(), 1.0)
                            .map_err(|error| anyhow::anyhow!("{error}"))
                    })
                    .await;
                if let Err(error) = &result {
                    log::warn!("failed to render D2 diagram: {error:#}");
                }
                if render_image.set(result).is_err() {
                    log::error!("attempted to store a D2 render result more than once");
                }
                this.update(cx, |_, cx| {
                    if let Some(fallback_image) = fallback_image {
                        cx.drop_image(fallback_image, None);
                    }
                    cx.notify();
                })
                .log_err();
            }
        });

        Self {
            render_image,
            fallback_image,
            _task: task,
        }
    }

    fn images(&self) -> impl Iterator<Item = *const RenderImage> {
        self.render_image
            .get()
            .and_then(|result| result.as_ref().ok())
            .into_iter()
            .chain(self.fallback_image.as_ref())
            .map(Arc::as_ptr)
    }

    fn drop_images(
        &self,
        cx: &mut App,
        protected_images: &HashSet<*const RenderImage>,
        dropped_images: &mut HashSet<*const RenderImage>,
    ) {
        let render_image = self
            .render_image
            .get()
            .and_then(|result| result.as_ref().ok());
        for image in render_image.into_iter().chain(self.fallback_image.as_ref()) {
            let image_pointer = Arc::as_ptr(image);
            if !protected_images.contains(&image_pointer) && dropped_images.insert(image_pointer) {
                cx.drop_image(image.clone(), None);
            }
        }
    }

    #[cfg(test)]
    fn new_for_test(
        result: Option<Result<Arc<RenderImage>>>,
        fallback_image: Option<Arc<RenderImage>>,
    ) -> Self {
        let render_image = Arc::new(OnceLock::new());
        if let Some(result) = result {
            assert!(
                render_image.set(result).is_ok(),
                "test render result should only be initialized once"
            );
        }
        Self {
            render_image,
            fallback_image,
            _task: Task::ready(()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, RegisterSetting)]
pub struct D2Settings {
    /// The `d2` binary to render with. Defaults to `d2` from `PATH`.
    pub path: Option<PathBuf>,
    pub arguments: Vec<String>,
}

impl Settings for D2Settings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content
            .markdown_preview
            .as_ref()
            .and_then(|preview| preview.d2.clone())
            .unwrap_or_default();
        Self {
            path: content.path.map(PathBuf::from),
            arguments: content.arguments.unwrap_or_default(),
        }
    }
}

pub(crate) fn extract_d2_diagrams(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
) -> BTreeMap<usize, ParsedMarkdownD2Diagram> {
    let mut diagrams = BTreeMap::default();

    for (source_range, event) in events {
        let MarkdownEvent::Start(MarkdownTag::CodeBlock { kind, metadata }) = event else {
            continue;
        };
        if !metadata.is_fenced_closed {
            continue;
        }
        let CodeBlockKind::FencedLang(info) = kind else {
            continue;
        };
        if !info
            .split_whitespace()
            .next()
            .is_some_and(|language| language.eq_ignore_ascii_case("d2"))
        {
            continue;
        }

        let source_contents = &source[metadata.content_range.clone()];
        let contents = source_contents
            .strip_suffix('\n')
            .unwrap_or(source_contents)
            .to_string();
        diagrams.insert(
            source_range.start,
            ParsedMarkdownD2Diagram {
                content_range: metadata.content_range.clone(),
                contents: contents.into(),
            },
        );
    }

    diagrams
}

/// The image to paint for a diagram, plus whether the Preview/Code tabs apply.
/// Tabs are hidden when rendering failed, because only the source is available.
fn d2_presentation(
    showing_code: bool,
    cached: Option<&CachedD2Diagram>,
) -> (Option<Arc<RenderImage>>, bool) {
    let render_result = cached.and_then(|cached| cached.render_image.get());
    if matches!(render_result, Some(Err(_))) {
        return (None, false);
    }
    if showing_code {
        return (None, true);
    }
    let image = match render_result {
        Some(Ok(render_image)) => Some(render_image.clone()),
        None => cached.and_then(|cached| cached.fallback_image.clone()),
        Some(Err(_)) => None,
    };
    (image, true)
}

pub(crate) fn render_d2_diagram(
    parsed: &ParsedMarkdownD2Diagram,
    d2_state: &D2State,
    style: &MarkdownStyle,
    markdown: Entity<Markdown>,
    source_offset: usize,
    showing_code: bool,
    copy_button_visibility: CopyButtonVisibility,
) -> AnyElement {
    let cached = d2_state.cache.get(&parsed.contents).map(Arc::as_ref);
    let (image, show_tabs) = d2_presentation(showing_code, cached);
    let loading = image.is_none() && !showing_code && show_tabs;
    let show_interactive = copy_button_visibility != CopyButtonVisibility::Hidden;
    let code = parsed.contents.clone();

    let mut container = div().group("code_block").relative().w_full().rounded_lg();
    container.style().refine(&style.code_block);

    let body = match image {
        Some(render_image) => div()
            .id(ElementId::named_usize("d2-diagram-body", source_offset))
            .w_full()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .child(
                img(ImageSource::Render(render_image))
                    .with_fallback(|| Label::new("Failed to load D2 diagram").into_any_element()),
            )
            .into_any_element(),
        None => div()
            .w_full()
            .child(render_diagram_code_view(&code))
            .when(loading, |body| {
                body.child(
                    div().absolute().top_1().right_10().child(
                        Label::new("Rendering...")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .with_animation(
                                "d2-loading-pulse",
                                Animation::new(Duration::from_secs(2))
                                    .repeat()
                                    .with_easing(pulsating_between(0.4, 0.8)),
                                |label, delta| label.alpha(delta),
                            ),
                    ),
                )
            })
            .into_any_element(),
    };

    container
        .when(show_interactive && show_tabs, |container| {
            container.child(render_diagram_tab_header(
                source_offset,
                showing_code,
                markdown.clone(),
                |markdown, source_offset, showing_code| {
                    markdown.set_d2_showing_code(source_offset, showing_code)
                },
            ))
        })
        .child(body)
        .when(show_interactive, |container| {
            container.child(
                div()
                    .absolute()
                    .top_1()
                    .right_1()
                    .child(render_diagram_copy_button(
                        "copy-d2-code",
                        source_offset,
                        code.to_string(),
                        markdown,
                        copy_button_visibility,
                    )),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{CachedD2Diagram, D2DiagramCache, D2State, d2_presentation, extract_d2_diagrams};
    use crate::{
        CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownOptions,
        MarkdownStyle, WrapButtonVisibility,
    };
    use collections::HashMap;
    use gpui::{Context, Entity, IntoElement, Render, RenderImage, TestAppContext, Window, size};
    use std::{cell::RefCell, rc::Rc, sync::Arc};
    use ui::prelude::*;

    fn extract(
        markdown: &str,
    ) -> std::collections::BTreeMap<usize, super::ParsedMarkdownD2Diagram> {
        let events =
            crate::parser::parse_markdown_with_options(markdown, false, false, false).events;
        extract_d2_diagrams(markdown, &events)
    }

    fn contents(source: &str) -> SharedString {
        source.to_string().into()
    }

    fn sequence(diagrams: &[&str]) -> Vec<SharedString> {
        diagrams.iter().map(|diagram| contents(diagram)).collect()
    }

    fn mock_render_image(cx: &mut TestAppContext) -> Arc<RenderImage> {
        cx.update(|cx| {
            cx.svg_renderer()
                .render_single_frame(
                    br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>"#,
                    1.0,
                )
                .expect("test SVG should render")
        })
    }

    fn ensure_theme_initialized(cx: &mut TestAppContext) {
        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
    }

    fn draw_markdown_element(
        markdown: Entity<Markdown>,
        cx: &mut gpui::VisualTestContext,
    ) -> crate::RenderedText {
        struct CaptureRenderedText {
            markdown: Entity<Markdown>,
            rendered_text: Rc<RefCell<Option<crate::RenderedText>>>,
        }

        impl Render for CaptureRenderedText {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let element = MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    })
                    .on_render({
                        let rendered_text = self.rendered_text.clone();
                        move |text| *rendered_text.borrow_mut() = Some(text)
                    });
                div().child(element)
            }
        }

        let rendered_text = Rc::new(RefCell::new(None));
        cx.draw(Default::default(), size(px(600.0), px(600.0)), {
            let rendered_text = rendered_text.clone();
            |_window, cx| {
                cx.new(|_| CaptureRenderedText {
                    markdown,
                    rendered_text,
                })
                .into_any_element()
            }
        });
        rendered_text
            .borrow_mut()
            .take()
            .expect("markdown element should have been laid out")
    }

    struct TestWindow;

    impl Render for TestWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn render_markdown_with_cached_d2(
        source: &str,
        render_result: Option<anyhow::Result<Arc<RenderImage>>>,
        cx: &mut TestAppContext,
    ) -> crate::RenderedText {
        ensure_theme_initialized(cx);
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                source.to_string().into(),
                None,
                None,
                MarkdownOptions::default(),
                cx,
            )
        });
        cx.run_until_parked();
        markdown.update(cx, |markdown, _| {
            markdown.options.render_d2_diagrams = true;
            let source = markdown.parsed_markdown.source.clone();
            let events = markdown.parsed_markdown.events.clone();
            markdown.parsed_markdown.d2_diagrams = extract_d2_diagrams(&source, &events);
            let contents = markdown
                .parsed_markdown
                .d2_diagrams
                .values()
                .next()
                .expect("D2 diagram should be extracted")
                .contents
                .clone();
            markdown.d2_state.cache.insert(
                contents.clone(),
                Arc::new(CachedD2Diagram::new_for_test(render_result, None)),
            );
            markdown.d2_state.order = vec![contents];
            markdown.d2_state.binary = Some(Arc::from(std::path::Path::new("d2")));
        });
        draw_markdown_element(markdown, cx)
    }

    fn render_markdown_without_d2(source: &str, cx: &mut TestAppContext) -> crate::RenderedText {
        ensure_theme_initialized(cx);
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| Markdown::new(source.to_string().into(), None, None, cx));
        cx.run_until_parked();
        draw_markdown_element(markdown, cx)
    }

    fn rendered_text(rendered: &crate::RenderedText) -> String {
        rendered
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn extracts_closed_d2_fenced_block() {
        let markdown = "```d2\nx -> y\n```";
        let diagrams = extract(markdown);

        let diagram = diagrams
            .values()
            .next()
            .expect("D2 block should be extracted");
        assert_eq!(diagram.contents, "x -> y");
        let content_start = markdown.find("x -> y").expect("content should be present");
        assert_eq!(
            diagram.content_range,
            content_start..content_start + "x -> y\n".len()
        );
        assert_eq!(&markdown[diagram.content_range.clone()], "x -> y\n");
    }

    #[test]
    fn ignores_non_d2_fenced_blocks() {
        let diagrams = extract("```rust\nlet d2 = true;\n```");

        assert!(diagrams.is_empty());
    }

    #[test]
    fn ignores_unclosed_d2_fenced_blocks() {
        let diagrams = extract("```d2\nx -> y\n");

        assert!(diagrams.is_empty());
    }

    #[test]
    fn ignores_fenced_source_paths_with_d2_extension() {
        let diagrams = extract("```diagrams/example.d2\nx -> y\n```");

        assert!(diagrams.is_empty());
    }

    #[test]
    fn preserves_unicode_content_and_byte_range() {
        let markdown = "before\n\n```d2\n東京 -> café: 🦀\n```";
        let diagrams = extract(markdown);

        let diagram = diagrams
            .values()
            .next()
            .expect("D2 block should be extracted");
        assert_eq!(diagram.contents, "東京 -> café: 🦀");
        assert_eq!(
            &markdown[diagram.content_range.clone()],
            "東京 -> café: 🦀\n"
        );
    }

    #[test]
    fn extracts_multiple_blocks_at_their_source_offsets() {
        let markdown = "intro\n\n```d2\na -> b\n```\n\nmiddle\n\n```D2 layout=elk\nc -> d\n```";
        let diagrams = extract(markdown);
        let expected_offsets = vec![
            markdown
                .find("```d2")
                .expect("first fence should be present"),
            markdown
                .rfind("```D2")
                .expect("second fence should be present"),
        ];

        assert_eq!(
            diagrams.keys().copied().collect::<Vec<_>>(),
            expected_offsets
        );
        assert_eq!(
            diagrams
                .values()
                .map(|diagram| diagram.contents.as_ref())
                .collect::<Vec<_>>(),
            vec!["a -> b", "c -> d"]
        );
    }

    #[gpui::test]
    fn uses_previous_image_at_the_same_position_after_an_edit(cx: &mut TestAppContext) {
        let old_order = sequence(&["a -> b", "b -> c"]);
        let new_order = sequence(&["a -> b", "b -> changed"]);
        let previous_image = mock_render_image(cx);
        let mut cache: D2DiagramCache = HashMap::default();
        cache.insert(
            contents("b -> c"),
            Arc::new(CachedD2Diagram::new_for_test(
                Some(Ok(previous_image.clone())),
                None,
            )),
        );

        let fallback = D2State::get_fallback_image(1, &old_order, new_order.len(), &cache);

        assert_eq!(fallback.map(|image| image.id), Some(previous_image.id));
    }

    #[gpui::test]
    fn carries_previous_fallback_across_rapid_edits(cx: &mut TestAppContext) {
        let old_order = sequence(&["a -> first edit"]);
        let new_order = sequence(&["a -> second edit"]);
        let original_image = mock_render_image(cx);
        let mut cache: D2DiagramCache = HashMap::default();
        cache.insert(
            contents("a -> first edit"),
            Arc::new(CachedD2Diagram::new_for_test(
                None,
                Some(original_image.clone()),
            )),
        );

        let fallback = D2State::get_fallback_image(0, &old_order, new_order.len(), &cache);

        assert_eq!(fallback.map(|image| image.id), Some(original_image.id));
    }

    #[gpui::test]
    fn cached_image_replaces_source_in_preview(cx: &mut TestAppContext) {
        let image = mock_render_image(cx);
        let cached = CachedD2Diagram::new_for_test(Some(Ok(image.clone())), None);

        let (rendered_image, show_tabs) = d2_presentation(false, Some(&cached));

        assert_eq!(rendered_image.map(|image| image.id), Some(image.id));
        assert!(show_tabs);
    }

    #[test]
    fn failed_render_shows_source_without_tabs() {
        let cached = CachedD2Diagram::new_for_test(
            Some(Err(anyhow::anyhow!("injected D2 render failure"))),
            None,
        );

        let (rendered_image, show_tabs) = d2_presentation(false, Some(&cached));

        assert!(rendered_image.is_none());
        assert!(!show_tabs);
    }

    #[gpui::test]
    fn disabled_d2_block_renders_as_ordinary_code(cx: &mut TestAppContext) {
        let rendered = render_markdown_without_d2("```d2\nclient -> server\n```", cx);

        assert!(rendered_text(&rendered).contains("client -> server"));
    }

    #[gpui::test]
    fn cached_d2_image_replaces_code_block_text(cx: &mut TestAppContext) {
        let image = mock_render_image(cx);
        let rendered =
            render_markdown_with_cached_d2("```d2\nclient -> server\n```", Some(Ok(image)), cx);

        assert!(!rendered_text(&rendered).contains("client -> server"));
    }

    #[test]
    fn reads_the_configured_d2_binary() {
        let mut content = settings::SettingsContent::default();
        content.markdown_preview = Some(settings::MarkdownPreviewSettingsContent {
            d2: Some(settings::D2SettingsContent {
                path: Some("/usr/local/bin/d2".into()),
                arguments: Some(vec!["--layout=elk".into()]),
            }),
            ..Default::default()
        });

        let settings = <super::D2Settings as settings::Settings>::from_settings(&content);

        assert_eq!(
            settings.path.as_deref(),
            Some(std::path::Path::new("/usr/local/bin/d2"))
        );
        assert_eq!(settings.arguments, ["--layout=elk"]);
    }
}

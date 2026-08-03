//! Text selection for rendered markdown (round 18).
//!
//! gpui has no built-in selection for plain text elements. Zed's markdown
//! selects continuously because its whole document is ONE element over one
//! text model; comet renders a TREE of text elements inside a virtualized
//! list, so this module rebuilds that continuity: every frame the renderer
//! registers each painted text element in paint order (= document order),
//! and a drag anchored in one element resolves against that registry into
//! per-element SPANS — partial in the anchor/head elements, whole for every
//! element between. The wash paints per element from its span; copy joins
//! the spans in order.
//!
//! This module is the pure state half (gpui-free, unit-tested); the
//! registry, geometry and mouse listeners live in `render.rs`.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

/// Original source for one top-level Markdown block (or one user bubble).
/// Every selectable text element in the block shares this value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockSource {
    pub block_key: String,
    pub part_key: String,
    pub source: Arc<str>,
    pub range: Range<usize>,
}

/// Styling for one byte range in a flattened inline text element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineCopyRun {
    pub range: Range<usize>,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strikethrough: bool,
    pub link: Option<String>,
}

/// One display-only projection inside a user bubble. Selecting any portion of
/// it copies the complete source range (file-mention chips are atomic).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projection {
    pub display: Range<usize>,
    pub source: Range<usize>,
}

/// How a partial selection inside one rendered text element becomes Markdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartialCopy {
    /// Assistant prose: preserve inline semantics while slicing rendered text.
    Inline(Vec<InlineCopyRun>),
    /// User prose with display-only mention-chip projections.
    Projected(Vec<Projection>),
    /// Code lines and other literal text.
    Plain,
}

/// Clipboard metadata registered alongside one rendered text element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyMeta {
    pub block: BlockSource,
    pub partial: PartialCopy,
}

/// One rendered selectable element in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub key: String,
    pub text: String,
    pub copy: Option<CopyMeta>,
}

impl Element {
    pub fn plain(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
            copy: None,
        }
    }
}

/// One element's slice of the selection, in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Element key (`{row_key}:{element ix}`).
    pub key: String,
    /// Selected byte range of the element's flat text.
    pub range: Range<usize>,
    /// The element's full flat text (copy source, snapshotted at drag time
    /// so copy still works after the element scrolls out of the registry).
    pub text: String,
    /// Source-aware clipboard projection for this element.
    pub copy: Option<CopyMeta>,
    /// Whether this is the first/last non-empty selectable element in its
    /// source block. A block is exact-copyable when both edges are selected
    /// in full.
    pub first_in_block: bool,
    pub last_in_block: bool,
}

#[derive(Clone, Default)]
struct MdSelection {
    /// Element that owns the drag (where the mouse went down).
    anchor_key: String,
    /// Byte offset of the anchor within its element.
    anchor_ix: usize,
    dragging: bool,
    /// Resolved spans, document order. Empty while a click hasn't moved.
    spans: Vec<Span>,
}

fn state() -> &'static Mutex<Option<MdSelection>> {
    static STATE: OnceLock<Mutex<Option<MdSelection>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// Resolve the spans for a selection between `a` and `b`, each an
/// `(element index, byte offset)` into `elements` (document-ordered
/// `(key, text)` pairs). Handles either direction; empty slices are skipped.
pub fn resolve_spans(elements: &[Element], a: (usize, usize), b: (usize, usize)) -> Vec<Span> {
    let (start, end) = if (a.0, a.1) <= (b.0, b.1) {
        (a, b)
    } else {
        (b, a)
    };
    let mut block_bounds: HashMap<&str, (usize, usize)> = HashMap::new();
    for (ix, element) in elements.iter().enumerate() {
        if element.text.is_empty() {
            continue;
        }
        if let Some(copy) = &element.copy {
            block_bounds
                .entry(&copy.block.block_key)
                .and_modify(|bounds| bounds.1 = ix)
                .or_insert((ix, ix));
        }
    }
    let mut spans = Vec::new();
    for (ei, element) in elements.iter().enumerate().take(end.0 + 1).skip(start.0) {
        let key = element.key.as_str();
        let text = element.text.as_str();
        let from = if ei == start.0 { start.1 } else { 0 };
        let to = if ei == end.0 { end.1 } else { text.len() };
        let (from, to) = (from.min(text.len()), to.min(text.len()));
        if from < to {
            let (first_in_block, last_in_block) = element
                .copy
                .as_ref()
                .and_then(|copy| block_bounds.get(copy.block.block_key.as_str()))
                .map_or((false, false), |&(first, last)| (first == ei, last == ei));
            spans.push(Span {
                key: key.to_string(),
                range: from..to,
                text: text.to_string(),
                copy: element.copy.clone(),
                first_in_block,
                last_in_block,
            });
        }
    }
    spans
}

/// Begin a drag anchored at `(key, ix)`; claims the global selection.
pub fn begin(key: &str, ix: usize) {
    *state().lock().unwrap() = Some(MdSelection {
        anchor_key: key.to_string(),
        anchor_ix: ix,
        dragging: true,
        spans: Vec::new(),
    });
}

/// Begin with an immediate span (double/triple click inside one element).
pub fn begin_with_span(key: &str, text: &str, range: Range<usize>) {
    *state().lock().unwrap() = Some(MdSelection {
        anchor_key: key.to_string(),
        anchor_ix: range.start,
        dragging: true,
        spans: vec![Span {
            key: key.to_string(),
            range,
            text: text.to_string(),
            copy: None,
            first_in_block: false,
            last_in_block: false,
        }],
    });
}

/// Begin a settled click-selection with source-aware spans resolved by the
/// renderer (double/triple click). This keeps Markdown clipboard metadata
/// intact just like a drag selection.
pub fn begin_with_spans(key: &str, anchor_ix: usize, spans: Vec<Span>) {
    *state().lock().unwrap() = Some(MdSelection {
        anchor_key: key.to_string(),
        anchor_ix,
        dragging: true,
        spans,
    });
}

/// The live drag's anchor, if `key` owns it: `(anchor byte offset)`.
pub fn drag_anchor(key: &str) -> Option<usize> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    (sel.dragging && sel.anchor_key == key).then_some(sel.anchor_ix)
}

/// Replace the resolved spans (drag update). Returns true if they changed.
pub fn update_spans(spans: Vec<Span>) -> bool {
    let mut guard = state().lock().unwrap();
    let Some(sel) = guard.as_mut() else {
        return false;
    };
    if sel.spans == spans {
        return false;
    }
    sel.spans = spans;
    true
}

/// End the drag for `key`'s claim; returns the joined text if non-empty.
pub fn end_drag(key: &str) -> Option<String> {
    let mut guard = state().lock().unwrap();
    let sel = guard.as_mut()?;
    if sel.anchor_key != key || !sel.dragging {
        return None;
    }
    sel.dragging = false;
    if sel.spans.iter().all(|s| s.range.is_empty()) {
        *guard = None;
        return None;
    }
    Some(join_spans(&sel.spans))
}

/// Clear if `key` owns a settled selection (a mouse-down landed outside the
/// owner; the element the down landed IN claims right after). True if cleared.
pub fn clear_if_owner(key: &str) -> bool {
    let mut guard = state().lock().unwrap();
    if guard
        .as_ref()
        .is_some_and(|s| s.anchor_key == key && !s.dragging)
    {
        *guard = None;
        return true;
    }
    false
}

/// The wash range for `key` this frame (empty ⇒ nothing to paint).
pub fn wash_range(key: &str) -> Option<Range<usize>> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    sel.spans
        .iter()
        .find(|s| s.key == key && !s.range.is_empty())
        .map(|s| s.range.clone())
}

/// The full selected text (Cmd+C), spans joined in document order.
pub fn selected_text() -> Option<String> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    if sel.spans.iter().all(|s| s.range.is_empty()) {
        return None;
    }
    Some(join_spans(&sel.spans))
}

fn join_spans(spans: &[Span]) -> String {
    let spans: Vec<&Span> = spans.iter().filter(|span| !span.range.is_empty()).collect();
    let mut blocks: Vec<BlockOutput> = Vec::new();
    let mut at = 0usize;
    while at < spans.len() {
        let key = spans[at]
            .copy
            .as_ref()
            .map(|copy| copy.block.block_key.as_str());
        let mut end = at + 1;
        while end < spans.len()
            && spans[end]
                .copy
                .as_ref()
                .map(|copy| copy.block.block_key.as_str())
                == key
        {
            end += 1;
        }
        blocks.push(copy_block(&spans[at..end]));
        at = end;
    }

    let mut out = String::new();
    let mut ix = 0usize;
    while ix < blocks.len() {
        // Consecutive complete blocks from the same part are one exact source
        // slice, preserving the author's blank lines and container syntax.
        if let Some((part_key, source, mut range)) = blocks[ix].exact.clone() {
            let mut end = ix + 1;
            while let Some((next_part, next_source, next_range)) =
                blocks.get(end).and_then(|block| block.exact.clone())
            {
                if next_part != part_key || next_source != source || next_range.start < range.end {
                    break;
                }
                range.end = next_range.end;
                end += 1;
            }
            append_block(&mut out, source.get(range).unwrap_or_default());
            ix = end;
        } else {
            append_block(&mut out, &blocks[ix].text);
            ix += 1;
        }
    }
    out
}

#[derive(Clone)]
struct BlockOutput {
    text: String,
    /// `(part key, full source, selected exact range)`.
    exact: Option<(String, Arc<str>, Range<usize>)>,
}

fn copy_block(spans: &[&Span]) -> BlockOutput {
    let full = spans
        .first()
        .is_some_and(|span| span.first_in_block && span.range.start == 0)
        && spans
            .last()
            .is_some_and(|span| span.last_in_block && span.range.end == span.text.len());

    if full
        && let Some(block) = spans
            .first()
            .and_then(|span| span.copy.as_ref())
            .map(|copy| copy.block.clone())
    {
        let text = block
            .source
            .get(block.range.clone())
            .unwrap_or_default()
            .to_string();
        return BlockOutput {
            text,
            exact: Some((block.part_key, block.source, block.range)),
        };
    }

    BlockOutput {
        text: spans
            .iter()
            .map(|span| partial_markdown(span))
            .collect::<Vec<_>>()
            .join("\n"),
        exact: None,
    }
}

fn append_block(out: &mut String, block: &str) {
    let block = block.trim_matches('\n');
    if block.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
}

fn partial_markdown(span: &Span) -> String {
    let Some(copy) = &span.copy else {
        return span.text[span.range.clone()].to_string();
    };
    match &copy.partial {
        PartialCopy::Inline(runs) => inline_markdown(&span.text, &span.range, runs),
        PartialCopy::Projected(projections) => {
            projected_source(&span.text, &span.range, &copy.block.source, projections)
        }
        PartialCopy::Plain => span.text[span.range.clone()].to_string(),
    }
}

fn inline_markdown(text: &str, selected: &Range<usize>, runs: &[InlineCopyRun]) -> String {
    #[derive(Clone, PartialEq, Eq)]
    struct Style {
        bold: bool,
        italic: bool,
        code: bool,
        strikethrough: bool,
        link: Option<String>,
    }

    let mut chunks: Vec<(String, Style)> = Vec::new();
    for run in runs {
        let start = selected.start.max(run.range.start);
        let end = selected.end.min(run.range.end);
        if start >= end {
            continue;
        }
        let style = Style {
            bold: run.bold,
            italic: run.italic,
            code: run.code,
            strikethrough: run.strikethrough,
            link: run.link.clone(),
        };
        let piece = text.get(start..end).unwrap_or_default();
        match chunks.last_mut() {
            Some((last, last_style)) if *last_style == style => last.push_str(piece),
            _ => chunks.push((piece.to_string(), style)),
        }
    }

    if chunks.is_empty() {
        return text.get(selected.clone()).unwrap_or_default().to_string();
    }

    chunks
        .into_iter()
        .map(|(text, style)| {
            let mut value = if style.code {
                code_span(&text)
            } else {
                escape_inline(&text)
            };
            if style.strikethrough {
                value = format!("~~{value}~~");
            }
            if style.italic {
                value = format!("_{value}_");
            }
            if style.bold {
                value = format!("**{value}**");
            }
            if let Some(url) = style.link {
                value = format!("[{value}]({})", escape_link_destination(&url));
            }
            value
        })
        .collect()
}

fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '~') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn escape_link_destination(url: &str) -> String {
    url.replace('\\', "\\\\").replace(')', "\\)")
}

fn code_span(text: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    let pad = text.starts_with('`')
        || text.starts_with(' ')
        || text.ends_with('`')
        || text.ends_with(' ');
    if pad {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

fn projected_source(
    display: &str,
    selected: &Range<usize>,
    source: &str,
    projections: &[Projection],
) -> String {
    let mut out = String::new();
    let mut display_at = 0usize;
    let mut source_at = 0usize;
    for projection in projections {
        let plain_display = display_at..projection.display.start;
        let start = selected.start.max(plain_display.start);
        let end = selected.end.min(plain_display.end);
        if start < end {
            let source_start = source_at + (start - plain_display.start);
            let source_end = source_at + (end - plain_display.start);
            out.push_str(source.get(source_start..source_end).unwrap_or_default());
        }
        if selected.start < projection.display.end && selected.end > projection.display.start {
            out.push_str(source.get(projection.source.clone()).unwrap_or_default());
        }
        display_at = projection.display.end;
        source_at = projection.source.end;
    }
    let plain_display = display_at..display.len();
    let start = selected.start.max(plain_display.start);
    let end = selected.end.min(plain_display.end);
    if start < end {
        let source_start = source_at + (start - plain_display.start);
        let source_end = source_at + (end - plain_display.start);
        out.push_str(source.get(source_start..source_end).unwrap_or_default());
    }
    out
}

/// Word range around `ix` for double-click selection: an alphanumeric/`_`
/// run, or the single non-space char under the cursor, or empty at spaces.
pub fn word_range(text: &str, ix: usize) -> Range<usize> {
    let mut ix = ix.min(text.len());
    // Snap into a char boundary (mouse indices should already be on one;
    // defensive against mid-char byte offsets).
    while ix > 0 && !text.is_char_boundary(ix) {
        ix -= 1;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let before = text[..ix].chars().next_back();
    let at = text[ix..].chars().next();
    // Off a word boundary entirely: select the single char (or nothing).
    if !at.is_some_and(is_word) && !before.is_some_and(is_word) {
        return match at {
            Some(c) if !c.is_whitespace() => ix..ix + c.len_utf8(),
            _ => ix..ix,
        };
    }
    let start = text[..ix]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(ix);
    let end = text[ix..]
        .char_indices()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map(|(i, c)| ix + i + c.len_utf8())
        .unwrap_or(ix);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elems() -> Vec<Element> {
        vec![
            Element::plain("p1", "first paragraph"),
            Element::plain("p2", "second"),
            Element::plain("p3", "third one"),
        ]
    }

    fn source_element(
        key: &str,
        text: &str,
        block_key: &str,
        part_key: &str,
        source: Arc<str>,
        range: Range<usize>,
        partial: PartialCopy,
    ) -> Element {
        Element {
            key: key.to_string(),
            text: text.to_string(),
            copy: Some(CopyMeta {
                block: BlockSource {
                    block_key: block_key.to_string(),
                    part_key: part_key.to_string(),
                    source,
                    range,
                },
                partial,
            }),
        }
    }

    #[test]
    fn spans_within_one_element() {
        let spans = resolve_spans(&elems(), (0, 6), (0, 15));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].key, "p1");
        assert_eq!(&spans[0].text[spans[0].range.clone()], "paragraph");
        // Reversed direction normalizes.
        assert_eq!(resolve_spans(&elems(), (0, 15), (0, 6)), spans);
    }

    #[test]
    fn spans_across_elements_cover_middles_whole() {
        let spans = resolve_spans(&elems(), (0, 6), (2, 5));
        assert_eq!(spans.len(), 3);
        assert_eq!(&spans[0].text[spans[0].range.clone()], "paragraph");
        assert_eq!(&spans[1].text[spans[1].range.clone()], "second");
        assert_eq!(&spans[2].text[spans[2].range.clone()], "third");
        // Reversed drag (bottom-up) resolves identically.
        assert_eq!(resolve_spans(&elems(), (2, 5), (0, 6)), spans);
    }

    /// The drag tests below mutate the process-global selection state —
    /// serialize them, or the parallel test runner interleaves their
    /// begin/end_drag calls (long-standing flake).
    fn state_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn drag_lifecycle_and_copy_joins() {
        let _state = state_lock();
        begin("p1", 6);
        assert_eq!(drag_anchor("p1"), Some(6));
        assert_eq!(drag_anchor("p2"), None);
        let spans = resolve_spans(&elems(), (0, 6), (1, 6));
        assert!(update_spans(spans.clone()));
        assert!(!update_spans(spans)); // unchanged ⇒ no repaint
        assert_eq!(wash_range("p1"), Some(6..15));
        assert_eq!(wash_range("p2"), Some(0..6));
        assert_eq!(wash_range("p3"), None);
        assert_eq!(end_drag("p1").as_deref(), Some("paragraph\nsecond"));
        assert_eq!(selected_text().as_deref(), Some("paragraph\nsecond"));
        // Settled: a down elsewhere clears via the owner's listener.
        assert!(!clear_if_owner("p2"));
        assert!(clear_if_owner("p1"));
        assert_eq!(selected_text(), None);
    }

    #[test]
    fn empty_click_clears_on_release() {
        let _state = state_lock();
        begin("p1", 3);
        assert_eq!(end_drag("p1"), None);
        assert_eq!(selected_text(), None);
    }

    #[test]
    fn double_click_span() {
        let _state = state_lock();
        begin_with_span("p1", "hello world", 6..11);
        assert_eq!(wash_range("p1"), Some(6..11));
        assert_eq!(end_drag("p1").as_deref(), Some("world"));
    }

    #[test]
    fn word_ranges() {
        let t = "let foo_bar = 12;";
        assert_eq!(word_range(t, 5), 4..11); // inside foo_bar
        assert_eq!(word_range(t, 4), 4..11); // at word start
        assert_eq!(word_range(t, 11), 4..11); // at word end
        assert_eq!(word_range(t, 15), 14..16); // inside 12
        assert_eq!(&t[word_range(t, 12)], "="); // lone symbol
        assert_eq!(word_range(t, 3), 0..3); // boundary after "let"
        // Unicode-safe (mid-char byte offsets snap down).
        let u = "héllo wörld";
        assert_eq!(&u[word_range(u, 2)], "héllo");
    }

    #[test]
    fn complete_blocks_copy_exact_original_markdown_and_spacing() {
        let source: Arc<str> =
            Arc::from("Pushed. `local` is **synced**.\n\n[Next](https://x.test).\n");
        let first_end = source.find("\n\n").unwrap();
        let elements = vec![
            source_element(
                "b0:0",
                "Pushed. local is synced.",
                "b0",
                "part",
                source.clone(),
                0..first_end,
                PartialCopy::Inline(Vec::new()),
            ),
            source_element(
                "b1:0",
                "Next.",
                "b1",
                "part",
                source.clone(),
                first_end + 2..source.len() - 1,
                PartialCopy::Inline(Vec::new()),
            ),
        ];
        let spans = resolve_spans(&elements, (0, 0), (1, elements[1].text.len()));
        assert_eq!(
            join_spans(&spans),
            "Pushed. `local` is **synced**.\n\n[Next](https://x.test)."
        );
    }

    #[test]
    fn partial_inline_selection_reconstructs_markdown_semantics() {
        let source: Arc<str> = Arc::from("A **bold** and `code` [link](https://x.test).\n");
        let text = "A bold and code link.";
        let runs = vec![
            InlineCopyRun {
                range: 0..2,
                bold: false,
                italic: false,
                code: false,
                strikethrough: false,
                link: None,
            },
            InlineCopyRun {
                range: 2..6,
                bold: true,
                italic: false,
                code: false,
                strikethrough: false,
                link: None,
            },
            InlineCopyRun {
                range: 6..11,
                bold: false,
                italic: false,
                code: false,
                strikethrough: false,
                link: None,
            },
            InlineCopyRun {
                range: 11..15,
                bold: false,
                italic: false,
                code: true,
                strikethrough: false,
                link: None,
            },
            InlineCopyRun {
                range: 15..16,
                bold: false,
                italic: false,
                code: false,
                strikethrough: false,
                link: None,
            },
            InlineCopyRun {
                range: 16..20,
                bold: false,
                italic: false,
                code: false,
                strikethrough: false,
                link: Some("https://x.test".into()),
            },
            InlineCopyRun {
                range: 20..21,
                bold: false,
                italic: false,
                code: false,
                strikethrough: false,
                link: None,
            },
        ];
        let element = source_element(
            "b0:0",
            text,
            "b0",
            "part",
            source.clone(),
            0..46,
            PartialCopy::Inline(runs),
        );
        let spans = resolve_spans(&[element], (0, 2), (0, 20));
        assert_eq!(
            join_spans(&spans),
            "**bold** and `code` [link](https://x.test)"
        );
    }

    #[test]
    fn projected_user_mention_copies_atomic_original_link() {
        let source: Arc<str> = Arc::from("see [a.rs](comet-file:src/a.rs) now");
        let display = "see \u{00a0}@a.rs\u{00a0} now";
        let chip = 4..display.find(" now").unwrap();
        let source_chip = 4..source.find(" now").unwrap();
        let element = source_element(
            "user:0",
            display,
            "user",
            "user",
            source.clone(),
            0..source.len(),
            PartialCopy::Projected(vec![Projection {
                display: chip.clone(),
                source: source_chip,
            }]),
        );
        let spans = resolve_spans(&[element], (0, chip.start + 2), (0, chip.end - 1));
        assert_eq!(join_spans(&spans), "[a.rs](comet-file:src/a.rs)");
    }
}

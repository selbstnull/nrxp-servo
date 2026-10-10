/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! "Find in page" for the embedder (nrxp-servo N5b): matches are found in the text of the document and painted by
//! layout as background rectangles behind the text — the DOM is **not** modified (no wrapper elements, no mutation
//! records, no change of the user's selection). The active match is scrolled into view.
//!
//! Matching is done on the DOM text with whitespace runs collapsed to one space; text of `script`, `style`, `template`,
//! `head`, `title` and of nodes without a rendered text run (`display: none`, form controls) is skipped. A match may
//! span inline elements (`hel<b>lo</b>`) but never crosses a block boundary. Limits: the main document only (not
//! iframes), no shadow DOM, highlights are not restored when layout rebuilds a text run (the embedder re-sends
//! `Refresh`).

use std::cell::RefCell;

use embedder_traits::{FindAction, FindRequest};
use js::context::JSContext;
use layout_api::{FindHighlight, QueryMsg};
use rustc_hash::FxHashMap;
use servo_base::id::PipelineId;
use servo_base::text::{RangeAny, Utf32CodeUnits};

use crate::dom::bindings::codegen::Bindings::ElementBinding::ScrollLogicalPosition;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::ScrollBehavior;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::characterdata::CharacterData;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::text::Text;
use crate::dom::window::scrolling_box::ScrollAxisState;

/// Most matches kept per search (a one-letter query on a huge page must not freeze the page).
const MAX_MATCHES: usize = 5000;

/// What is remembered between requests, per document: no DOM references, so nothing is kept alive.
struct FindState {
    query: String,
    case_sensitive: bool,
    active: usize,
}

thread_local! {
    static STATES: RefCell<FxHashMap<PipelineId, FindState>> = RefCell::default();
}

/// One piece of a match inside a single text node, in `char` offsets of that node's data.
struct Segment {
    node: usize,
    start: u32,
    end: u32,
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "html" | "body" | "div" | "p" | "li" | "ul" | "ol" | "dl" | "dt" | "dd" | "table" | "tr" | "td" | "th" |
            "thead" | "tbody" | "tfoot" | "caption" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" |
            "article" | "header" | "footer" | "nav" | "aside" | "main" | "form" | "pre" | "blockquote" |
            "figure" | "figcaption" | "fieldset" | "details" | "summary" | "address" | "hr" | "button" |
            "select" | "option" | "label" | "legend"
    )
}

fn is_skipped(name: &str) -> bool {
    matches!(
        name,
        "script" | "style" | "template" | "head" | "title" | "noscript" | "textarea" | "iframe"
    )
}

/// The nearest block-level ancestor element of `node` (as an address, used only for equality), and whether the node
/// sits in a skipped subtree.
fn block_ancestor(node: &Node) -> (usize, bool) {
    let mut block = 0usize;
    let mut skipped = false;
    for ancestor in node.ancestors() {
        let Some(element) = ancestor.downcast::<Element>() else {
            continue;
        };
        let name = element.local_name();
        if is_skipped(name) {
            skipped = true;
        }
        if block == 0 && is_block(name) {
            block = &*ancestor as *const Node as usize;
        }
    }
    (block, skipped)
}

fn fold(c: char, case_sensitive: bool) -> char {
    if case_sensitive {
        c
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Collapses whitespace runs to a single space (and trims) — the form in which both the query and the page text are
/// compared.
fn normalize_query(query: &str, case_sensitive: bool) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    let mut pending_space = false;
    for c in query.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(fold(c, case_sensitive));
        }
    }
    out
}

/// Runs one "find in page" request in `document`; returns `(match count, active match index)`.
pub(crate) fn run(
    cx: &mut JSContext,
    document: &DomRoot<Document>,
    request: &FindRequest,
) -> (usize, Option<usize>) {
    let window = document.window();
    let pipeline_id = window.pipeline_id();
    // Layout must be current: only nodes with a text run can be highlighted.
    window.layout_reflow(QueryMsg::StyleQuery);

    // 1. All text nodes in tree order (every one gets its highlights replaced, so old ones vanish).
    let mut texts: Vec<(DomRoot<CharacterData>, Vec<char>, bool)> = Vec::new();
    for node in document
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No)
    {
        let Some(text) = node.downcast::<Text>() else {
            continue;
        };
        let (_, skipped) = block_ancestor(&node);
        let character_data = DomRoot::from_ref(text.upcast::<CharacterData>());
        let chars: Vec<char> = character_data.data().chars().collect();
        let rendered = !skipped && character_data.rendered_text(RangeAny::full()).is_some();
        texts.push((character_data, chars, rendered));
    }

    let clear_all = |texts: &[(DomRoot<CharacterData>, Vec<char>, bool)]| {
        for (character_data, _, _) in texts {
            character_data.set_text_run_find_highlights(Vec::new());
        }
        window.layout().set_needs_new_display_list();
    };

    if request.action == FindAction::Clear || request.query.trim().is_empty() {
        STATES.with(|states| states.borrow_mut().remove(&pipeline_id));
        clear_all(&texts);
        return (0, None);
    }

    // 2. Flatten the visible text: collapsed whitespace, a NUL between blocks. `origin[i]` — (text index, char offset).
    let query = normalize_query(&request.query, request.case_sensitive);
    let mut haystack: Vec<char> = Vec::new();
    let mut origin: Vec<(usize, u32)> = Vec::new();
    let mut previous_block: Option<usize> = None;
    for (index, (character_data, chars, rendered)) in texts.iter().enumerate() {
        if !rendered {
            continue;
        }
        let (block, _) = block_ancestor(character_data.upcast::<Node>());
        if previous_block.is_some_and(|b| b != block) {
            haystack.push('\0');
            origin.push((index, 0));
        }
        previous_block = Some(block);
        let mut last_was_space = haystack.last().is_some_and(|c| *c == ' ' || *c == '\0');
        for (offset, c) in chars.iter().enumerate() {
            if c.is_whitespace() {
                if last_was_space {
                    continue;
                }
                haystack.push(' ');
                last_was_space = true;
            } else {
                haystack.push(fold(*c, request.case_sensitive));
                last_was_space = false;
            }
            origin.push((index, offset as u32));
        }
    }

    // 3. Matches (non-overlapping), each as per-node segments.
    let mut matches: Vec<Vec<Segment>> = Vec::new();
    if !query.is_empty() && haystack.len() >= query.len() {
        let mut i = 0;
        while i + query.len() <= haystack.len() && matches.len() < MAX_MATCHES {
            if haystack[i..i + query.len()] == query[..] {
                let mut segments: Vec<Segment> = Vec::new();
                for (node, offset) in &origin[i..i + query.len()] {
                    match segments.last_mut() {
                        Some(last) if last.node == *node && last.end == *offset => last.end += 1,
                        Some(last) if last.node == *node && last.end < *offset => last.end = *offset + 1,
                        _ => segments.push(Segment {
                            node: *node,
                            start: *offset,
                            end: *offset + 1,
                        }),
                    }
                }
                matches.push(segments);
                i += query.len();
            } else {
                i += 1;
            }
        }
    }

    if matches.is_empty() {
        STATES.with(|states| states.borrow_mut().remove(&pipeline_id));
        clear_all(&texts);
        return (0, None);
    }

    // 4. Which match is active.
    let previous = STATES.with(|states| {
        states.borrow().get(&pipeline_id).map(|s| {
            (
                s.query == request.query && s.case_sensitive == request.case_sensitive,
                s.active,
            )
        })
    });
    let count = matches.len();
    let (active, scroll) = match (request.action, previous) {
        (FindAction::Next, Some((true, current))) => ((current + 1) % count, true),
        (FindAction::Previous, Some((true, current))) => ((current + count - 1) % count, true),
        (FindAction::Refresh, Some((true, current))) => (current.min(count - 1), false),
        // Search, or Next/Previous/Refresh of a query we do not remember: start from the first match.
        _ => (0, request.action != FindAction::Refresh),
    };
    STATES.with(|states| {
        states.borrow_mut().insert(
            pipeline_id,
            FindState {
                query: request.query.clone(),
                case_sensitive: request.case_sensitive,
                active,
            },
        )
    });

    // 5. Highlights per text node.
    let mut per_node: FxHashMap<usize, Vec<FindHighlight>> = FxHashMap::default();
    for (match_index, segments) in matches.iter().enumerate() {
        for segment in segments {
            per_node.entry(segment.node).or_default().push(FindHighlight {
                range: RangeAny::new(
                    Some(Utf32CodeUnits(segment.start)),
                    Some(Utf32CodeUnits(segment.end)),
                ),
                active: match_index == active,
            });
        }
    }
    for (index, (character_data, _, _)) in texts.iter().enumerate() {
        character_data.set_text_run_find_highlights(per_node.remove(&index).unwrap_or_default());
    }
    window.layout().set_needs_new_display_list();

    // 6. Bring the active match into view.
    if scroll {
        if let Some(segment) = matches[active].first() {
            let node = texts[segment.node].0.upcast::<Node>();
            if let Some(parent) = node.GetParentElement() {
                parent.scroll_into_view_with_options(
                    cx,
                    ScrollBehavior::Instant,
                    ScrollAxisState::new_always_scroll_position(ScrollLogicalPosition::Center),
                    ScrollAxisState::new_always_scroll_position(ScrollLogicalPosition::Nearest),
                    None,
                    None,
                );
            }
        }
    }
    (count, Some(active))
}

//! Resolve style and layout

use blitz_traits::node_id::NodeId;
use std::cell::RefCell;

use debug_timer::debug_timer;
use kurbo::{Affine, Rect};
use parley::LayoutContext;
use selectors::Element as _;
use style::dom::TDocument;

#[cfg(feature = "parallel-construct")]
use rayon::prelude::*;

// FIXME: static thread_local FontCtx isn't necessarily correct in multi-document context.
// Should use thread_local crate with ThreadLocal value store in the Document.
thread_local! {
    pub(crate) static LAYOUT_CTX: RefCell<Option<Box<LayoutContext<TextBrush>>>> = const { RefCell::new(None) };
}

use style::selector_parser::RestyleDamage;
use taffy::AvailableSpace;

use crate::{
    BaseDocument,
    layout::{
        construct::{
            ConstructionTask, ConstructionTaskData, ConstructionTaskResult,
            ConstructionTaskResultData, LayoutChildren, build_inline_layout_into,
            collect_layout_children,
        },
        damage::{ALL_DAMAGE, CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC},
    },
    node::TextBrush,
};

impl BaseDocument {
    /// Restyle the tree and then relayout it
    pub fn resolve(&mut self, current_time_for_animations: f64) {
        if TDocument::as_node(&self.root_node())
            .first_element_child()
            .is_none()
        {
            #[cfg(feature = "tracing")]
            tracing::warn!("No DOM - not resolving");
            return;
        }

        // Process messages that have been sent to our message channel (e.g. loaded resource)
        self.handle_messages();

        // While render-blocking resources (e.g. stylesheets linked from the `<head>`) are
        // still loading, don't resolve styles or layout (matching how browsers block
        // rendering). Resolving styles before the document's stylesheets have loaded would
        // give elements computed styles based on an incomplete cascade, and a later restyle
        // (once the stylesheet loads) would treat those as genuine "before-change styles",
        // spuriously starting CSS transitions from unstyled values. See issue #689.
        //
        // `handle_messages` above must still run so that loaded resources are ingested and
        // this state can clear.
        if self.has_pending_critical_resources() {
            return;
        }

        self.resolve_scroll_animation();

        // Drop scrollbar-activity entries whose fade-out has finished (also
        // sheds entries for removed nodes).
        {
            use crate::node::scrollbar::{FADE_DELAY, FADE_DURATION};
            self.scrollbar_activity
                .retain(|_, last| last.elapsed() < FADE_DELAY + FADE_DURATION);
        }

        let root_node_id = self.root_element().id;
        debug_timer!(timer, feature = "log-phase-times");

        // Apply any device changes (viewport resize, zoom, color-scheme, etc)
        // accumulated since the last resolve as a single device rebuild.
        self.flush_pending_device_changes();

        // we need to resolve stylist first since it will need to drive our layout bits
        let current_time_for_animations =
            self.animation_time().unwrap_or(current_time_for_animations);
        self.resolve_stylist(current_time_for_animations);
        timer.record_time("style");

        // Propagate damage flags (from mutation and restyles) up and down the tree
        if self.incremental_layout {
            self.propagate_damage_flags(root_node_id, RestyleDamage::empty());
            timer.record_time("damage");
        }

        // Fix up tree for layout (insert anonymous blocks as necessary, etc)
        self.resolve_layout_children();
        timer.record_time("construct");

        self.resolve_deferred_tasks();
        // Flush background/mask images from style to dedicated storage on the
        // nodes whose style changed (queued by the style traversal and by
        // pseudo-element box construction), fetching any not-yet-loaded images.
        self.flush_pending_style_images();
        timer.record_time("pconstruct");

        // Merge stylo into taffy
        self.flush_styles_to_layout(root_node_id);
        timer.record_time("flush");

        // Next we resolve layout with the data resolved by stlist
        self.resolve_layout();
        timer.record_time("layout");

        // Resolve transforms
        self.resolve_transforms(root_node_id);
        timer.record_time("transform");

        // Clear all damage and dirty flags, walking only subtrees which are
        // marked as (potentially) containing damage.
        if self.incremental_layout {
            let doc_node_id = self.root_node().id;
            self.clear_damage_and_dirty_flags(doc_node_id);
            timer.record_time("c_damage");
        }

        // Re-resolve the hover node from the pointer position against the fresh
        // layout. This must run *after* the damage/dirty flags are cleared
        // above, so that the restyle hint and ancestor `dirty_descendants`
        // flags set by any resulting hover change survive into the next resolve
        // pass (the clearing loop would otherwise wipe them). Any resulting
        // restyle is picked up on the next resolve pass; a redraw is requested
        // if the hovered node actually changes.
        self.refresh_hover();

        let mut subdoc_is_animating = false;
        for &node_id in &self.sub_document_nodes {
            let node = &mut self.nodes[node_id];
            let size = node.final_layout().size;
            if let Some(mut sub_doc) = node.subdoc_mut().map(|doc| doc.inner_mut()) {
                // Set viewport
                // viewport_mut handles change detection. So we just unconditionally set the values;
                let mut sub_viewport = sub_doc.viewport_mut();
                sub_viewport.hidpi_scale = self.viewport.hidpi_scale;
                sub_viewport.zoom = self.viewport.zoom;
                sub_viewport.color_scheme = self.viewport.color_scheme;

                let viewport_scale = self.viewport.scale();
                sub_viewport.window_size = (
                    (size.width * viewport_scale) as u32,
                    (size.height * viewport_scale) as u32,
                );
                drop(sub_viewport);

                sub_doc.resolve(current_time_for_animations);

                subdoc_is_animating |= sub_doc.is_animating();
            }
        }
        self.subdoc_is_animating = subdoc_is_animating;
        timer.record_time("subdocs");

        timer.print_times(&format!("Resolve({}): ", self.id()));
    }

    fn resolve_transforms(&mut self, node_id: NodeId) -> Rect {
        if !self.nodes.contains_key(node_id) {
            return Rect::ZERO;
        }

        let scale = self.viewport.scale_f64();

        if !self.nodes[node_id]
            .damage()
            .map(|d| d.contains(style::selector_parser::RestyleDamage::RECALCULATE_OVERFLOW))
            .unwrap_or(false)
        {
            let node = &self.nodes[node_id];
            let location = node.final_layout().location.map(|v| v as f64 * scale);

            let mut transform = Affine::translate((location.x, location.y));
            if let Some(t) = node.transform().as_deref() {
                transform *= *t
            }

            let overflow = *node.scrollable_overflow();
            return transform.transform_rect_bbox(overflow);
        }

        let transform = self.nodes[node_id].set_transform(scale as f32);

        let w = self.nodes[node_id].final_layout().size.width as f64 * scale;
        let h = self.nodes[node_id].final_layout().size.height as f64 * scale;
        let mut overflow = Rect::new(0.0, 0.0, w, h);

        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());

        if let Some(ref children) = layout_children {
            for &child_id in children {
                let child_rect_in_self = self.resolve_transforms(child_id);
                overflow = overflow.union(child_rect_in_self);
            }
        }
        if let Some(before) = self.nodes[node_id].before() {
            let child_rect_in_self = self.resolve_transforms(before);
            overflow = overflow.union(child_rect_in_self);
        }
        if let Some(after) = self.nodes[node_id].after() {
            let child_rect_in_self = self.resolve_transforms(after);
            overflow = overflow.union(child_rect_in_self);
        }

        *self.nodes[node_id].scrollable_overflow_mut() = overflow;
        *self.nodes[node_id].layout_children.get_mut() = layout_children;

        let scaled_x = self.nodes[node_id].final_layout().location.x as f64 * scale;
        let scaled_y = self.nodes[node_id].final_layout().location.y as f64 * scale;

        let full = if let Some(t) = transform {
            Affine::translate((scaled_x, scaled_y)) * t
        } else {
            Affine::translate((scaled_x, scaled_y))
        };

        full.transform_rect_bbox(overflow)
    }

    /// Ensure that the layout_children field is populated for all nodes
    pub fn resolve_layout_children(&mut self) {
        resolve_layout_children_recursive(self, self.root_node().id);

        fn resolve_layout_children_recursive(doc: &mut BaseDocument, node_id: NodeId) {
            // Anonymous blocks and pseudo-elements can be removed from the slab
            // between render passes. Bail out rather than panicking on a stale key.
            if doc.nodes.get(node_id).is_none() {
                return;
            }

            let mut damage = doc.nodes[node_id].damage().unwrap_or(ALL_DAMAGE);
            let _flags = doc.nodes[node_id].flags;

            if !doc.incremental_layout || damage.intersects(CONSTRUCT_FC | CONSTRUCT_BOX) {
                //} || flags.contains(NodeFlags::IS_INLINE_ROOT) {

                // Deallocate the anonymous blocks created for this node in the
                // previous construction round. They live only in the slab, so
                // reconstructing without freeing them would leak a slab entry per
                // anonymous block per reconstruction.
                let old_anonymous_blocks = std::mem::take(&mut doc.nodes[node_id].anonymous_blocks);
                for anon_id in old_anonymous_blocks {
                    doc.deallocate_anonymous_block(anon_id);
                }

                let mut collected = LayoutChildren::default();
                collect_layout_children(doc, node_id, &mut collected);
                let layout_children = collected.children;
                doc.nodes[node_id].anonymous_blocks = collected.anonymous_blocks;

                // Recurse into newly collected layout children
                for child_id in layout_children.iter().copied() {
                    resolve_layout_children_recursive(doc, child_id);
                    doc.nodes[child_id].layout_parent.set(Some(node_id));
                    if let Some(mut data) = doc.nodes[child_id]
                        .try_stylo_element_data_mut()
                        .and_then(|s| s.get_mut())
                    {
                        data.damage
                            .remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                }

                *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children.clone());
                // *doc.nodes[node_id].paint_children.borrow_mut() = Some(layout_children);

                damage.remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            } else {
                //if damage.contains(CONSTRUCT_DESCENDENT) {
                let layout_children = doc.nodes[node_id].layout_children.borrow_mut().take();
                if let Some(layout_children) = layout_children {
                    for child_id in layout_children.iter().copied() {
                        // Anonymous blocks and pseudo-elements can be removed from the
                        // slab between render passes; skip stale IDs.
                        if !doc.nodes.contains_key(child_id) {
                            continue;
                        }
                        resolve_layout_children_recursive(doc, child_id);
                        doc.nodes[child_id].layout_parent.set(Some(node_id));
                    }

                    *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children);
                }

                // damage.remove(CONSTRUCT_DESCENDENT);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            }

            doc.nodes[node_id].set_damage(damage);
        }
    }

    pub fn resolve_deferred_tasks(&mut self) {
        let mut deferred_construction_nodes = std::mem::take(&mut self.deferred_construction_nodes);

        // Deduplicate deferred tasks by node_id to avoid redundant work
        deferred_construction_nodes.sort_unstable_by_key(|task| task.node_id);
        deferred_construction_nodes.dedup_by_key(|task| task.node_id);

        #[cfg(feature = "parallel-construct")]
        let iter = deferred_construction_nodes.into_par_iter();
        #[cfg(not(feature = "parallel-construct"))]
        let iter = deferred_construction_nodes.into_iter();

        let results: Vec<ConstructionTaskResult> = iter
            .map(|task: ConstructionTask| match task.data {
                ConstructionTaskData::InlineLayout(mut layout) => {
                    #[cfg(feature = "parallel-construct")]
                    let mut layout_ctx = LAYOUT_CTX
                        .take()
                        .unwrap_or_else(|| Box::new(LayoutContext::new()));
                    #[cfg(feature = "parallel-construct")]
                    let layout_ctx_mut = &mut layout_ctx;

                    #[cfg(feature = "parallel-construct")]
                    let mut font_ctx = self
                        .thread_font_contexts
                        .get_or(|| RefCell::new(Box::new(self.font_ctx.lock().unwrap().clone())))
                        .borrow_mut();
                    #[cfg(feature = "parallel-construct")]
                    let font_ctx_mut = &mut *font_ctx;

                    #[cfg(not(feature = "parallel-construct"))]
                    let layout_ctx_mut = &mut self.layout_ctx;
                    #[cfg(not(feature = "parallel-construct"))]
                    let font_ctx_mut = &mut *self.font_ctx.lock().unwrap();

                    layout.content_widths = None;
                    build_inline_layout_into(
                        &self.nodes,
                        layout_ctx_mut,
                        font_ctx_mut,
                        &mut layout,
                        self.viewport.scale(),
                        task.node_id,
                    );

                    #[cfg(feature = "parallel-construct")]
                    {
                        LAYOUT_CTX.set(Some(layout_ctx));
                    }

                    // If layout doesn't contain any inline boxes, then it is safe to populate the content_widths
                    // cache during this parallelized stage.
                    // if layout.layout.inline_boxes().is_empty() {
                    //     layout.content_widths();
                    // }

                    ConstructionTaskResult {
                        node_id: task.node_id,
                        data: ConstructionTaskResultData::InlineLayout(layout),
                    }
                }
            })
            .collect();

        for result in results {
            match result.data {
                ConstructionTaskResultData::InlineLayout(layout) => {
                    self.nodes[result.node_id].clear_layout_cache();
                    self.nodes[result.node_id]
                        .element_data_mut()
                        .unwrap()
                        .inline_layout_data = Some(layout);
                }
            }
        }

        self.deferred_construction_nodes.clear();
    }

    /// Walk the nodes now that they're properly styled and transfer their styles to the taffy style system
    ///
    /// TODO: update taffy to use an associated type instead of slab key
    /// TODO: update taffy to support traited styles so we don't even need to rely on taffy for storage
    pub fn resolve_layout(&mut self) {
        let size = self.stylist.device().au_viewport_size();

        let available_space = taffy::Size {
            width: AvailableSpace::Definite(size.width.to_f32_px()),
            height: AvailableSpace::Definite(size.height.to_f32_px()),
        };

        let root_element_id = crate::taffy_node_id(self.root_element().id);

        // println!("\n\nRESOLVE LAYOUT\n===========\n");

        taffy::compute_root_layout(self, root_element_id, available_space);
        taffy::round_layout(self, root_element_id);
        self.place_absolute_boxes_in_their_containing_blocks();

        // println!("\n\n");
        // taffy::print_tree(self, root_node_id)
    }
}

/// A box's containing block for `position: absolute` is the padding box of
/// its nearest positioned ancestor, and the initial containing block when
/// there is none (CSS 2.1 §10.1). The layout algorithms place an absolutely
/// positioned box against its *layout parent*, whatever that parent's
/// `position`, so a box whose parent is not positioned lands in the wrong
/// place: `top: 0` puts it at the top of the parent instead of the top of the
/// page. This pass runs after layout and moves such boxes to where their
/// insets say relative to the right containing block. Sizes are left as they
/// were laid out; it is the placement that is corrected.
///
/// A `position: fixed` box's containing block is the viewport (CSS Positioned
/// Layout §3.1), not its nearest positioned ancestor -- unless an ancestor has
/// a transform, perspective, filter or paint/layout containment, which makes
/// that ancestor the containing block for fixed (and absolute) descendants.
impl BaseDocument {
    fn place_absolute_boxes_in_their_containing_blocks(&mut self) {
        use style::computed_values::position::T as Position;
        use taffy::{CoreStyle as _, MaybeResolve as _};

        /// A containing block: where its padding box is on the page.
        #[derive(Clone, Copy)]
        struct Block {
            x: f32,
            y: f32,
            width: f32,
            height: f32,
        }

        /// Whether the box is the containing block of its fixed (and absolute)
        /// descendants because of a transform, perspective, filter or
        /// paint/layout containment (CSS Transforms §2, Filter Effects §5,
        /// CSS Containment §3).
        fn contains_fixed(style: &style::properties::ComputedValues) -> bool {
            use style::values::computed::Contain;
            let box_style = style.get_box();
            !box_style.transform.0.is_empty()
                || !matches!(
                    box_style.perspective,
                    style::values::generics::box_::Perspective::None
                )
                || !style.get_effects().filter.0.is_empty()
                || box_style
                    .contain
                    .intersects(Contain::PAINT | Contain::LAYOUT)
        }

        /// Where a node is on the page: rounded (`final_layout`) and
        /// unrounded (`unrounded_layout`), accumulated down the tree.
        #[derive(Clone, Copy)]
        struct Origin {
            rounded: (f32, f32),
            unrounded: (f32, f32),
        }

        fn walk(
            doc: &mut BaseDocument,
            node_id: NodeId,
            parent_origin: Origin,
            block: Block,
            fixed_block: Block,
            reround: bool,
        ) {
            let Some(node) = doc.nodes.get(node_id) else {
                return;
            };
            let position = node
                .primary_styles()
                .map(|style| style.clone_position())
                .unwrap_or(Position::Static);
            let parent = node.layout_parent.get().and_then(|id| doc.nodes.get(id));
            let parent_styles = parent.and_then(|parent| parent.primary_styles());
            let parent_contains_fixed = parent_styles
                .as_ref()
                .is_some_and(|style| contains_fixed(style));
            let parent_is_block = parent_contains_fixed
                || parent_styles
                    .as_ref()
                    .map(|style| style.clone_position())
                    .is_none_or(|p| p != Position::Static);
            drop(parent_styles);

            // The layout algorithms already placed it against its layout
            // parent; move it only when that is not its containing block.
            let block_of_box = match position {
                Position::Absolute if !parent_is_block => Some(block),
                Position::Fixed if !parent_contains_fixed => Some(fixed_block),
                _ => None,
            };
            if let Some(block) = block_of_box {
                // Placed in unrounded coordinates, and the rounded position
                // derived from that (as Taffy rounds: the rounded page
                // position less the parent's), so that placing it again on
                // the next pass gives the same result.
                let layout = *node.unrounded_layout();
                let style = node.layout_style();
                let inset = style.inset();
                let calc = crate::layout::resolve_calc_value;
                let left = inset.left.maybe_resolve(Some(block.width), calc);
                let right = inset.right.maybe_resolve(Some(block.width), calc);
                let top = inset.top.maybe_resolve(Some(block.height), calc);
                let bottom = inset.bottom.maybe_resolve(Some(block.height), calc);
                drop(style);
                let margin = layout.margin;
                // Where the box is now, on the page.
                let now_x = parent_origin.unrounded.0 + layout.location.x;
                let now_y = parent_origin.unrounded.1 + layout.location.y;
                // Where its insets put it, against the containing block. With
                // neither inset it stays at its static position.
                let x = match (left, right) {
                    (Some(left), _) => block.x + left + margin.left,
                    (None, Some(right)) => {
                        block.x + block.width - right - margin.right - layout.size.width
                    }
                    (None, None) => now_x,
                };
                let y = match (top, bottom) {
                    (Some(top), _) => block.y + top + margin.top,
                    (None, Some(bottom)) => {
                        block.y + block.height - bottom - margin.bottom - layout.size.height
                    }
                    (None, None) => now_y,
                };
                let placed = doc.nodes[node_id].unrounded_layout_mut();
                placed.location.x = x - parent_origin.unrounded.0;
                placed.location.y = y - parent_origin.unrounded.1;
            }
            // A placed box, and everything in it, is rounded again from where
            // it now is: its descendants were rounded at its old position.
            let reround = reround || block_of_box.is_some();
            if reround {
                let unrounded = *doc.nodes[node_id].unrounded_layout();
                let x = parent_origin.unrounded.0 + unrounded.location.x;
                let y = parent_origin.unrounded.1 + unrounded.location.y;
                let rounded = doc.nodes[node_id].final_layout_mut();
                rounded.location.x = x.round() - parent_origin.rounded.0;
                rounded.location.y = y.round() - parent_origin.rounded.1;
                rounded.size.width = (x + unrounded.size.width).round() - x.round();
                rounded.size.height = (y + unrounded.size.height).round() - y.round();
            }

            let node = &doc.nodes[node_id];
            let layout = *node.final_layout();
            let unrounded = *node.unrounded_layout();
            let origin = Origin {
                rounded: (
                    parent_origin.rounded.0 + layout.location.x,
                    parent_origin.rounded.1 + layout.location.y,
                ),
                unrounded: (
                    parent_origin.unrounded.0 + unrounded.location.x,
                    parent_origin.unrounded.1 + unrounded.location.y,
                ),
            };
            let establishes_fixed = node
                .primary_styles()
                .is_some_and(|style| contains_fixed(&style));
            let padding_box = Block {
                x: origin.unrounded.0 + unrounded.border.left,
                y: origin.unrounded.1 + unrounded.border.top,
                width: unrounded.size.width - unrounded.border.left - unrounded.border.right,
                height: unrounded.size.height - unrounded.border.top - unrounded.border.bottom,
            };
            let block = if position != Position::Static || establishes_fixed {
                padding_box
            } else {
                block
            };
            let fixed_block = if establishes_fixed {
                padding_box
            } else {
                fixed_block
            };
            let children: Vec<NodeId> = node
                .layout_children
                .borrow()
                .as_ref()
                .map(|c| c.to_vec())
                .unwrap_or_default();
            for child in children {
                walk(doc, child, origin, block, fixed_block, reround);
            }
        }

        let root = self.root_element().id;
        let layout = *self.nodes[root].unrounded_layout();
        let block = Block {
            x: layout.location.x,
            y: layout.location.y,
            width: layout.size.width,
            height: layout.size.height,
        };
        // The viewport, where it is on the page (it scrolls over the page).
        let viewport = self.stylist.device().au_viewport_size();
        let fixed_block = Block {
            x: self.viewport_scroll.x as f32,
            y: self.viewport_scroll.y as f32,
            width: viewport.width.to_f32_px(),
            height: viewport.height.to_f32_px(),
        };
        let origin = Origin {
            rounded: (0.0, 0.0),
            unrounded: (0.0, 0.0),
        };
        walk(self, root, origin, block, fixed_block, false);
    }
}

#[cfg(test)]
mod fixed_position_tests {
    use crate::{Attribute, BaseDocument, DocumentConfig, NodeId, qual_name};
    use blitz_traits::shell::{ColorScheme, Viewport};

    fn style(value: &str) -> Attribute {
        Attribute {
            name: qual_name!("style"),
            value: value.to_string(),
        }
    }

    /// Lays out `<body>` > a box styled `outer` at (320, 130) > a fixed box at
    /// (290, 130) of its containing block, and returns where the fixed box is
    /// on the page.
    fn fixed_in(outer: &str) -> (f32, f32) {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = doc.root_node().id;
        let mut mutator = doc.mutate();
        let html = mutator.create_element(qual_name!("html"), vec![]);
        let body =
            mutator.create_element(qual_name!("body"), vec![style("display:block;margin:0")]);
        let container = mutator.create_element(
            qual_name!("div"),
            vec![style(&format!(
                "display:block;left:320px;top:130px;width:300px;height:300px;{outer}"
            ))],
        );
        let fixed: NodeId = mutator.create_element(
            qual_name!("div"),
            vec![style(
                "display:block;position:fixed;left:290px;top:130px;width:50px;height:50px",
            )],
        );
        mutator.append_children(container, &[fixed]);
        mutator.append_children(body, &[container]);
        mutator.append_children(html, &[body]);
        mutator.append_children(root_id, &[html]);
        drop(mutator);
        doc.resolve(0.0);
        let at = doc.nodes[fixed].absolute_position(0.0, 0.0);
        (at.x, at.y)
    }

    /// CSS Positioned Layout §3.1: the containing block of a fixed box is the
    /// viewport, not its positioned ancestor.
    #[test]
    fn a_fixed_box_is_placed_against_the_viewport() {
        assert_eq!(fixed_in("position:absolute"), (290.0, 130.0));
        assert_eq!(fixed_in("position:relative"), (290.0, 130.0));
    }

    /// An ancestor with a transform is the containing block of its fixed
    /// descendants (CSS Transforms §2).
    #[test]
    fn a_transformed_ancestor_contains_fixed_boxes() {
        assert_eq!(
            fixed_in("position:absolute;transform:translate(0px, 0px)"),
            (320.0 + 290.0, 130.0 + 130.0)
        );
    }

    /// Placing a box against a containing block that is not its parent, at a
    /// fractional scale, gives the same position on every layout pass (the
    /// box used to alternate between two rounded positions).
    #[test]
    fn placed_boxes_stay_put_across_layout_passes() {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(1162, 653, 0.6052, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = doc.root_node().id;
        let mut mutator = doc.mutate();
        let html = mutator.create_element(qual_name!("html"), vec![]);
        let body = mutator.create_element(
            qual_name!("body"),
            vec![style(
                "margin:0;position:relative;width:1920px;height:1079.3px",
            )],
        );
        let column = mutator.create_element(
            qual_name!("div"),
            vec![style("position:static;height:0;margin-top:1079.3px")],
        );
        let panel = mutator.create_element(
            qual_name!("div"),
            vec![style(
                "position:absolute;left:669px;bottom:40.7px;width:1199px;height:309.7px",
            )],
        );
        let inner = mutator.create_element(
            qual_name!("div"),
            vec![style("position:relative;top:10.3px;height:100px")],
        );
        mutator.append_children(panel, &[inner]);
        mutator.append_children(column, &[panel]);
        mutator.append_children(body, &[column]);
        mutator.append_children(html, &[body]);
        mutator.append_children(root_id, &[html]);
        drop(mutator);
        doc.resolve(0.0);
        let first = (
            doc.nodes[panel].final_layout().location,
            doc.nodes[inner].final_layout().location,
        );
        for _ in 0..5 {
            doc.resolve(0.0);
            assert_eq!(
                (
                    doc.nodes[panel].final_layout().location,
                    doc.nodes[inner].final_layout().location,
                ),
                first
            );
        }
    }
}

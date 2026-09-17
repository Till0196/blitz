use blitz_traits::node_id::NodeId;
use std::ops::Range;

use crate::Node;
use crate::net::ResourceHandler;
use crate::node::NodeFlags;
use crate::{
    BaseDocument, net::ImageHandler, node::ImageResourceData, node::Status, util::ImageLayerKind,
};
use style::properties::ComputedValues;
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::selector_parser::RestyleDamage;
use style::url::ComputedUrl;
use style::values::computed::Float;
use style::values::computed::Overflow;
use style::values::generics::image::Image as StyloImage;
use style::values::specified::align::AlignFlags;
use style::values::specified::box_::DisplayInside;
use style::values::specified::box_::DisplayOutside;
use taffy::Rect;
use thin_vec::ThinVec;

pub(crate) const CONSTRUCT_BOX: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0001_0000);
pub(crate) const CONSTRUCT_FC: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0010_0000);
pub(crate) const CONSTRUCT_DESCENDENT: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0100_0000);

pub(crate) const ONLY_RELAYOUT: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0000_1000);

pub(crate) const ALL_DAMAGE: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0111_1111);

impl BaseDocument {
    pub(crate) fn propagate_damage_flags(
        &mut self,
        node_id: NodeId,
        damage_from_parent: RestyleDamage,
    ) -> RestyleDamage {
        let mut damage = if let Some(data) = self.nodes[node_id]
            .try_stylo_element_data_mut()
            .and_then(|s| s.get_mut())
        {
            data.damage
        } else {
            return RestyleDamage::empty();
        };
        damage |= damage_from_parent;

        // Skip subtrees which contain no damage. Anonymous nodes are never
        // skipped themselves because damage marking walks the DOM parent
        // chain, which bypasses anonymous boxes: a damaged node's flagged
        // ancestors may reach it only through an unflagged anonymous wrapper.
        {
            let node = &self.nodes[node_id];
            if damage.is_empty() && !node.has_damaged_descendants() && !node.is_anonymous() {
                return RestyleDamage::empty();
            }
        }

        let damage_for_children = RestyleDamage::empty();
        let children = std::mem::take(&mut self.nodes[node_id].children);
        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());
        let use_layout_children = self.nodes[node_id].should_traverse_layout_children();
        if use_layout_children {
            let layout_children = layout_children.as_ref().unwrap();
            for child in layout_children.iter() {
                damage |= self.propagate_damage_flags(*child, damage_for_children);
            }
        } else {
            for child in children.iter() {
                damage |= self.propagate_damage_flags(*child, damage_for_children);
            }
            if let Some(before_id) = self.nodes[node_id].before() {
                damage |= self.propagate_damage_flags(before_id, damage_for_children);
            }
            if let Some(after_id) = self.nodes[node_id].after() {
                damage |= self.propagate_damage_flags(after_id, damage_for_children);
            }
        }

        let node = &mut self.nodes[node_id];

        // Put children back
        node.children = children;
        *node.layout_children.get_mut() = layout_children;

        if damage.contains(CONSTRUCT_BOX) {
            damage.insert(RestyleDamage::RELAYOUT);
        }

        // Compute damage to propagate to parent
        let damage_for_parent = damage; // & RestyleDamage::RELAYOUT;

        // If the node or any of it's children have been mutated or their layout styles
        // have changed, then we should clear it's layout cache.
        if damage.intersects(ONLY_RELAYOUT | CONSTRUCT_BOX) {
            node.clear_layout_cache();
            if let Some(inline_layout) = node
                .data
                .downcast_element_mut()
                .and_then(|el| el.inline_layout_data.as_mut())
            {
                inline_layout.content_widths = None;
            }
            damage.remove(ONLY_RELAYOUT);
        }

        // Store damage for current node
        node.set_damage(damage);

        // let _is_fc_root = node
        //     .primary_styles()
        //     .map(|s| is_fc_root(&s))
        //     .unwrap_or(false);

        // if damage.contains(CONSTRUCT_BOX) {
        //     // damage_for_parent.insert(CONSTRUCT_FC | CONSTRUCT_DESCENDENT);
        //     damage_for_parent.insert(CONSTRUCT_BOX);
        // }

        // if damage.contains(CONSTRUCT_FC) {
        //     damage_for_parent.insert(CONSTRUCT_DESCENDENT);
        //     // if !is_fc_root {
        //     damage_for_parent.insert(CONSTRUCT_FC);
        //     // }
        // }

        // Propagate damage to parent
        damage_for_parent
    }

    /// Clear damage and the `damaged_descendants`/`dirty_descendants` flags
    /// on all nodes which may carry them, using the `damaged_descendants`
    /// flags to skip clean subtrees (mirroring `propagate_damage_flags`).
    pub(crate) fn clear_damage_and_dirty_flags(&mut self, node_id: NodeId) {
        {
            let node = &self.nodes[node_id];
            let has_damage = node.damage().is_some_and(|d| !d.is_empty());
            if !has_damage && !node.has_damaged_descendants() && !node.is_anonymous() {
                return;
            }
        }

        let children = std::mem::take(&mut self.nodes[node_id].children);
        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());
        for child in children.iter() {
            self.clear_damage_and_dirty_flags(*child);
        }
        if let Some(layout_children) = layout_children.as_ref() {
            for child in layout_children.iter() {
                self.clear_damage_and_dirty_flags(*child);
            }
        }
        if let Some(before_id) = self.nodes[node_id].before() {
            self.clear_damage_and_dirty_flags(before_id);
        }
        if let Some(after_id) = self.nodes[node_id].after() {
            self.clear_damage_and_dirty_flags(after_id);
        }

        let node = &mut self.nodes[node_id];
        node.children = children;
        *node.layout_children.get_mut() = layout_children;
        node.clear_damage_mut();
        node.unset_damaged_descendants();
        node.unset_dirty_descendants();
    }
}

// fn is_fc_root(style: &ComputedValues) -> bool {
//     let display = style.clone_display();
//     let display_inside = display.inside();

//     match display_inside {
//         DisplayInside::Flow => {
//             // Depends on parent context
//             false
//         }

//         DisplayInside::None => true,
//         DisplayInside::FlowRoot => true,
//         DisplayInside::Flex => true,
//         DisplayInside::Grid => true,
//         DisplayInside::Table => true,
//         DisplayInside::TableCell => true,

//         DisplayInside::Contents => false,
//         DisplayInside::TableRowGroup => false,
//         DisplayInside::TableColumn => false,
//         DisplayInside::TableColumnGroup => false,
//         DisplayInside::TableHeaderGroup => false,
//         DisplayInside::TableFooterGroup => false,
//         DisplayInside::TableRow => false,
//     }
// }

pub(crate) fn compute_layout_damage(old: &ComputedValues, new: &ComputedValues) -> RestyleDamage {
    let box_tree_needs_rebuild = || {
        let old_box = old.get_box();
        let new_box = new.get_box();

        if old_box.display != new_box.display
            || old_box.float != new_box.float
            || old_box.position != new_box.position
            || old_box.contain != new_box.contain
            || old.clone_visibility() != new.clone_visibility()
        {
            return true;
        }

        if old.get_font() != new.get_font() {
            return true;
        }

        if new_box.display.outside() == DisplayOutside::Block
            && new_box.display.inside() == DisplayInside::Flow
        {
            let alignment_establishes_new_block_formatting_context = |style: &ComputedValues| {
                style.get_position().align_content.primary() != AlignFlags::NORMAL
            };

            let old_column = old.get_column();
            let new_column = new.get_column();
            if old_box.overflow_x.is_scrollable() != new_box.overflow_x.is_scrollable()
                || old_column.is_multicol() != new_column.is_multicol()
                || old_column.column_span != new_column.column_span
                || alignment_establishes_new_block_formatting_context(old)
                    != alignment_establishes_new_block_formatting_context(new)
            {
                return true;
            }
        }

        if old_box.display.is_list_item() {
            let old_list = old.get_list();
            let new_list = new.get_list();
            if old_list.list_style_position != new_list.list_style_position
                || old_list.list_style_image != new_list.list_style_image
                || (new_list.list_style_image == StyloImage::None
                    && old_list.list_style_type != new_list.list_style_type)
            {
                return true;
            }
        }

        if new.is_pseudo_style() && old.get_counters().content != new.get_counters().content {
            return true;
        }

        false
    };

    let text_shaping_needs_recollect = || {
        if old.clone_direction() != new.clone_direction()
            || old.clone_unicode_bidi() != new.clone_unicode_bidi()
        {
            return true;
        }

        let old_text = old.get_inherited_text();
        let new_text = new.get_inherited_text();
        if !std::ptr::eq(old_text, new_text)
            && (old_text.white_space_collapse != new_text.white_space_collapse
                || old_text.text_transform != new_text.text_transform
                || old_text.word_break != new_text.word_break
                || old_text.overflow_wrap != new_text.overflow_wrap
                || old_text.letter_spacing != new_text.letter_spacing
                || old_text.word_spacing != new_text.word_spacing
                || old_text.text_rendering != new_text.text_rendering)
        {
            return true;
        }

        false
    };

    #[allow(
        clippy::if_same_then_else,
        reason = "these branches will soon be different"
    )]
    if box_tree_needs_rebuild() {
        ALL_DAMAGE
    } else if text_shaping_needs_recollect() {
        ALL_DAMAGE
    } else {
        // This element needs to be laid out again, but does not have any damage to
        // its box. In the future, we will distinguish between types of damage to the
        // fragment as well.
        RestyleDamage::RELAYOUT
    }
}

/// A child with a z_index that is hoisted up to it's containing Stacking Context for paint purposes
#[derive(Debug, Clone)]
pub struct HoistedPaintChild {
    pub node_id: NodeId,
    pub z_index: i32,
    pub position: taffy::Point<f32>,
}

#[derive(Debug)]
pub struct HoistedPaintChildren {
    pub children: Vec<HoistedPaintChild>,
    /// The number of hoisted point children with negative z_index
    pub negative_z_count: u32,

    pub content_area: taffy::Rect<f32>,
}

impl HoistedPaintChildren {
    fn new() -> Self {
        Self {
            children: Vec::new(),
            negative_z_count: 0,
            content_area: taffy::Rect::ZERO,
        }
    }

    pub fn reset(&mut self) {
        self.children.clear();
        self.negative_z_count = 0;
    }

    pub fn compute_content_size(&mut self, doc: &BaseDocument) {
        fn child_pos(child: &HoistedPaintChild, doc: &BaseDocument) -> Rect<f32> {
            let node = &doc.nodes[child.node_id];
            let left = child.position.x + node.final_layout().location.x;
            let top = child.position.y + node.final_layout().location.y;
            let right = left + node.final_layout().size.width;
            let bottom = top + node.final_layout().size.height;

            taffy::Rect {
                top,
                left,
                bottom,
                right,
            }
        }

        if self.children.is_empty() {
            self.content_area = taffy::Rect::ZERO;
        } else {
            self.content_area = child_pos(&self.children[0], doc);
            for child in self.children[1..].iter() {
                let pos = child_pos(child, doc);
                self.content_area.left = self.content_area.left.min(pos.left);
                self.content_area.top = self.content_area.top.min(pos.top);
                self.content_area.right = self.content_area.right.max(pos.right);
                self.content_area.bottom = self.content_area.bottom.max(pos.bottom);
            }
        }
    }

    pub fn sort(&mut self) {
        self.children.sort_by_key(|c| c.z_index);
        self.negative_z_count = self.children.iter().take_while(|c| c.z_index < 0).count() as u32;
    }

    pub fn neg_z_range(&self) -> Range<usize> {
        0..(self.negative_z_count as usize)
    }

    pub fn pos_z_range(&self) -> Range<usize> {
        (self.negative_z_count as usize)..self.children.len()
    }

    pub fn neg_z_hoisted_children(
        &self,
    ) -> impl ExactSizeIterator<Item = &HoistedPaintChild> + DoubleEndedIterator {
        self.children[self.neg_z_range()].iter()
    }

    pub fn pos_z_hoisted_children(
        &self,
    ) -> impl ExactSizeIterator<Item = &HoistedPaintChild> + DoubleEndedIterator {
        self.children[self.pos_z_range()].iter()
    }
}

impl BaseDocument {
    pub(crate) fn invalidate_inline_contexts(&mut self) {
        let scale = self.viewport.scale();

        let font_ctx = &self.font_ctx;
        let layout_ctx = &mut self.layout_ctx;

        let mut anon_nodes = Vec::new();

        for (_, node) in self.nodes.iter_mut() {
            if !(node.flags.contains(NodeFlags::IS_IN_DOCUMENT)) {
                continue;
            }

            let Some(element) = node.data.downcast_element_mut() else {
                continue;
            };

            if element.inline_layout_data.is_some() {
                if node.is_anonymous() {
                    anon_nodes.push(node.id);
                } else {
                    node.insert_damage(ALL_DAMAGE);
                }
            } else if let Some(input) = element.text_input_data_mut() {
                input.editor.set_scale(scale);
                let mut font_ctx = font_ctx.lock().unwrap();
                input.editor.refresh_layout(&mut font_ctx, layout_ctx);
                node.insert_damage(ONLY_RELAYOUT);
            }
        }

        for node_id in anon_nodes {
            if let Some(parent_id) = *(self.nodes[node_id].layout_parent.get_mut()) {
                self.nodes[parent_id].insert_damage(ALL_DAMAGE);
            }
        }
    }

    pub fn flush_styles_to_layout(&mut self, node_id: NodeId) {
        self.flush_styles_to_layout_impl(node_id, None, None);
    }

    /// Flush the image layers of nodes whose style changed during the last
    /// style traversal (or whose pseudo-element boxes were (re)constructed).
    pub(crate) fn flush_pending_style_images(&mut self) {
        let mut pending = std::mem::take(&mut self.pending_style_image_nodes);
        pending.sort_unstable();
        pending.dedup();
        for node_id in pending {
            // Anonymous boxes (including pseudo-elements) can be removed from
            // the slab between queueing and flushing; skip stale IDs.
            if !self.nodes.contains_key(node_id) {
                continue;
            }
            self.flush_image_layers_from_style(node_id, ImageLayerKind::Background);
            self.flush_image_layers_from_style(node_id, ImageLayerKind::Mask);
        }
    }

    /// Flush a CSS image layer list (`background-image` or `mask-image`) from style
    /// to dedicated storage on the node, fetching any images which are not yet loaded.
    fn flush_image_layers_from_style(&mut self, node_id: NodeId, kind: ImageLayerKind) {
        let doc_id = self.id();
        let node = self.nodes.get_mut(node_id).unwrap();
        // Clone the primary style `Arc` into an owned value so the immutable
        // borrow of `node` (held by the stylo element data guard) is released
        // before we take a mutable borrow of `node.data` below.
        let style = {
            let stylo_element_data = node.try_stylo_element_data().and_then(|s| s.get());
            let primary_styles = stylo_element_data
                .as_ref()
                .and_then(|data| data.styles.get_primary());
            let Some(style) = primary_styles else {
                return;
            };
            style.clone()
        };
        let Some(elem) = node.data.downcast_element_mut() else {
            return;
        };

        let (style_images, elem_images) = match kind {
            ImageLayerKind::Background => (
                &style.get_background().background_image.0,
                &mut elem.background_images,
            ),
            ImageLayerKind::Mask => (&style.get_svg().mask_image.0, &mut elem.mask_images),
        };

        let len = style_images.len();
        elem_images.resize(len, None);

        for idx in 0..len {
            let style_image = &style_images[idx];
            let new_image = match style_image {
                StyloImage::Url(ComputedUrl::Valid(new_url)) => {
                    let old_image = elem_images[idx].as_ref();
                    let old_image_url = old_image.map(|data| &data.url);
                    if old_image_url.is_some_and(|old_url| **new_url == **old_url) {
                        continue;
                    }

                    // Check cache first
                    let url_str = new_url.as_str();
                    if let Some(cached_image) = self.image_cache.get(url_str) {
                        #[cfg(feature = "tracing")]
                        tracing::info!("Loading image {url_str} from cache");
                        Some(ImageResourceData {
                            url: new_url.clone(),
                            status: Status::Ok,
                            image: cached_image.clone(),
                        })
                    } else if let Some(waiting_list) = self.pending_images.get_mut(url_str) {
                        // Image is already being fetched, queue this node
                        #[cfg(feature = "tracing")]
                        tracing::info!("Image {url_str} already pending, queueing node {node_id}");
                        waiting_list.push((node_id, kind.image_type(idx)));
                        Some(ImageResourceData::new(new_url.clone()))
                    } else {
                        // Start fetch and track as pending
                        #[cfg(feature = "tracing")]
                        tracing::info!("Fetching image {url_str}");
                        self.pending_images
                            .insert(url_str.to_string(), vec![(node_id, kind.image_type(idx))]);

                        self.net_provider.fetch(
                            doc_id,
                            crate::net::stamped_request(
                                (**new_url).clone(),
                                self.abort_signal.as_ref(),
                            ),
                            ResourceHandler::boxed(
                                self.tx.clone(),
                                doc_id,
                                None, // Don't pass node_id, we'll handle via pending_images
                                self.shell_provider.clone(),
                                ImageHandler::new(kind.image_type(idx)),
                            ),
                        );

                        Some(ImageResourceData::new(new_url.clone()))
                    }
                }
                _ => None,
            };

            // Element will always exist due to resize_with above
            elem_images[idx] = new_image;
        }
    }

    /// Walk the whole tree, rebuilding paint children and hoisting z-indexed boxes
    fn flush_styles_to_layout_impl(
        &mut self,
        node_id: NodeId,
        parent_stacking_context: Option<&mut HoistedPaintChildren>,
        parent_auto_hoist: Option<&mut Vec<HoistedPaintChild>>,
    ) {
        let mut new_stacking_context: HoistedPaintChildren = HoistedPaintChildren::new();
        let stacking_context = &mut new_stacking_context;

        // Whether this node collects the `z-index: auto` positioned
        // descendants below its static children (see `Node::auto_hoisted`).
        // A stacking context root, a positioned box, and a box that clips
        // its overflow each start a new collection; anything else passes
        // its parent's collection through.
        let owns_auto_hoist = parent_stacking_context.is_none()
            || parent_auto_hoist.is_none()
            || self.nodes[node_id].primary_styles().is_some_and(|style| {
                style.clone_position() != Position::Static
                    || style.clone_overflow_x() != Overflow::Visible
                    || style.clone_overflow_y() != Overflow::Visible
            });
        let mut own_auto_hoist: Vec<HoistedPaintChild> = Vec::new();
        let (auto_hoist, hoist_start): (&mut Vec<HoistedPaintChild>, usize) = if owns_auto_hoist {
            (&mut own_auto_hoist, 0)
        } else {
            let list = parent_auto_hoist.expect("a static box has its parent's collection");
            let start = list.len();
            (list, start)
        };

        let incremental = self.incremental_layout;
        let display = {
            let node = self.nodes.get_mut(node_id).unwrap();

            let Some(display) = node.display_style() else {
                return;
            };

            node.subtree_has_floats = node
                .primary_styles()
                .is_some_and(|style| style.clone_float() != Float::None);

            // In non-incremental mode we unconditionally clear the Taffy cache.
            // In incremental mode this is handled as part of damage propagation.
            if !incremental {
                node.clear_layout_cache();
                if let Some(inline_layout) = node
                    .data
                    .downcast_element_mut()
                    .and_then(|el| el.inline_layout_data.as_mut())
                {
                    inline_layout.content_widths = None;
                }
            }

            display
        };

        // If the node has children, then take those children and...
        let children = self.nodes[node_id].layout_children.borrow_mut().take();
        if let Some(mut children) = children {
            let is_flex_or_grid =
                matches!(display.inside(), DisplayInside::Flex | DisplayInside::Grid);

            // Recursively call flush_styles_to_layout on each child. A
            // positioned child with `z-index: auto` under a static box is
            // recorded in the collection *before* its own descendants, so
            // that it paints below them. A positioned child with an explicit
            // `z-index: 0` is a stacking context root, but step 8 paints it
            // in the same pass as the `auto` boxes, in tree order, so it is
            // recorded too (its own descendants stay inside it).
            for &child in children.iter() {
                let child_is_root = self.nodes[child].is_stacking_context_root(is_flex_or_grid);
                let hoists_auto = !owns_auto_hoist
                    && self.nodes[child].primary_styles().is_some_and(|style| {
                        style.clone_position() != Position::Static
                            && style.clone_z_index().integer_or(0) == 0
                    });
                if hoists_auto {
                    auto_hoist.push(HoistedPaintChild {
                        node_id: child,
                        z_index: 0,
                        position: taffy::Point::ZERO,
                    });
                }
                self.flush_styles_to_layout_impl(
                    child,
                    match child_is_root {
                        true => None,
                        false => Some(stacking_context),
                    },
                    match child_is_root {
                        true => None,
                        false => Some(&mut *auto_hoist),
                    },
                );
            }

            // Floats anywhere below this box (see `Node::subtree_has_floats`)
            if children
                .iter()
                .any(|child| self.nodes[*child].subtree_has_floats)
            {
                self.nodes[node_id].subtree_has_floats = true;
            }

            // Sort layout_children
            if is_flex_or_grid {
                children.sort_by(|left, right| {
                    let left_node = self.nodes.get(*left).unwrap();
                    let right_node = self.nodes.get(*right).unwrap();
                    left_node.order().cmp(&right_node.order())
                });
            }

            // Reserve space for paint_children
            let mut paint_children = self.nodes[node_id].paint_children.borrow_mut();
            if paint_children.is_none() {
                *paint_children = Some(ThinVec::new());
            }
            let paint_children = paint_children.as_mut().unwrap();
            paint_children.clear();
            paint_children.reserve(children.len());

            // Push children to either paint_children or layout_children depending on
            for &child_id in children.iter() {
                let child = &self.nodes[child_id];

                let Some(style) = child.primary_styles() else {
                    paint_children.push(child_id);
                    continue;
                };

                let position = style.clone_position();
                let z_index = style.clone_z_index().integer_or(0);

                // TODO: more complete hoisting detection
                // z-index applies to static flex/grid items too
                // (css-flexbox-1 §painting, css-grid-1 §z-order).
                if z_index != 0 && (position != Position::Static || is_flex_or_grid) {
                    stacking_context.children.push(HoistedPaintChild {
                        node_id: child_id,
                        z_index,
                        position: taffy::Point::ZERO,
                    })
                } else if !owns_auto_hoist && position != Position::Static {
                    // Recorded in the parent's collection above.
                } else {
                    paint_children.push(child_id);
                }
            }

            // Sort paint_children
            paint_children.sort_by(|left, right| {
                let left_node = self.nodes.get(*left).unwrap();
                let right_node = self.nodes.get(*right).unwrap();
                node_to_paint_order(left_node, is_flex_or_grid)
                    .cmp(&node_to_paint_order(right_node, is_flex_or_grid))
            });

            // Put children back
            *self.nodes[node_id].layout_children.borrow_mut() = Some(children);
        }

        // The collection: keep it here, or hand the entries this subtree
        // added up to the parent, offset by this box's position.
        if owns_auto_hoist {
            let node = &mut self.nodes[node_id];
            node.auto_hoisted = (!own_auto_hoist.is_empty()).then(|| Box::new(own_auto_hoist));
        } else {
            let position = self.nodes[node_id].final_layout().location;
            let scroll_offset = *self.nodes[node_id].scroll_offset();
            for hoisted in auto_hoist[hoist_start..].iter_mut() {
                hoisted.position.x += position.x - scroll_offset.x as f32;
                hoisted.position.y += position.y - scroll_offset.y as f32;
            }
            self.nodes[node_id].auto_hoisted = None;
        }

        if let Some(parent_stacking_context) = parent_stacking_context {
            let position = self.nodes[node_id].final_layout().location;
            let scroll_offset = *self.nodes[node_id].scroll_offset();
            for hoisted in stacking_context.children.iter_mut() {
                hoisted.position.x += position.x - scroll_offset.x as f32;
                hoisted.position.y += position.y - scroll_offset.y as f32;
            }
            parent_stacking_context
                .children
                .extend(stacking_context.children.iter().cloned());
        } else {
            stacking_context.sort();
            stacking_context.compute_content_size(self);
            self.nodes[node_id].stacking_context = Some(Box::new(new_stacking_context));
        }
    }
}

#[inline(always)]
fn position_to_order(pos: Position) -> i32 {
    match pos {
        Position::Static => 0,
        // All positioned descendants with z-index: auto share one paint
        // level (CSS 2.1 Appendix E step 8); the stable sort keeps them in
        // tree order among themselves, above in-flow content and floats.
        Position::Relative | Position::Sticky | Position::Absolute | Position::Fixed => 2,
    }
}
#[inline(always)]
fn float_to_order(pos: Float) -> i32 {
    match pos {
        Float::None => 0,
        _ => 1,
    }
}

/// Paint sort key: (paint level, order-modified position). Positioned
/// (z-index: auto) descendants paint above in-flow content (CSS 2.1
/// Appendix E step 8); within a level the stable sort preserves
/// (order-modified) document order.
#[inline(always)]
fn node_to_paint_order(node: &Node, is_flex_or_grid: bool) -> (i32, i32) {
    let Some(style) = node.primary_styles() else {
        return (0, 0);
    };
    let position = style.clone_position();
    if is_flex_or_grid {
        match position {
            Position::Static => (0, style.clone_order()),
            Position::Relative | Position::Sticky => (2, style.clone_order()),
            // Out-of-flow children are not flex/grid items: `order` does
            // not apply; tree order does.
            Position::Absolute | Position::Fixed => (2, 0),
        }
    } else {
        (
            position_to_order(position) + float_to_order(style.clone_float()),
            0,
        )
    }
}

#![allow(unused_variables)]
#![allow(unused_macros)]

use webrender::api::*;
use resources::AppResources;
use traits::LayoutScreen;
use constraints::{DisplayRect, CssConstraint};
use ui_description::{UiDescription, StyledNode};
use cassowary::{Constraint, Solver, Variable};
use window::{WindowDimensions, UiSolver};
use id_tree::{Arena, NodeId};
use css_parser::*;
use dom::NodeData;
use css::Css;
use std::collections::BTreeMap;
use FastHashMap;
use cache::DomChangeSet;
use std::sync::atomic::{Ordering, AtomicUsize};

const DEBUG_COLOR: ColorU = ColorU { r: 255, g: 0, b: 0, a: 255 };

pub(crate) struct DisplayList<'a, T: LayoutScreen + 'a> {
    pub(crate) ui_descr: &'a UiDescription<T>,
    pub(crate) rectangles: BTreeMap<NodeId, DisplayRectangle<'a>>
}

#[derive(Debug)]
pub(crate) struct DisplayRectangle<'a> {
    /// `Some(id)` if this rectangle has a callback attached to it 
    /// Note: this is not the same as the `NodeId`! 
    /// These two are completely seperate numbers!
    pub tag: Option<u64>,
    /// The original styled node
    pub(crate) styled_node: &'a StyledNode,
    /// The style properties of the node, parsed
    pub(crate) style: RectStyle<'a>,
    /// The layout properties of the node, parsed
    pub(crate) layout: RectLayout,
}

/// It is not very efficient to re-create constraints on every call, the difference
/// in performance can be huge. Without re-creating constraints, solving can take 0.3 ms,
/// with re-creation it can take up to 9 ms. So the goal is to not re-create constraints
/// if their contents haven't changed. 
#[derive(Default)]
pub(crate) struct SolvedLayout<T: LayoutScreen> {
    // List of previously solved constraints
    pub(crate) solved_constraints: FastHashMap<NodeId, NodeData<T>>,
}

impl<T: LayoutScreen> SolvedLayout<T> {
    pub fn empty() -> Self {
        Self {
            solved_constraints: FastHashMap::default(),
        }
    }
}

impl<'a> DisplayRectangle<'a> {
    #[inline]
    pub fn new(tag: Option<u64>, styled_node: &'a StyledNode) -> Self {
        Self {
            tag: tag,
            styled_node: styled_node,
            style: RectStyle::default(),
            layout: RectLayout::default(),
        }
    }
}

impl<'a, T: LayoutScreen + 'a> DisplayList<'a, T> {

    /// NOTE: This function assumes that the UiDescription has an initialized arena
    /// 
    /// This only looks at the user-facing styles of the `UiDescription`, not the actual
    /// layout. The layout is done only in the `into_display_list_builder` step.
    pub fn new_from_ui_description(ui_description: &'a UiDescription<T>) -> Self {

        let arena = &ui_description.ui_descr_arena;

        let mut rect_btree = BTreeMap::new();

        for node in &ui_description.styled_nodes {
            let mut rect = DisplayRectangle::new(arena.borrow()[node.id].data.tag, &node);
            parse_css_style_properties(&mut rect);
            parse_css_layout_properties(&mut rect);
            rect_btree.insert(node.id, rect);
        }

        Self {
            ui_descr: ui_description,
            rectangles: rect_btree,
        }
    }

    /// Looks if any new images need to be uploaded and stores the in the image resources
    fn update_resources(api: &RenderApi, app_resources: &mut AppResources, resource_updates: &mut ResourceUpdates) {
        Self::update_image_resources(api, app_resources, resource_updates);
        Self::update_font_resources(api, app_resources, resource_updates);
    }

    fn update_image_resources(api: &RenderApi, app_resources: &mut AppResources, resource_updates: &mut ResourceUpdates) {
        use images::{ImageState, ImageInfo};

        let mut updated_images = Vec::<(String, (ImageData, ImageDescriptor))>::new();
        let mut to_delete_images = Vec::<(String, Option<ImageKey>)>::new();

        // possible performance bottleneck (duplicated cloning) !!
        for (key, value) in app_resources.images.iter() {
            match *value {
                ImageState::ReadyForUpload(ref d) => {
                    updated_images.push((key.clone(), d.clone()));
                },
                ImageState::Uploaded(_) => { },
                ImageState::AboutToBeDeleted(ref k) => {
                    to_delete_images.push((key.clone(), k.clone()));
                }
            }
        }

        // Remove any images that should be deleted
        for (resource_key, image_key) in to_delete_images.into_iter() {
            if let Some(image_key) = image_key {
                resource_updates.delete_image(image_key);
            }
            app_resources.images.remove(&resource_key);
        }

        // Upload all remaining images to the GPU only if the haven't been
        // uploaded yet
        for (resource_key, (data, descriptor)) in updated_images.into_iter() {
            let key = api.generate_image_key();
            resource_updates.add_image(key, descriptor, data, None);
            *app_resources.images.get_mut(&resource_key).unwrap() = 
                ImageState::Uploaded(ImageInfo { 
                    key: key, 
                    descriptor: descriptor 
            });
        }
    }

    // almost the same as update_image_resources, but fonts 
    // have two HashMaps that need to be updated
    fn update_font_resources(api: &RenderApi, app_resources: &mut AppResources, resource_updates: &mut ResourceUpdates) {

        use font::FontState;

        let mut updated_fonts = Vec::<(String, Vec<u8>)>::new();
        let mut to_delete_fonts = Vec::<(String, Option<(FontKey, Vec<FontInstanceKey>)>)>::new();

        for (key, value) in app_resources.font_data.iter() {
            match value.1 {
                FontState::ReadyForUpload(ref bytes) => {
                    updated_fonts.push((key.clone(), bytes.clone()));
                },
                FontState::Uploaded(_) => { },
                FontState::AboutToBeDeleted(ref font_key) => {
                    let to_delete_font_instances = font_key.and_then(|f_key| {
                        let to_delete_font_instances = app_resources.fonts[&f_key].values().cloned().collect();
                        Some((f_key.clone(), to_delete_font_instances))
                    });
                    to_delete_fonts.push((key.clone(), to_delete_font_instances));
                }
            }
        }

        // Delete the complete font. Maybe a more granular option to 
        // keep the font data in memory should be added later
        for (resource_key, to_delete_instances) in to_delete_fonts.into_iter() {
            if let Some((font_key, font_instance_keys)) = to_delete_instances {               
                for instance in font_instance_keys {
                    resource_updates.delete_font_instance(instance);
                }
                resource_updates.delete_font(font_key);
                app_resources.fonts.remove(&font_key);
            }
            app_resources.font_data.remove(&resource_key);
        }

        // Upload all remaining fonts to the GPU only if the haven't been uploaded yet
        for (resource_key, data) in updated_fonts.into_iter() {
            let key = api.generate_font_key();
            println!("adding new font key");
            resource_updates.add_raw_font(key, data, 0); // TODO: use the index better?
            app_resources.font_data.get_mut(&resource_key).unwrap().1 = FontState::Uploaded(key);
        }
    }

    pub fn into_display_list_builder(
        &self, 
        pipeline_id: PipelineId, 
        ui_solver: &mut UiSolver<T>, 
        css: &mut Css,
        app_resources: &mut AppResources,
        render_api: &RenderApi,
        mut has_window_size_changed: bool)
    -> Option<DisplayListBuilder>
    {       
        let mut changeset = None;
        if let Some(root) = self.ui_descr.ui_descr_root {
            let local_changeset = ui_solver.dom_tree_cache.update(root, &*(self.ui_descr.ui_descr_arena.borrow()));
            ui_solver.edit_variable_cache.initialize_new_rectangles(&mut ui_solver.solver, &local_changeset);
            ui_solver.edit_variable_cache.remove_unused_variables(&mut ui_solver.solver);
            changeset = Some(local_changeset);
        }

        if css.needs_relayout {
/* 
            // constraints were added or removed during the last frame
            for rect_id in self.rectangles.keys() {
                let mut layout_contraints = Vec::<CssConstraint>::new();
                let arena = &*self.ui_descr.ui_descr_arena.borrow();
                create_layout_constraints(&rect, arena, ui_solver);
                let cassowary_constraints = css_constraints_to_cassowary_constraints(rect.rect, &layout_contraints);
                ui_solver.solver.add_constraints(&cassowary_constraints).unwrap();
            }
*/
            // if we push or pop constraints that means we also need to re-layout the window
            has_window_size_changed = true;
        }

        let changeset_is_useless = match changeset {
            None => true,
            Some(c) => c.is_empty()
        };
/*
        // early return if we have nothing
        if !css.needs_relayout && changeset_is_useless && !has_window_size_changed {
            return None;
        }
*/

        // recalculate the actual layout
        if css.needs_relayout || has_window_size_changed {
            /*
                for change in solver.fetch_changes() {
                    println!("change: - {:?}", change);
                }
            */
        }

        css.needs_relayout = false;

        let mut builder = DisplayListBuilder::with_capacity(pipeline_id, ui_solver.window_dimensions.layout_size, self.rectangles.len());
        let mut resource_updates = ResourceUpdates::new();
        
        // Upload image and font resources
        Self::update_resources(render_api, app_resources, &mut resource_updates);

        for (rect_idx, rect) in self.rectangles.iter() {

            // ask the solver what the bounds of the current rectangle is
            // let bounds = ui_solver.query_bounds_of_rect(*rect_idx);

            // debugging - there are currently two rectangles on the screen
            // if the rectangle doesn't have a background color, choose the first bound
            //
            // this means, since the DOM in the debug example has two rectangles, we should
            // have two touching rectangles
            let mut bounds = if rect.style.background_color.is_some() { 
                LayoutRect::new(
                    LayoutPoint::new(0.0, 0.0),
                    LayoutSize::new(200.0, 200.0),
                ) 
            } else {
                LayoutRect::new(
                    LayoutPoint::new(0.0, 0.0),
                    LayoutSize::new((*rect_idx).index as f32 * 3.0, 3.0),
                )
            };

            // bug - for some reason, the origin gets scaled by 2.0, 
            // even if the HiDpi factor is set to 1.0
            // this is a workaround, this seems to be a bug in webrender
            bounds.origin.x /= 2.0;
            bounds.origin.y /= 2.0;

            let clip_region_id = rect.style.border_radius.and_then(|border_radius| {
                let region = ComplexClipRegion {
                    rect: bounds,
                    radii: border_radius,
                    mode: ClipMode::Clip,
                };
                Some(builder.define_clip(bounds, vec![region], None))
            });

            let mut info = LayoutPrimitiveInfo::new(bounds);
            info.tag = rect.tag.and_then(|tag| Some((tag, 0)));
            
            builder.push_stacking_context(
                &info,
                ScrollPolicy::Scrollable,
                None,
                TransformStyle::Flat,
                None,
                MixBlendMode::Normal,
                Vec::new(),
            );

            // Push clip
            if let Some(id) = clip_region_id {
                builder.push_clip_id(id);
            }

            // Push box shadow
            if let Some(ref pre_shadow) = rect.style.box_shadow {
                // The pre_shadow is missing the BorderRadius & LayoutRect
                // TODO: do we need to pop the shadows?
                let border_radius = rect.style.border_radius.unwrap_or(BorderRadius::zero());
                builder.push_box_shadow(&info, bounds, pre_shadow.offset, pre_shadow.color,
                                         pre_shadow.blur_radius, pre_shadow.spread_radius,
                                         border_radius, pre_shadow.clip_mode);
            }

            // Push basic rect + background
            builder.push_rect(&info, rect.style.background_color.unwrap_or(DEBUG_COLOR).into());

            // Push background gradient / image
            if let Some(ref background) = rect.style.background {
                match *background {
                    Background::RadialGradient(ref _gradient) => {

                    },
                    Background::LinearGradient(ref gradient) => {
                        let mut stops: Vec<GradientStop> = gradient.stops.iter().map(|gradient_pre|
                            GradientStop {
                                offset: gradient_pre.offset.unwrap(),
                                color: gradient_pre.color,
                            }).collect();
                        let (begin_pt, end_pt) = gradient.direction.to_points(&bounds);
                        let gradient = builder.create_gradient(begin_pt, end_pt, stops, gradient.extend_mode);
                        builder.push_gradient(&info, gradient, bounds.size, LayoutSize::zero());
                    },
                    Background::Image(image_id) => {
                        if let Some(image_info) = app_resources.images.get(image_id.0) {
                            use images::ImageState::*;
                            match *image_info {
                                Uploaded(ref image_info) => {
                                    builder.push_image(
                                            &info,
                                            bounds.size,
                                            LayoutSize::zero(),
                                            ImageRendering::Auto,
                                            AlphaType::Alpha,
                                            image_info.key);
                                },
                                _ => { },
                            }
                        }
                    }
                }
            }

            use font::FontState;
            use euclid::TypedPoint2D;
            const FONT_COLOR: ColorU = ColorU { r: 0, b: 0, g: 255, a: 150 };

            // Push font
            if let Some(ref font_family) = rect.style.font_family {
                let font_id = font_family.fonts.get(0).unwrap_or(&Font::BuiltinFont("sans-serif")).get_font_id();
                let font_size = rect.style.font_size.unwrap_or(DEFAULT_FONT_SIZE);
                let font_size_app_units = Au((font_size.0.to_pixels() as i32) * AU_PER_PX);
                let font_result = push_font(font_id, font_size_app_units, &mut resource_updates, app_resources, render_api);
                
                if let Some(font_instance_key) = font_result {
                    let font = &app_resources.font_data[font_id].0;
                    let glyph = font.glyph('a'); // TODO: get label
                    let glyphs = [GlyphInstance {
                        index: glyph.id().0,
                        point: TypedPoint2D::new(50.0, 50.0),
                    }];
                    builder.push_text(&info, &glyphs, font_instance_key, FONT_COLOR.into(), None);
                }
            }

            // Push border
            if let Some((border_widths, mut border_details)) = rect.style.border {
                if let Some(border_radius) = rect.style.border_radius {
                    if let BorderDetails::Normal(ref mut n) = border_details {
                        n.radius = border_radius;
                    }
                }
                builder.push_border(&info, border_widths, border_details);
            }

            // Pop clip
            if clip_region_id.is_some() {
                builder.pop_clip_id();
            }

            builder.pop_stacking_context();
        }

        render_api.update_resources(resource_updates);

        Some(builder)
    }
}

use app_units::{AU_PER_PX, MIN_AU, MAX_AU, Au};

fn push_font(
    font_id: &str, 
    font_size_app_units: Au, 
    resource_updates: &mut ResourceUpdates, 
    app_resources: &mut AppResources, 
    render_api: &RenderApi) -> Option<FontInstanceKey> {

    use font::FontState;

    if font_size_app_units < MIN_AU || font_size_app_units > MAX_AU {
        println!("warning: too big or too small font size");
        return None;
    } 

    if let Some(&(ref font, ref font_state)) = app_resources.font_data.get(font_id) {
        match *font_state {
            FontState::Uploaded(font_key) => {
                let font_sizes_hashmap = app_resources.fonts.entry(font_key)
                                         .or_insert(FastHashMap::default());
                let font_instance_key = font_sizes_hashmap.entry(font_size_app_units)
                    .or_insert_with(|| {
                        let f_instance_key = render_api.generate_font_instance_key();
                        resource_updates.add_font_instance(
                            f_instance_key,
                            font_key,
                            font_size_app_units,
                            None,
                            None,
                            Vec::new(),
                        );
                        f_instance_key
                    }
                );

                return Some(*font_instance_key);
            },
            _ => {
                println!("warning: trying to use font {:?} that isn't available", font_id);
            },
        }
    }

    return None;
}

macro_rules! parse {
    ($constraint_list:ident, $key:expr, $func:tt) => (
        $constraint_list.get($key).and_then(|w| $func(w).map_err(|e| { 
            #[cfg(debug_assertions)]
            println!("ERROR - invalid {:?}: {:?}", e, $key);
            e 
        }).ok())
    )
}

/// Populate and parse the CSS style properties
fn parse_css_style_properties(rect: &mut DisplayRectangle)
{
    let constraint_list = &rect.styled_node.css_constraints.list;

    rect.style.border_radius    = parse!(constraint_list, "border-radius", parse_css_border_radius);
    rect.style.background_color = parse!(constraint_list, "background-color", parse_css_color);
    rect.style.border           = parse!(constraint_list, "border", parse_css_border);
    rect.style.background       = parse!(constraint_list, "background", parse_css_background);
    rect.style.font_size        = parse!(constraint_list, "font-size", parse_css_font_size);
    rect.style.font_family      = parse!(constraint_list, "font-family", parse_css_font_family);

    let box_shadow_opt          = parse!(constraint_list, "box-shadow", parse_css_box_shadow);
    if let Some(box_shadow_opt) = box_shadow_opt{
        rect.style.box_shadow = box_shadow_opt;
    }
}

/// Populate and parse the CSS layout properties
fn parse_css_layout_properties(rect: &mut DisplayRectangle) {

    let constraint_list = &rect.styled_node.css_constraints.list;
    
    rect.layout.width       = parse!(constraint_list, "width", parse_layout_width);
    rect.layout.height      = parse!(constraint_list, "height", parse_layout_height);
    rect.layout.min_width   = parse!(constraint_list, "min-width", parse_layout_min_width);
    rect.layout.min_height  = parse!(constraint_list, "min-height", parse_layout_min_height);
    
    rect.layout.wrap            = parse!(constraint_list, "flex-wrap", parse_layout_wrap);
    rect.layout.direction       = parse!(constraint_list, "flex-direction", parse_layout_direction);
    rect.layout.justify_content = parse!(constraint_list, "justify-content", parse_layout_justify_content);
    rect.layout.align_items     = parse!(constraint_list, "align-items", parse_layout_align_items);
    rect.layout.align_content   = parse!(constraint_list, "align-content", parse_layout_align_content);
}

// Adds and removes layout constraints if necessary
fn create_layout_constraints<T>(rect: &DisplayRectangle, 
                                arena: &Arena<NodeData<T>>, 
                                ui_solver: &mut UiSolver<T>)
where T: LayoutScreen
{
    use css_parser;
    // todo: put these to use!
    let window_dimensions = &ui_solver.window_dimensions;
    let solver = &mut ui_solver.solver;
    let previous_layout = &mut ui_solver.solved_layout;

    use cassowary::strength::*;
    use constraints::{SizeConstraint, Strength};

    /*
    // centering a rectangle: 
        center(&root),
        bound_by(&root).padding(50.0).strength(WEAK),
    */
}

fn css_constraints_to_cassowary_constraints(rect: &DisplayRect, css: &Vec<CssConstraint>)
-> Vec<Constraint>
{
    use self::CssConstraint::*;

    css.iter().flat_map(|constraint|
        match *constraint {
            Size((constraint, strength)) => { 
                constraint.build(&rect, strength.0) 
            }
            Padding((constraint, strength, padding)) => { 
                constraint.build(&rect, strength.0, padding.0) 
            }
        }
    ).collect()
}
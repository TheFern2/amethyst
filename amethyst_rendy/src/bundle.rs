//! A home of [`RenderingBundle`] with it's rendering plugins system and all types directly related to it.

use std::collections::HashMap;

use amethyst_assets::{register_asset_type, AssetProcessorSystem, AssetStorage};
use amethyst_core::ecs::{DispatcherBuilder, Resources, SystemBundle, World};
use amethyst_error::{format_err, Error};
use rendy::init::Rendy;

use crate::{
    bundle,
    camera::ActiveCamera,
    mtl::{Material, MaterialDefaults},
    rendy::{
        command::QueueId,
        factory::Factory,
        graph::{
            render::{RenderGroupBuilder, RenderPassNodeBuilder, SubpassBuilder},
            GraphBuilder, ImageId, NodeId,
        },
        hal,
        wsi::Surface,
    },
    system::{
        create_default_mat, make_graph_aux_data, render, GraphAuxData, GraphCreator, RenderState,
    },
    types::{Backend, DefaultBackend, Mesh, Texture},
};

/// A bundle of systems used for rendering using `Rendy` render graph.
///
/// Provides a mechanism for registering rendering plugins.
/// By itself doesn't render anything, you must use `with_plugin` method
/// to define a set of functionalities you want to use.
///
/// If you need much more control, or you need to deal directly with the render pipeline,
/// it's possible to define a `RenderGraphCreator` as show by the
/// `renderable_custom` example.
#[derive(Debug)]
pub struct RenderingBundle<B: Backend> {
    plugins: Vec<Box<dyn RenderPlugin<B>>>,
}

impl<B: Backend> RenderingBundle<B> {
    /// Create empty `RenderingBundle`. You must register a plugin using
    /// [`with_plugin`] in order to actually display anything.
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// Register a [`RenderPlugin`].
    ///
    /// If you want the non-consuming version of this method, see [`add_plugin`].
    pub fn with_plugin(mut self, plugin: impl RenderPlugin<B> + 'static) -> Self {
        self.add_plugin(plugin);
        self
    }

    /// Register a [`RenderPlugin`].
    pub fn add_plugin(&mut self, plugin: impl RenderPlugin<B> + 'static) {
        self.plugins.push(Box::new(plugin));
    }
}

register_asset_type!(Material => Material; AssetProcessorSystem<Material>);

impl<B: Backend> SystemBundle for RenderingBundle<B> {
    fn load(
        &mut self,
        world: &mut World,
        resources: &mut Resources,
        builder: &mut DispatcherBuilder,
    ) -> Result<(), Error> {
        resources.insert(ActiveCamera::default());

        for plugin in &mut self.plugins {
            plugin.on_build(world, resources, builder)?;
        }

        let config: rendy::factory::Config = Default::default();
        let r: Rendy<DefaultBackend> = rendy::init::Rendy::init(&config).unwrap();

        let queue_id = QueueId {
            family: r.families.family_by_index(0).id(),
            index: 0,
        };

        resources.insert(r.factory);
        resources.insert(queue_id);

        let mat = create_default_mat::<B>(resources);
        resources.insert(MaterialDefaults(mat));

        resources.insert(RenderState {
            graph: None,
            families: r.families,
            graph_creator: PluggableRenderGraphCreator {
                plugins: self.plugins.drain(..).collect(),
            },
        });

        builder.add_thread_local_fn(render::<B, PluggableRenderGraphCreator<B>>);

        Ok(())
    }

    fn unload(&mut self, world: &mut World, resources: &mut Resources) -> Result<(), Error> {
        let mut state = resources
            .remove::<RenderState<B, PluggableRenderGraphCreator<B>>>()
            .unwrap();

        if let Some(graph) = state.graph.take() {
            let mut factory = resources.get_mut::<Factory<B>>().unwrap();
            log::debug!("Dispose graph");

            let aux = make_graph_aux_data(world, resources);
            graph.dispose(&mut factory, &aux);
        }

        log::debug!("Unload resources");
        if let Some(mut storage) = resources.get_mut::<AssetStorage<Mesh>>() {
            storage.unload_all();
        }
        if let Some(mut storage) = resources.get_mut::<AssetStorage<Texture>>() {
            storage.unload_all();
        }

        log::debug!("Drop families");
        drop(state.families);

        Ok(())
    }
}

struct PluggableRenderGraphCreator<B: Backend> {
    plugins: Vec<Box<dyn RenderPlugin<B>>>,
}

impl<B: Backend> GraphCreator<B> for PluggableRenderGraphCreator<B> {
    fn rebuild(&mut self, world: &World, resources: &Resources) -> bool {
        let mut rebuild = false;
        for plugin in &mut self.plugins {
            rebuild = plugin.should_rebuild(world, resources) || rebuild;
        }
        rebuild
    }

    fn builder(
        &mut self,
        factory: &mut Factory<B>,
        world: &World,
        resources: &Resources,
    ) -> GraphBuilder<B, GraphAuxData> {
        if self.plugins.is_empty() {
            log::warn!("RenderingBundle is configured to display nothing. Use `with_plugin` to add functionality.");
        }

        let mut plan = RenderPlan::new();
        for plugin in &mut self.plugins {
            plugin
                .on_plan(&mut plan, factory, world, resources)
                .unwrap();
        }
        plan.build(factory).unwrap()
    }
}

/// Basic building block of rendering in [`RenderingBundle`].
///
/// Can be used to register rendering-related systems to the dispatcher,
/// building render graph by registering render targets, adding [RenderableAction]s to them
/// and signalling when the graph has to be rebuild.
pub trait RenderPlugin<B: Backend>: std::fmt::Debug {
    /// Hook for adding systems and bundles to the dispatcher.
    fn on_build(
        &mut self,
        _world: &mut World,
        _resources: &mut Resources,
        _builder: &mut DispatcherBuilder,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// Hook for providing triggers to rebuild the render graph.
    fn should_rebuild(&mut self, _world: &World, _resources: &Resources) -> bool {
        false
    }

    /// Hook for extending the rendering plan.
    fn on_plan(
        &mut self,
        plan: &mut RenderPlan<B>,
        factory: &mut Factory<B>,
        world: &World,
        resources: &Resources,
    ) -> Result<(), Error>;
}

/// Builder of a rendering plan for specified target.
#[derive(Debug)]
pub struct RenderPlan<B: Backend> {
    targets: HashMap<Target, TargetPlan<B>>,
    roots: Vec<Target>,
}

impl<B: Backend> RenderPlan<B> {
    fn new() -> Self {
        Self {
            targets: std::collections::HashMap::default(),
            roots: vec![],
        }
    }

    /// Mark render target as root. Root render targets are always
    /// evaluated, even if nothing depends on them.
    pub fn add_root(&mut self, target: Target) {
        if !self.roots.contains(&target) {
            self.roots.push(target);
        }
    }

    /// Define a render target with predefined set of outputs.
    pub fn define_pass(
        &mut self,
        target: Target,
        outputs: TargetPlanOutputs<B>,
    ) -> Result<(), Error> {
        let target_plan = self
            .targets
            .entry(target)
            .or_insert_with(|| TargetPlan::new(target));

        target_plan.set_outputs(outputs)?;

        Ok(())
    }

    /// Extend the rendering plan of a render target. Target can be defined in other plugins.
    /// The closure is evaluated only if the target contributes to the rendering result, e.g.
    /// is rendered to a window or is a dependency of other evaluated target.
    pub fn extend_target(
        &mut self,
        target: Target,
        closure: impl FnOnce(&mut TargetPlanContext<'_, B>) -> Result<(), Error> + 'static,
    ) {
        let target_plan = self
            .targets
            .entry(target)
            .or_insert_with(|| TargetPlan::new(target));
        target_plan.add_extension(Box::new(closure));
    }

    fn build(self, factory: &Factory<B>) -> Result<GraphBuilder<B, GraphAuxData>, Error> {
        let mut ctx = PlanContext {
            target_metadata: self
                .targets
                .iter()
                .filter_map(|(k, t)| unsafe { t.metadata(factory.physical()) }.map(|m| (*k, m)))
                .collect(),
            targets: self.targets,
            passes: std::collections::HashMap::default(),
            outputs: std::collections::HashMap::default(),
            graph_builder: GraphBuilder::new(),
        };

        for target in self.roots {
            ctx.evaluate_target(target)?;
        }

        Ok(ctx.graph_builder)
    }
}

#[derive(Debug)]
enum EvaluationState {
    Evaluating,
    Built(NodeId),
}

impl EvaluationState {
    fn node(&self) -> Option<NodeId> {
        match self {
            EvaluationState::Built(node) => Some(*node),
            EvaluationState::Evaluating => None,
        }
    }

    fn is_built(&self) -> bool {
        self.node().is_some()
    }
}

/// Metadata for a planned render target.
/// Defines effective size and layer count that target's renderpass will operate on.
#[derive(Debug, Clone, Copy)]
pub struct TargetMetadata {
    width: u32,
    height: u32,
    layers: u16,
}

#[derive(Debug)]
struct PlanContext<B: Backend> {
    targets: HashMap<Target, TargetPlan<B>>,
    target_metadata: HashMap<Target, TargetMetadata>,
    passes: HashMap<Target, EvaluationState>,
    outputs: HashMap<TargetImage, ImageId>,
    graph_builder: GraphBuilder<B, GraphAuxData>,
}

impl<B: Backend> PlanContext<B> {
    pub fn mark_evaluating(&mut self, target: Target) -> Result<(), Error> {
        match self.passes.get(&target) {
            None => {},
            Some(EvaluationState::Evaluating) => return Err(format_err!("Trying to evaluate {:?} render plan that is already evaluating. Circular dependency detected.", target)),
            // this case is not a soft runtime error, as this should never be allowed by the API.
            Some(EvaluationState::Built(_)) => panic!("Trying to reevaluate a render plan for {:?}.", target),
        };
        self.passes.insert(target, EvaluationState::Evaluating);
        Ok(())
    }

    fn evaluate_target(&mut self, target: Target) -> Result<(), Error> {
        // prevent evaluation of roots that were accessed recursively or undefined
        if let Some(pass) = self.targets.remove(&target) {
            pass.evaluate(self)?;
        }
        Ok(())
    }

    fn submit_pass(&mut self, target: Target, pass: RenderPassNodeBuilder<B, GraphAuxData>) {
        match self.passes.get(&target) {
            None | Some(EvaluationState::Evaluating) => {}
            // this case is not a soft runtime error, as this should never be allowed by the API.
            Some(EvaluationState::Built(_)) => {
                panic!(
                    "Trying to resubmit a render pass for {:?}. This is a RenderingBundle bug.",
                    target
                );
            }
        };
        let node = self.graph_builder.add_node(pass);
        self.passes.insert(target, EvaluationState::Built(node));
    }

    fn get_pass_node_raw(&self, target: Target) -> Option<NodeId> {
        self.passes
            .get(&target)
            .and_then(bundle::EvaluationState::node)
    }

    pub fn get_node(&mut self, target: Target) -> Result<NodeId, Error> {
        if let Some(node) = self.get_pass_node_raw(target) {
            Ok(node)
        } else {
            self.evaluate_target(target)?;
            Ok(self
                .passes
                .get(&target)
                .and_then(bundle::EvaluationState::node)
                .expect("Just built"))
        }
    }

    pub fn target_metadata(&self, target: Target) -> Option<TargetMetadata> {
        self.target_metadata.get(&target).copied()
    }

    fn get_image(&mut self, image_ref: TargetImage) -> Result<ImageId, Error> {
        self.try_get_image(image_ref)?.ok_or_else(|| {
            format_err!(
                "Output image {:?} is not registered by the target.",
                image_ref
            )
        })
    }

    fn try_get_image(&mut self, image_ref: TargetImage) -> Result<Option<ImageId>, Error> {
        if !self
            .passes
            .get(&image_ref.target())
            .map_or(false, bundle::EvaluationState::is_built)
        {
            self.evaluate_target(image_ref.target())?;
        }
        Ok(self.outputs.get(&image_ref).copied())
    }

    fn register_output(&mut self, output: TargetImage, image: ImageId) -> Result<(), Error> {
        if self.outputs.contains_key(&output) {
            return Err(format_err!(
                "Trying to register already registered output image {:?}",
                output
            ));
        }
        self.outputs.insert(output, image);
        Ok(())
    }

    pub fn graph(&mut self) -> &mut GraphBuilder<B, GraphAuxData> {
        &mut self.graph_builder
    }

    pub fn create_image(&mut self, options: &ImageOptions) -> ImageId {
        self.graph_builder
            .create_image(options.kind, options.levels, options.format, options.clear)
    }
}

/// A planning context focused on specific render target.
#[derive(Debug)]
pub struct TargetPlanContext<'a, B: Backend> {
    plan_context: &'a mut PlanContext<B>,
    key: Target,
    colors: usize,
    depth: bool,
    actions: Vec<(i32, RenderableAction<B>)>,
    deps: Vec<NodeId>,
}

impl<'a, B: Backend> TargetPlanContext<'a, B> {
    /// Add new action to render target in defined order.
    pub fn add(&mut self, order: impl Into<i32>, action: impl IntoAction<B>) -> Result<(), Error> {
        let action = action.into();

        if self.colors != action.colors() {
            return Err(format_err!(
                "Trying to add render action with {} colors to target {:?} that expects {} colors.",
                action.colors(),
                self.key,
                self.colors,
            ));
        }
        if self.depth != action.depth() {
            return Err(format_err!(
                "Trying to add render action with depth '{}' to target {:?} that expects depth '{}'.",
                action.depth(),
                self.key,
                self.depth,
            ));
        }

        self.actions.push((order.into(), action));
        Ok(())
    }

    /// Get number of color outputs of current render target.
    #[must_use]
    pub fn colors(&self) -> usize {
        self.colors
    }

    /// Check if current render target has a depth output.
    #[must_use]
    pub fn depth(&self) -> bool {
        self.depth
    }

    /// Retrieve an image produced by other render target.
    ///
    /// # Errors
    /// Results in an error if such image doesn't exist or
    /// retrieving it would result in a dependency cycle.
    pub fn get_image(&mut self, image: TargetImage) -> Result<ImageId, Error> {
        self.plan_context.get_image(image).map(|i| {
            let node = self
                .plan_context
                .get_pass_node_raw(image.target())
                .expect("Image without target node");
            self.add_dep(node);
            i
        })
    }
    /// Retrieve an image produced by other render target.
    /// Returns `None` when such image isn't registered.
    ///
    /// # Errors
    /// Results in an error if retrieving it would result in a dependency cycle.
    pub fn try_get_image(&mut self, image: TargetImage) -> Result<Option<ImageId>, Error> {
        self.plan_context.try_get_image(image).map(|i| {
            i.map(|i| {
                let node = self
                    .plan_context
                    .get_pass_node_raw(image.target())
                    .expect("Image without target node");
                self.add_dep(node);
                i
            })
        })
    }

    /// Add explicit dependency on another node.
    ///
    /// This is done automatically when you use `get_image`.
    pub fn add_dep(&mut self, node: NodeId) {
        if !self.deps.contains(&node) {
            self.deps.push(node);
        }
    }

    /// Access underlying rendy's `GraphBuilder` directly.
    /// This is useful for adding custom rendering nodes
    /// that are not just standard graphics render passes,
    /// e.g. for compute dispatch.
    pub fn graph(&mut self) -> &mut GraphBuilder<B, GraphAuxData> {
        self.plan_context.graph()
    }

    /// Retrieve render target metadata, e.g. size.
    #[must_use]
    pub fn target_metadata(&self, target: Target) -> Option<TargetMetadata> {
        self.plan_context.target_metadata(target)
    }

    /// Access computed `NodeId` of render target.
    pub fn get_node(&mut self, target: Target) -> Result<NodeId, Error> {
        self.plan_context.get_node(target)
    }
}

/// An identifier for output image of specific render target.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum TargetImage {
    /// Select target color output with given index.
    Color(Target, usize),
    /// Select target depth output.
    Depth(Target),
}

impl TargetImage {
    /// Retrieve target identifier for this image
    #[must_use]
    pub fn target(&self) -> Target {
        match self {
            TargetImage::Color(target, _) | TargetImage::Depth(target) => *target,
        }
    }
}

/// Set of options required to create an image node in render graph.
#[derive(Debug, Clone)]
pub struct ImageOptions {
    /// Image kind and size
    pub kind: hal::image::Kind,
    /// Number of mipmap levels
    pub levels: hal::image::Level,
    /// Image format
    pub format: hal::format::Format,
    /// Clear operation performed once per frame.
    pub clear: Option<hal::command::ClearValue>,
}

/// Definition of render target color output image.
#[derive(Debug)]
pub enum OutputColor<B: Backend> {
    /// Render to image with specified options
    Image(ImageOptions),
    /// Render directly to a window surface.
    Surface(Surface<B>, Option<hal::command::ClearValue>),
}

/// Definition for set of outputs for a given render target.
#[derive(Debug)]
pub struct TargetPlanOutputs<B: Backend> {
    /// List of target color outputs with options
    pub colors: Vec<OutputColor<B>>,
    /// Settings for optional depth output
    pub depth: Option<ImageOptions>,
}

#[derive(derivative::Derivative)]
#[derivative(Debug(bound = ""))]
struct TargetPlan<B: Backend> {
    key: Target,
    #[derivative(Debug = "ignore")]
    extensions: Vec<Box<dyn FnOnce(&mut TargetPlanContext<'_, B>) -> Result<(), Error> + 'static>>,
    outputs: Option<TargetPlanOutputs<B>>,
}

impl<B: Backend> TargetPlan<B> {
    fn new(key: Target) -> Self {
        Self {
            key,
            extensions: vec![],
            outputs: None,
        }
    }

    // safety:
    // * `physical_device` must be created from same `Instance` as the `Surface` present in output
    unsafe fn metadata(&self, physical_device: &B::PhysicalDevice) -> Option<TargetMetadata> {
        self.outputs
            .as_ref()
            .map(|TargetPlanOutputs { colors, depth }| {
                use std::cmp::min;
                let mut framebuffer_width = u32::MAX;
                let mut framebuffer_height = u32::MAX;
                let mut framebuffer_layers = u16::MAX;

                for color in colors {
                    match color {
                        OutputColor::Surface(surface, _) => {
                            if let Some(extent) = surface.extent(physical_device) {
                                framebuffer_width = min(framebuffer_width, extent.width);
                                framebuffer_height = min(framebuffer_height, extent.height);
                                framebuffer_layers = min(framebuffer_layers, 1);
                            } else {
                                // Window was just closed, using size of 1 is the least bad option
                                // to default to. The output won't be used, things won't crash and
                                // graph is either going to be destroyed or rebuilt next frame.
                                framebuffer_width = min(framebuffer_width, 1);
                                framebuffer_height = min(framebuffer_height, 1);
                            }
                            framebuffer_layers = min(framebuffer_layers, 1);
                        }
                        OutputColor::Image(options) => {
                            let extent = options.kind.extent();
                            framebuffer_width = min(framebuffer_width, extent.width);
                            framebuffer_height = min(framebuffer_height, extent.height);
                            framebuffer_layers = min(framebuffer_layers, options.kind.num_layers());
                        }
                    };
                }
                if let Some(options) = depth {
                    let extent = options.kind.extent();
                    framebuffer_width = min(framebuffer_width, extent.width);
                    framebuffer_height = min(framebuffer_height, extent.height);
                    framebuffer_layers = min(framebuffer_layers, options.kind.num_layers());
                }
                TargetMetadata {
                    width: framebuffer_width,
                    height: framebuffer_height,
                    layers: framebuffer_layers,
                }
            })
    }

    fn set_outputs(&mut self, outputs: TargetPlanOutputs<B>) -> Result<(), Error> {
        if self.outputs.is_some() {
            return Err(format_err!("Target {:?} already defined.", self.key));
        }
        self.outputs.replace(outputs);
        Ok(())
    }

    fn add_extension(
        &mut self,
        extension: Box<dyn FnOnce(&mut TargetPlanContext<'_, B>) -> Result<(), Error> + 'static>,
    ) {
        self.extensions.push(extension);
    }

    fn evaluate(self, ctx: &mut PlanContext<B>) -> Result<(), Error> {
        if self.outputs.is_none() {
            return Err(format_err!(
                "Trying to evaluate not fully defined pass {:?}. Missing `define_pass` call.",
                self.key
            ));
        }
        let mut outputs = self.outputs.unwrap();
        let suggested_extent = {
            let metadata = ctx.target_metadata(self.key).unwrap();
            hal::window::Extent2D {
                width: metadata.width,
                height: metadata.height,
            }
        };

        ctx.mark_evaluating(self.key)?;

        let mut target_ctx = TargetPlanContext {
            plan_context: ctx,
            key: self.key,
            actions: vec![],
            colors: outputs.colors.len(),
            depth: outputs.depth.is_some(),
            deps: vec![],
        };

        for extension in self.extensions {
            extension(&mut target_ctx)?;
        }

        let TargetPlanContext {
            mut actions, deps, ..
        } = target_ctx;

        let mut subpass = SubpassBuilder::new();
        let mut pass = RenderPassNodeBuilder::new();

        actions.sort_by_key(|a| a.0);
        for action in actions.drain(..).map(|a| a.1) {
            match action {
                RenderableAction::RenderGroup(group) => {
                    subpass.add_dyn_group(group);
                }
            }
        }

        for (i, color) in outputs.colors.drain(..).enumerate() {
            match color {
                OutputColor::Surface(surface, clear) => {
                    subpass.add_color_surface();
                    pass.add_surface(surface, suggested_extent, clear);
                }
                OutputColor::Image(opts) => {
                    let node = ctx.create_image(&opts);
                    ctx.register_output(TargetImage::Color(self.key, i), node)?;
                    subpass.add_color(node);
                }
            }
        }

        if let Some(opts) = outputs.depth {
            let node = ctx.create_image(&opts);
            ctx.register_output(TargetImage::Depth(self.key), node)?;
            subpass.set_depth_stencil(node);
        }

        for node in deps {
            subpass.add_dependency(node);
        }

        pass.add_subpass(subpass);
        ctx.submit_pass(self.key, pass);
        Ok(())
    }
}

/// An action that represents a single transformation to the
/// render graph, e.g. addition of single render group.
///
/// TODO: more actions needed for e.g. splitting pass into subpasses.
#[derive(Debug)]
pub enum RenderableAction<B: Backend> {
    /// Register single render group for evaluation during target rendering
    RenderGroup(Box<dyn RenderGroupBuilder<B, GraphAuxData>>),
}

impl<B: Backend> RenderableAction<B> {
    fn colors(&self) -> usize {
        match self {
            RenderableAction::RenderGroup(g) => g.colors(),
        }
    }

    fn depth(&self) -> bool {
        match self {
            RenderableAction::RenderGroup(g) => g.depth(),
        }
    }
}

/// Trait for easy conversion of various types into `RenderableAction` shell.
pub trait IntoAction<B: Backend> {
    /// Convert to `RenderableAction`.
    fn into(self) -> RenderableAction<B>;
}

impl<B: Backend, G: RenderGroupBuilder<B, GraphAuxData> + 'static> IntoAction<B> for G {
    fn into(self) -> RenderableAction<B> {
        RenderableAction::RenderGroup(Box::new(self))
    }
}

/// Collection of predefined constants for action ordering in the builtin targets.
/// Two actions with the same order will be applied in their insertion order.
/// The list is provided mostly as a comparison point. If you can't find the exact
/// ordering you need, provide custom `i32` that fits into the right place.
///
/// Modules that provide custom render plugins using their own orders should export
/// similar enum with ordering they have added.
#[derive(Debug)]
#[repr(i32)]
pub enum RenderOrder {
    /// register before all opaques
    BeforeOpaque = 90,
    /// register for rendering opaque objects
    Opaque = 100,
    /// register after rendering opaque objects
    AfterOpaque = 110,
    /// register before rendering transparent objects
    BeforeTransparent = 190,
    /// register for rendering transparent objects
    Transparent = 200,
    /// register after rendering transparent objects
    AfterTransparent = 210,
    /// register as post effect in linear color space
    LinearPostEffects = 300,
    /// register as tonemapping step
    ToneMap = 400,
    /// register as post effect in display color space
    DisplayPostEffects = 500,
    /// register as overlay on final render
    Overlay = 600,
}

impl From<RenderOrder> for i32 {
    fn from(r: RenderOrder) -> Self {
        r as i32
    }
}

/// An identifier for render target used in render plugins.
/// Predefined targets are part of default rendering flow
/// used by builtin amethyst render plugins, but the list
/// can be arbitrarily extended for custom usage in user
/// plugins using custom str identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// Default render target for most operations.
    /// Usually the one that gets presented to the window.
    Main,
    /// Render target for shadow mapping.
    /// Builtin plugins use cascaded shadow maps.
    ShadowMap,
    /// Custom render target identifier.
    Custom(&'static str),
}

impl Default for Target {
    fn default() -> Target {
        Target::Main
    }
}

#[cfg(test)]
mod tests {
    use hal::{
        command::{ClearDepthStencil, ClearValue},
        format::Format,
    };
    use winit::{event_loop::EventLoop, window::WindowBuilder};

    use super::*;
    use crate::{
        rendy::{
            command::QueueId,
            graph::{
                render::{RenderGroup, RenderGroupDesc},
                GraphContext, NodeBuffer, NodeImage,
            },
        },
        types::{Backend, DefaultBackend},
    };

    #[derive(Debug)]
    struct TestGroup1;
    #[derive(Debug)]
    struct TestGroup2;

    impl<B: Backend, T> RenderGroupDesc<B, T> for TestGroup1 {
        fn build(
            self,
            ctx: &GraphContext<B>,
            factory: &mut Factory<B>,
            queue: QueueId,
            aux: &T,
            framebuffer_width: u32,
            framebuffer_height: u32,
            subpass: hal::pass::Subpass<'_, B>,
            buffers: Vec<NodeBuffer>,
            images: Vec<NodeImage>,
        ) -> Result<Box<dyn RenderGroup<B, T>>, hal::pso::CreationError> {
            unimplemented!()
        }
    }
    impl<B: Backend, T> RenderGroupDesc<B, T> for TestGroup2 {
        fn build(
            self,
            ctx: &GraphContext<B>,
            factory: &mut Factory<B>,
            queue: QueueId,
            aux: &T,
            framebuffer_width: u32,
            framebuffer_height: u32,
            subpass: hal::pass::Subpass<'_, B>,
            buffers: Vec<NodeBuffer>,
            images: Vec<NodeImage>,
        ) -> Result<Box<dyn RenderGroup<B, T>>, hal::pso::CreationError> {
            unimplemented!()
        }
    }

    #[test]
    #[ignore] // CI can't run tests requiring actual backend
    fn main_pass_color_image_plan() {
        let config: rendy::factory::Config = Default::default();
        let factory: Factory<DefaultBackend> = rendy::init::Rendy::init(&config).unwrap().factory;
        let mut plan = RenderPlan::<DefaultBackend>::new();

        plan.extend_target(Target::Main, |ctx| {
            ctx.add(RenderOrder::Transparent, TestGroup1.builder())?;
            ctx.add(RenderOrder::Opaque, TestGroup2.builder())?;
            Ok(())
        });

        let kind = crate::Kind::D2(1920, 1080, 1, 1);
        plan.add_root(Target::Main);
        plan.define_pass(
            Target::Main,
            TargetPlanOutputs {
                colors: vec![OutputColor::Image(ImageOptions {
                    kind,
                    levels: 1,
                    format: Format::Rgb8Unorm,
                    clear: None,
                })],
                depth: Some(ImageOptions {
                    kind,
                    levels: 1,
                    format: Format::D32Sfloat,
                    clear: Some(ClearValue {
                        depth_stencil: ClearDepthStencil {
                            depth: 0.0,
                            stencil: 0,
                        },
                    }),
                }),
            },
        )
        .unwrap();

        let planned_graph = plan.build(&factory).unwrap();

        let mut manual_graph = GraphBuilder::<DefaultBackend, World>::new();
        let color = manual_graph.create_image(kind, 1, Format::Rgb8Unorm, None);
        let depth = manual_graph.create_image(
            kind,
            1,
            Format::D32Sfloat,
            Some(ClearValue {
                depth_stencil: ClearDepthStencil {
                    depth: 0.0,
                    stencil: 0,
                },
            }),
        );
        manual_graph.add_node(
            RenderPassNodeBuilder::new().with_subpass(
                SubpassBuilder::new()
                    .with_group(TestGroup2.builder())
                    .with_group(TestGroup1.builder())
                    .with_color(color)
                    .with_depth_stencil(depth),
            ),
        );

        assert_eq!(
            format!("{:?}", planned_graph),
            format!("{:?}", manual_graph)
        );
    }

    #[test]
    #[ignore] // CI can't run tests requiring actual backend
    #[cfg(feature = "window")]
    fn main_pass_surface_plan() {
        let ev_loop = EventLoop::new();
        let mut window_builder = WindowBuilder::new();
        window_builder.window.visible = false;
        let window = window_builder.build(&ev_loop).unwrap();

        let size = window.inner_size();
        let window_kind = crate::Kind::D2(size.width as u32, size.height as u32, 1, 1);

        let config: rendy::factory::Config = Default::default();
        let mut factory: Factory<DefaultBackend> =
            rendy::init::Rendy::init(&config).unwrap().factory;
        let mut plan = RenderPlan::<DefaultBackend>::new();

        let surface1 = factory.create_surface(&window).unwrap();
        let surface2 = factory.create_surface(&window).unwrap();

        plan.extend_target(Target::Main, |ctx| {
            ctx.add(RenderOrder::Opaque, TestGroup2.builder())?;
            Ok(())
        });

        plan.add_root(Target::Main);
        plan.define_pass(
            Target::Main,
            TargetPlanOutputs {
                colors: vec![OutputColor::Surface(surface1, None)],
                depth: Some(ImageOptions {
                    kind: window_kind,
                    levels: 1,
                    format: Format::D32Sfloat,
                    clear: Some(ClearValue {
                        depth_stencil: ClearDepthStencil {
                            depth: 0.0,
                            stencil: 0,
                        },
                    }),
                }),
            },
        )
        .unwrap();

        plan.extend_target(Target::Main, |ctx| {
            ctx.add(RenderOrder::Transparent, TestGroup1.builder())?;
            Ok(())
        });

        let planned_graph = plan.build(&factory).unwrap();

        let mut manual_graph = GraphBuilder::<DefaultBackend, World>::new();
        let depth = manual_graph.create_image(
            window_kind,
            1,
            Format::D32Sfloat,
            Some(ClearValue {
                depth_stencil: ClearDepthStencil {
                    depth: 0.0,
                    stencil: 0,
                },
            }),
        );
        manual_graph.add_node(
            RenderPassNodeBuilder::new()
                .with_subpass(
                    SubpassBuilder::new()
                        .with_group(TestGroup2.builder())
                        .with_group(TestGroup1.builder())
                        .with_color_surface()
                        .with_depth_stencil(depth),
                )
                .with_surface(
                    surface2,
                    hal::window::Extent2D {
                        width: 1,
                        height: 1,
                    },
                    None,
                ),
        );

        assert_eq!(
            format!("{:?}", planned_graph),
            format!("{:?}", manual_graph)
        );
    }
}

use std::{
    cell::{Cell, Ref, RefCell, RefMut},
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::Arc,
};

use collections::{FxHashMap, HashMap};
use futures::{FutureExt, channel::oneshot::Receiver};

use raw_window_handle as rwh;
use wayland_backend::client::ObjectId;
use wayland_client::{
    Proxy,
    protocol::{wl_output, wl_seat, wl_surface},
};
use wayland_client::{WEnum, globals::GlobalListContents};
use wayland_protocols::wp::viewporter::client::wp_viewport;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1;
use wayland_protocols::xdg::shell::client::xdg_popup;
use wayland_protocols::xdg::shell::client::xdg_positioner;
use wayland_protocols::xdg::shell::client::xdg_surface;
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self};
use wayland_protocols::{
    wp::fractional_scale::v1::client::wp_fractional_scale_v1,
    xdg::dialog::v1::client::xdg_dialog_v1::XdgDialogV1,
};
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1;

use crate::linux::wayland::{display::WaylandDisplay, serial::SerialKind};
use crate::linux::{Globals, Output, WaylandClientStatePtr, get_window};
use gpui::{
    AnyWindowHandle, Bounds, Capslock, Decorations, DevicePixels, GpuSpecs, Modifiers, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, PromptLevel, RequestFrameOptions, ResizeEdge, Scene, Size, Tiling,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowControls,
    WindowDecorations, WindowKind, WindowParams,
    layer_shell::{Anchor, LayerShellNotSupportedError},
    popup::PopupOptions,
    px, size,
};
use gpui_wgpu::{CompositorGpuHint, WgpuRenderer, WgpuSurfaceConfig, wgpu};

#[derive(Default)]
pub(crate) struct Callbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(gpui::PlatformInput) -> gpui::DispatchEventResult>>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    hover_status_change: Option<Box<dyn FnMut(bool)>>,
    resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved: Option<Box<dyn FnMut()>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    close: Option<Box<dyn FnOnce()>>,
    appearance_changed: Option<Box<dyn FnMut()>>,
    button_layout_changed: Option<Box<dyn FnMut()>>,
}

#[derive(Debug, Clone, Copy)]
struct RawWindow {
    window: *mut c_void,
    display: *mut c_void,
}

// Safety: The raw pointers in RawWindow point to Wayland surface/display
// which are valid for the window's lifetime. These are used only for
// passing to wgpu which needs Send+Sync for surface creation.
unsafe impl Send for RawWindow {}
unsafe impl Sync for RawWindow {}

impl rwh::HasWindowHandle for RawWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let window = NonNull::new(self.window).unwrap();
        let handle = rwh::WaylandWindowHandle::new(window);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}
impl rwh::HasDisplayHandle for RawWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let display = NonNull::new(self.display).unwrap();
        let handle = rwh::WaylandDisplayHandle::new(display);
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(handle.into()) })
    }
}

#[derive(Debug)]
struct InProgressConfigure {
    size: Option<Size<Pixels>>,
    fullscreen: bool,
    maximized: bool,
    resizing: bool,
    tiling: Tiling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisibilityPhase {
    Visible,
    Draining(u64),
    Unmapping(u64),
    Hidden,
    Configuring(u64),
    Remapping(u64),
}

impl VisibilityPhase {
    fn can_acknowledge_configure(self) -> bool {
        !matches!(self, Self::Unmapping(_) | Self::Hidden)
    }

    fn can_draw(self, configured: bool) -> bool {
        configured
            && matches!(
                self,
                Self::Visible | Self::Configuring(_) | Self::Remapping(_)
            )
    }
}

struct RetainedVisibility {
    sequence: u64,
    desired: bool,
    phase: VisibilityPhase,
    waiter: Option<(
        u64,
        futures::channel::oneshot::Sender<anyhow::Result<(u64, bool)>>,
    )>,
}

impl Default for RetainedVisibility {
    fn default() -> Self {
        Self {
            sequence: 0,
            desired: true,
            phase: VisibilityPhase::Visible,
            waiter: None,
        }
    }
}

impl RetainedVisibility {
    fn request(&mut self, visible: bool) -> Receiver<anyhow::Result<(u64, bool)>> {
        if let Some((_, waiter)) = self.waiter.take() {
            let _ = waiter.send(Err(anyhow::anyhow!("Window visibility request superseded")));
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.desired = visible;
        let (sender, receiver) = futures::channel::oneshot::channel();
        self.waiter = Some((self.sequence, sender));
        receiver
    }

    fn begin_unmap(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1);
        let transition = self.sequence;
        self.phase = VisibilityPhase::Draining(transition);
        transition
    }

    fn complete(&mut self) {
        let visible = self.phase == VisibilityPhase::Visible;
        if !matches!(
            self.phase,
            VisibilityPhase::Visible | VisibilityPhase::Hidden
        ) || visible != self.desired
        {
            return;
        }
        if let Some((generation, waiter)) = self.waiter.take() {
            let _ = waiter.send(Ok((generation, visible)));
        }
    }

    fn processed(&mut self, transition: u64, visible: bool) -> bool {
        let expected = if visible {
            VisibilityPhase::Remapping(transition)
        } else {
            VisibilityPhase::Unmapping(transition)
        };
        if self.phase != expected {
            return false;
        }
        self.phase = if visible {
            VisibilityPhase::Visible
        } else {
            VisibilityPhase::Hidden
        };
        true
    }

    fn fail_restore(&mut self, error: anyhow::Error) -> u64 {
        self.fail(error);
        self.desired = false;
        self.begin_unmap()
    }

    fn fail(&mut self, error: anyhow::Error) {
        if let Some((_, waiter)) = self.waiter.take() {
            let _ = waiter.send(Err(error));
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VisibilitySync {
    pub surface: ObjectId,
    pub transition: u64,
    pub visible: bool,
}

pub struct WaylandWindowState {
    visibility: RetainedVisibility,
    retiring_surface: Option<futures::future::Shared<gpui::Task<()>>>,
    title: String,
    minimum_size: Option<Size<Pixels>>,
    surface_state: WaylandSurfaceState,
    acknowledged_first_configure: bool,
    parent: Option<WaylandWindowStatePtr>,
    /// Child surfaces mapped to whether they block this window's input (dialogs
    /// block, popups don't). Children are closed before this window closes.
    children: FxHashMap<ObjectId, bool>,
    pub surface: wl_surface::WlSurface,
    app_id: Option<String>,
    appearance: WindowAppearance,
    blur: Option<org_kde_kwin_blur::OrgKdeKwinBlur>,
    viewport: Option<wp_viewport::WpViewport>,
    outputs: HashMap<ObjectId, Output>,
    display: Option<(ObjectId, Output)>,
    preferred_output: Option<wl_output::WlOutput>,
    globals: Globals,
    renderer: WgpuRenderer,
    bounds: Bounds<Pixels>,
    fixed_outer_size: Option<Size<Pixels>>,
    fixed_geometry_size: Option<Size<i32>>,
    scale: f32,
    input_handler: Option<PlatformInputHandler>,
    decorations: WindowDecorations,
    background_appearance: WindowBackgroundAppearance,
    fullscreen: bool,
    maximized: bool,
    tiling: Tiling,
    window_bounds: Bounds<Pixels>,
    client: WaylandClientStatePtr,
    handle: AnyWindowHandle,
    active: bool,
    hovered: bool,
    pub(crate) force_render_after_recovery: bool,
    renderer_presented: bool,
    in_progress_configure: Option<InProgressConfigure>,
    resize_throttle: bool,
    in_progress_window_controls: Option<WindowControls>,
    window_controls: WindowControls,
    client_inset: Option<Pixels>,
    accesskit_adapter: Option<accesskit_unix::Adapter>,
}

pub enum WaylandSurfaceState {
    Xdg(WaylandXdgSurfaceState),
    LayerShell(WaylandLayerSurfaceState),
    Popup(WaylandPopupSurfaceState),
}

impl WaylandSurfaceState {
    fn new(
        surface: &wl_surface::WlSurface,
        globals: &Globals,
        params: &WindowParams,
        parent: Option<WaylandWindowStatePtr>,
        popup_grab: Option<(u32, wl_seat::WlSeat)>,
        target_output: Option<wl_output::WlOutput>,
    ) -> anyhow::Result<Self> {
        // For layer_shell windows, create a layer surface instead of an xdg surface
        if let WindowKind::LayerShell(options) = &params.kind {
            let Some(layer_shell) = globals.layer_shell.as_ref() else {
                return Err(LayerShellNotSupportedError.into());
            };

            let layer_surface = layer_shell.get_layer_surface(
                &surface,
                target_output.as_ref(),
                super::layer_shell::wayland_layer(options.layer),
                options.namespace.clone(),
                &globals.qh,
                surface.id(),
            );

            let width = f32::from(params.bounds.size.width);
            let height = f32::from(params.bounds.size.height);
            layer_surface.set_size(width as u32, height as u32);

            layer_surface.set_anchor(super::layer_shell::wayland_anchor(options.anchor));
            layer_surface.set_keyboard_interactivity(
                super::layer_shell::wayland_keyboard_interactivity(options.keyboard_interactivity),
            );

            if let Some(margin) = options.margin {
                layer_surface.set_margin(
                    f32::from(margin.0) as i32,
                    f32::from(margin.1) as i32,
                    f32::from(margin.2) as i32,
                    f32::from(margin.3) as i32,
                )
            }

            if let Some(exclusive_zone) = options.exclusive_zone {
                layer_surface.set_exclusive_zone(f32::from(exclusive_zone) as i32);
            }

            if let Some(exclusive_edge) = options.exclusive_edge {
                Self::apply_exclusive_edge(&layer_surface, options.anchor, exclusive_edge);
            }

            return Ok(WaylandSurfaceState::LayerShell(WaylandLayerSurfaceState {
                layer_surface,
                anchor: options.anchor,
            }));
        }

        if let WindowKind::AnchoredPopup(options) = &params.kind {
            let Some(parent) = parent.as_ref() else {
                return Err(anyhow::anyhow!("popup parent window not found"));
            };

            let positioner = build_popup_positioner(
                globals,
                options,
                params.bounds.size,
                parent.window_geometry(),
            );

            let xdg_surface = globals
                .wm_base
                .get_xdg_surface(&surface, &globals.qh, surface.id());

            // A layer-shell parent takes a null xdg parent and is attached via the layer
            // surface. Every other surface kind has an xdg_surface to parent to directly.
            let xdg_popup = if let Some(parent_layer_surface) = parent.layer_surface() {
                let xdg_popup = xdg_surface.get_popup(None, &positioner, &globals.qh, surface.id());
                parent_layer_surface.get_popup(&xdg_popup);
                xdg_popup
            } else {
                xdg_surface.get_popup(
                    parent.xdg_surface().as_ref(),
                    &positioner,
                    &globals.qh,
                    surface.id(),
                )
            };
            positioner.destroy();

            if let Some((serial, seat)) = popup_grab {
                xdg_popup.grab(&seat, serial);
            }

            // Non-blocking: the parent keeps its input so it can dismiss the popup on
            // clicks in its own window.
            parent.add_child(surface.id(), false);

            return Ok(WaylandSurfaceState::Popup(WaylandPopupSurfaceState {
                xdg_surface,
                xdg_popup,
                options: options.clone(),
                next_reposition_token: Cell::new(0),
            }));
        }

        // All other WindowKinds result in a regular xdg surface
        let xdg_surface = globals
            .wm_base
            .get_xdg_surface(&surface, &globals.qh, surface.id());

        let toplevel = xdg_surface.get_toplevel(&globals.qh, surface.id());
        let xdg_parent = parent.as_ref().and_then(|w| w.toplevel());

        if params.kind == WindowKind::Floating || params.kind == WindowKind::Dialog {
            toplevel.set_parent(xdg_parent.as_ref());
        }

        let dialog = if params.kind == WindowKind::Dialog {
            let dialog = globals.dialog.as_ref().map(|dialog| {
                let xdg_dialog = dialog.get_xdg_dialog(&toplevel, &globals.qh, ());
                xdg_dialog.set_modal();
                xdg_dialog
            });

            if let Some(parent) = parent.as_ref() {
                parent.add_child(surface.id(), true);
            }

            dialog
        } else {
            None
        };

        if let Some(size) = params.window_min_size {
            toplevel.set_min_size(f32::from(size.width) as i32, f32::from(size.height) as i32);
        }

        // Attempt to set up window decorations based on the requested configuration
        let decoration = globals
            .decoration_manager
            .as_ref()
            .map(|decoration_manager| {
                decoration_manager.get_toplevel_decoration(&toplevel, &globals.qh, surface.id())
            });

        Ok(WaylandSurfaceState::Xdg(WaylandXdgSurfaceState {
            xdg_surface,
            toplevel,
            decoration,
            dialog,
        }))
    }
}

pub struct WaylandXdgSurfaceState {
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    dialog: Option<XdgDialogV1>,
}

pub struct WaylandLayerSurfaceState {
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    anchor: Anchor,
}

pub struct WaylandPopupSurfaceState {
    xdg_surface: xdg_surface::XdgSurface,
    xdg_popup: xdg_popup::XdgPopup,
    // Kept so the popup can be re-anchored via `xdg_popup.reposition` when resized.
    options: PopupOptions,
    next_reposition_token: Cell<u32>,
}

fn build_popup_positioner(
    globals: &Globals,
    options: &PopupOptions,
    size: Size<Pixels>,
    parent_geometry: Bounds<Pixels>,
) -> xdg_positioner::XdgPositioner {
    let positioner = globals.wm_base.create_positioner(&globals.qh, ());
    // A zero or negative size is a protocol error.
    positioner.set_size(
        f32::from(size.width).max(1.0) as i32,
        f32::from(size.height).max(1.0) as i32,
    );

    // The protocol wants the anchor rect relative to the parent's window geometry, while
    // `options.anchor_rect` is in gpui window coordinates (surface-local). A rect extending
    // outside the geometry or with a zero size is a protocol error, so translate, then clamp
    // to at least one pixel inside the geometry, pulling the origin inward at the edges.
    let anchor_rect = Bounds {
        origin: options.anchor_rect.origin - parent_geometry.origin,
        size: options.anchor_rect.size,
    };
    let one = Point::new(px(1.0), px(1.0));
    let geometry_bottom_right: Point<Pixels> = parent_geometry.size.into();
    let top_left = anchor_rect
        .origin
        .min(&(geometry_bottom_right - one))
        .max(&Point::default());
    let bottom_right = anchor_rect
        .bottom_right()
        .min(&geometry_bottom_right)
        .max(&(top_left + one));
    let anchor_rect = Bounds::from_corners(top_left, bottom_right);
    positioner.set_anchor_rect(
        f32::from(anchor_rect.origin.x) as i32,
        f32::from(anchor_rect.origin.y) as i32,
        f32::from(anchor_rect.size.width) as i32,
        f32::from(anchor_rect.size.height) as i32,
    );

    positioner.set_anchor(super::popup::wayland_anchor(options.anchor));
    positioner.set_gravity(super::popup::wayland_gravity(options.gravity));
    positioner.set_constraint_adjustment(super::popup::wayland_constraint_adjustment(
        options.constraint_adjustment,
    ));
    positioner.set_offset(
        f32::from(options.offset.x) as i32,
        f32::from(options.offset.y) as i32,
    );
    positioner
}

impl WaylandSurfaceState {
    fn ack_configure(&self, serial: u32) {
        match self {
            WaylandSurfaceState::Xdg(WaylandXdgSurfaceState { xdg_surface, .. }) => {
                xdg_surface.ack_configure(serial);
            }
            WaylandSurfaceState::LayerShell(WaylandLayerSurfaceState { layer_surface, .. }) => {
                layer_surface.ack_configure(serial);
            }
            WaylandSurfaceState::Popup(WaylandPopupSurfaceState { xdg_surface, .. }) => {
                xdg_surface.ack_configure(serial);
            }
        }
    }

    fn decoration(&self) -> Option<&zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1> {
        if let WaylandSurfaceState::Xdg(WaylandXdgSurfaceState { decoration, .. }) = self {
            decoration.as_ref()
        } else {
            None
        }
    }

    fn toplevel(&self) -> Option<&xdg_toplevel::XdgToplevel> {
        if let WaylandSurfaceState::Xdg(WaylandXdgSurfaceState { toplevel, .. }) = self {
            Some(toplevel)
        } else {
            None
        }
    }

    fn xdg_surface(&self) -> Option<&xdg_surface::XdgSurface> {
        match self {
            WaylandSurfaceState::Xdg(WaylandXdgSurfaceState { xdg_surface, .. }) => {
                Some(xdg_surface)
            }
            WaylandSurfaceState::Popup(WaylandPopupSurfaceState { xdg_surface, .. }) => {
                Some(xdg_surface)
            }
            WaylandSurfaceState::LayerShell(_) => None,
        }
    }

    fn layer_surface(&self) -> Option<&zwlr_layer_surface_v1::ZwlrLayerSurfaceV1> {
        if let WaylandSurfaceState::LayerShell(WaylandLayerSurfaceState { layer_surface, .. }) =
            self
        {
            Some(layer_surface)
        } else {
            None
        }
    }

    fn set_geometry(&self, x: i32, y: i32, width: i32, height: i32) {
        match self {
            WaylandSurfaceState::Xdg(WaylandXdgSurfaceState { xdg_surface, .. }) => {
                xdg_surface.set_window_geometry(x, y, width, height);
            }
            WaylandSurfaceState::LayerShell(WaylandLayerSurfaceState { layer_surface, .. }) => {
                // cannot set window position of a layer surface
                layer_surface.set_size(width as u32, height as u32);
            }
            WaylandSurfaceState::Popup(WaylandPopupSurfaceState { xdg_surface, .. }) => {
                xdg_surface.set_window_geometry(x, y, width, height);
            }
        }
    }

    // Re-anchors a mapped popup at a new size via `xdg_popup.reposition`. Repositioning an
    // unmapped popup (before the first configure) is a protocol error.
    fn reposition_popup(
        &self,
        globals: &Globals,
        size: Size<Pixels>,
        parent_geometry: Bounds<Pixels>,
    ) {
        if let WaylandSurfaceState::Popup(WaylandPopupSurfaceState {
            xdg_popup,
            options,
            next_reposition_token,
            ..
        }) = self
            && xdg_popup.version() >= xdg_popup::REQ_REPOSITION_SINCE
        {
            let token = next_reposition_token.get();
            next_reposition_token.set(token.wrapping_add(1));

            let positioner = build_popup_positioner(globals, options, size, parent_geometry);
            xdg_popup.reposition(&positioner, token);
            positioner.destroy();
        }
    }

    fn set_exclusive_zone(&self, zone: i32) -> bool {
        if let WaylandSurfaceState::LayerShell(WaylandLayerSurfaceState { layer_surface, .. }) =
            self
        {
            layer_surface.set_exclusive_zone(zone);
            true
        } else {
            false
        }
    }

    /// An exclusive edge must be a single edge that the surface is anchored to,
    /// otherwise the compositor raises a fatal `invalid_exclusive_edge` protocol
    /// error. An invalid edge is logged and ignored. Returns whether it applied.
    fn apply_exclusive_edge(
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        anchor: Anchor,
        edge: Anchor,
    ) -> bool {
        if edge.bits().count_ones() == 1 && anchor.contains(edge) {
            layer_surface.set_exclusive_edge(super::layer_shell::wayland_anchor(edge));
            true
        } else {
            log::warn!(
                "ignoring exclusive edge {edge:?}: must be a single edge of the surface anchor {anchor:?}"
            );
            false
        }
    }

    fn set_exclusive_edge(&self, edge: Anchor) -> bool {
        if let WaylandSurfaceState::LayerShell(WaylandLayerSurfaceState {
            layer_surface,
            anchor,
            ..
        }) = self
        {
            Self::apply_exclusive_edge(layer_surface, *anchor, edge)
        } else {
            false
        }
    }

    fn destroy(&mut self) {
        match self {
            WaylandSurfaceState::Xdg(WaylandXdgSurfaceState {
                xdg_surface,
                toplevel,
                decoration: _decoration,
                dialog,
            }) => {
                // drop the dialog before toplevel so compositor can explicitly unapply it's effects
                if let Some(dialog) = dialog {
                    dialog.destroy();
                }

                // The role object (toplevel) must always be destroyed before the xdg_surface.
                // See https://wayland.app/protocols/xdg-shell#xdg_surface:request:destroy
                toplevel.destroy();
                xdg_surface.destroy();
            }
            WaylandSurfaceState::LayerShell(WaylandLayerSurfaceState { layer_surface, .. }) => {
                layer_surface.destroy();
            }
            WaylandSurfaceState::Popup(WaylandPopupSurfaceState {
                xdg_surface,
                xdg_popup,
                ..
            }) => {
                // Role object before its xdg_surface, as with the toplevel above.
                xdg_popup.destroy();
                xdg_surface.destroy();
            }
        }
    }
}

#[derive(Clone)]
pub struct WaylandWindowStatePtr {
    state: Rc<RefCell<WaylandWindowState>>,
    callbacks: Rc<RefCell<Callbacks>>,
}

impl WaylandWindowState {
    fn fullscreen_output(&self) -> Option<&wl_output::WlOutput> {
        self.preferred_output.as_ref().filter(|output| {
            let Some(global_name) = output.data::<u32>() else {
                return false;
            };
            output.is_alive()
                && self
                    .globals
                    .registry
                    .data::<GlobalListContents>()
                    .is_some_and(|globals| {
                        globals.with_list(|list| {
                            list.iter().any(|global| {
                                global.name == *global_name && global.interface == "wl_output"
                            })
                        })
                    })
        })
    }

    pub(crate) fn new(
        handle: AnyWindowHandle,
        surface: wl_surface::WlSurface,
        surface_state: WaylandSurfaceState,
        appearance: WindowAppearance,
        viewport: Option<wp_viewport::WpViewport>,
        client: WaylandClientStatePtr,
        globals: Globals,
        gpu_context: gpui_wgpu::GpuContext,
        compositor_gpu: Option<CompositorGpuHint>,
        options: WindowParams,
        parent: Option<WaylandWindowStatePtr>,
        preferred_output: Option<wl_output::WlOutput>,
    ) -> anyhow::Result<Self> {
        let renderer = {
            let raw_window = RawWindow {
                window: surface.id().as_ptr().cast::<c_void>(),
                display: surface
                    .backend()
                    .upgrade()
                    .unwrap()
                    .display_ptr()
                    .cast::<c_void>(),
            };
            let config = WgpuSurfaceConfig {
                size: Size {
                    width: DevicePixels(f32::from(options.bounds.size.width) as i32),
                    height: DevicePixels(f32::from(options.bounds.size.height) as i32),
                },
                transparent: true,
                // Prefer Mailbox to avoid blocking. Falls back to FIFO if Mailbox is unsupported.
                preferred_present_mode: Some(wgpu::PresentMode::Mailbox),
            };
            WgpuRenderer::new(gpu_context, &raw_window, config, compositor_gpu)?
        };

        if let WaylandSurfaceState::Xdg(ref xdg_state) = surface_state {
            if let Some(title) = options
                .titlebar
                .as_ref()
                .and_then(|titlebar| titlebar.title.as_ref())
            {
                xdg_state.toplevel.set_title(title.to_string());
            }

            if let Some(app_id) = options.app_id.as_ref() {
                xdg_state.toplevel.set_app_id(app_id.clone());
            }

            // Set max window size based on the GPU's maximum texture dimension.
            // This prevents the window from being resized larger than what the GPU can render.
            let max_texture_size = renderer.max_texture_size() as i32;
            xdg_state
                .toplevel
                .set_max_size(max_texture_size, max_texture_size);
        }

        let mut state = Self {
            visibility: RetainedVisibility::default(),
            retiring_surface: None,
            title: options
                .titlebar
                .as_ref()
                .and_then(|titlebar| titlebar.title.as_ref())
                .map(ToString::to_string)
                .unwrap_or_default(),
            minimum_size: options.window_min_size,
            surface_state,
            acknowledged_first_configure: false,
            parent,
            children: FxHashMap::default(),
            surface,
            app_id: options.app_id,
            blur: None,
            viewport,
            globals,
            outputs: HashMap::default(),
            display: None,
            preferred_output,
            renderer,
            bounds: options.bounds,
            fixed_outer_size: (!options.is_resizable).then_some(options.bounds.size),
            fixed_geometry_size: None,
            scale: 1.0,
            input_handler: None,
            decorations: WindowDecorations::Client,
            background_appearance: WindowBackgroundAppearance::Opaque,
            fullscreen: false,
            maximized: false,
            tiling: Tiling::default(),
            window_bounds: options.bounds,
            in_progress_configure: None,
            resize_throttle: false,
            client,
            appearance,
            handle,
            active: false,
            hovered: false,
            force_render_after_recovery: false,
            renderer_presented: false,
            in_progress_window_controls: None,
            window_controls: WindowControls::default(),
            client_inset: None,
            accesskit_adapter: None,
        };
        state.update_size_constraints();
        Ok(state)
    }

    fn update_size_constraints(&mut self) {
        let Some(outer_size) = self.fixed_outer_size else {
            return;
        };
        let geometry_size = fixed_window_geometry_size(
            outer_size,
            self.inset(),
            self.tiling,
            self.renderer.max_texture_size(),
            self.scale,
        );
        if self.fixed_geometry_size == Some(geometry_size) {
            return;
        }
        if let Some(toplevel) = self.surface_state.toplevel() {
            toplevel.set_min_size(geometry_size.width, geometry_size.height);
            toplevel.set_max_size(geometry_size.width, geometry_size.height);
            self.fixed_geometry_size = Some(geometry_size);
        }
    }

    pub fn is_transparent(&self) -> bool {
        self.decorations == WindowDecorations::Client
            || self.background_appearance != WindowBackgroundAppearance::Opaque
    }

    fn update_subpixel_layout(&mut self) {
        use wayland_client::protocol::wl_output::Subpixel;
        let is_bgr = self
            .display
            .as_ref()
            .and_then(|(_, output)| output.subpixel)
            .is_some_and(|s| s == Subpixel::HorizontalBgr);
        self.renderer.set_subpixel_layout(is_bgr);
    }

    pub fn primary_output_scale(&mut self) -> i32 {
        let mut scale = 1;
        let mut current_output = self.display.take();
        for (id, output) in self.outputs.iter() {
            if let Some((_, output_data)) = &current_output {
                if output.scale > output_data.scale {
                    current_output = Some((id.clone(), output.clone()));
                }
            } else {
                current_output = Some((id.clone(), output.clone()));
            }
            scale = scale.max(output.scale);
        }
        self.display = current_output;
        scale
    }

    pub fn inset(&self) -> Pixels {
        match self.decorations {
            WindowDecorations::Server => px(0.0),
            WindowDecorations::Client => self.client_inset.unwrap_or(px(0.0)),
        }
    }
}

pub(crate) struct WaylandWindow(pub WaylandWindowStatePtr);
pub enum ImeInput {
    InsertText(String),
    SetMarkedText(String),
    UnmarkText,
    DeleteText,
}

impl Drop for WaylandWindow {
    fn drop(&mut self) {
        let mut state = self.0.state.borrow_mut();
        state.visibility.fail(anyhow::anyhow!(
            "Window closed during visibility transition"
        ));
        state.visibility.phase = VisibilityPhase::Hidden;
        let retirement = state.retiring_surface.take();
        let executor = state.globals.executor.clone();
        drop(state);
        if let Some(retirement) = retirement {
            let window = self.0.clone();
            executor
                .spawn(async move {
                    retirement.await;
                    window.destroy_window();
                })
                .detach();
        } else {
            self.0.destroy_window();
        }
    }
}

impl WaylandWindowStatePtr {
    fn destroy_window(&self) {
        let mut state = self.state.borrow_mut();
        state.visibility.fail(anyhow::anyhow!(
            "Window closed during visibility transition"
        ));
        state.visibility.phase = VisibilityPhase::Hidden;
        let surface_id = state.surface.id();
        if let Some(parent) = state.parent.as_ref() {
            parent.state.borrow_mut().children.remove(&surface_id);
        }

        let client = state.client.clone();

        state.renderer.destroy();

        // Destroy blur first, this has no dependencies.
        if let Some(blur) = &state.blur {
            blur.release();
        }

        // Decorations must be destroyed before the xdg state.
        // See https://wayland.app/protocols/xdg-decoration-unstable-v1#zxdg_toplevel_decoration_v1
        if let Some(decoration) = &state.surface_state.decoration() {
            decoration.destroy();
        }

        // Surface state might contain xdg_toplevel/xdg_surface which can be destroyed now that
        // decorations are gone. layer_surface has no dependencies.
        state.surface_state.destroy();

        // Viewport must be destroyed before the wl_surface.
        // See https://wayland.app/protocols/viewporter#wp_viewport
        if let Some(viewport) = &state.viewport {
            viewport.destroy();
        }

        // The wl_surface itself should always be destroyed last.
        state.surface.destroy();

        let state_ptr = self.clone();
        state
            .globals
            .executor
            .spawn(async move {
                state_ptr.close();
                client.drop_window(&surface_id)
            })
            .detach();
        drop(state);
    }
}

impl WaylandWindow {
    fn borrow(&self) -> Ref<'_, WaylandWindowState> {
        self.0.state.borrow()
    }

    fn borrow_mut(&self) -> RefMut<'_, WaylandWindowState> {
        self.0.state.borrow_mut()
    }

    pub fn new(
        handle: AnyWindowHandle,
        globals: Globals,
        gpu_context: gpui_wgpu::GpuContext,
        compositor_gpu: Option<CompositorGpuHint>,
        client: WaylandClientStatePtr,
        params: WindowParams,
        appearance: WindowAppearance,
        parent: Option<WaylandWindowStatePtr>,
        popup_grab: Option<(u32, wl_seat::WlSeat)>,
        target_output: Option<wl_output::WlOutput>,
    ) -> anyhow::Result<(Self, ObjectId)> {
        let surface = globals.compositor.create_surface(&globals.qh, ());
        let surface_state = WaylandSurfaceState::new(
            &surface,
            &globals,
            &params,
            parent.clone(),
            popup_grab,
            target_output.clone(),
        )?;

        if let Some(fractional_scale_manager) = globals.fractional_scale_manager.as_ref() {
            fractional_scale_manager.get_fractional_scale(&surface, &globals.qh, surface.id());
        }

        let viewport = globals
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(&surface, &globals.qh, ()));

        let this = Self(WaylandWindowStatePtr {
            state: Rc::new(RefCell::new(WaylandWindowState::new(
                handle,
                surface.clone(),
                surface_state,
                appearance,
                viewport,
                client,
                globals,
                gpu_context,
                compositor_gpu,
                params,
                parent,
                target_output,
            )?)),
            callbacks: Rc::new(RefCell::new(Callbacks::default())),
        });

        // Kick things off
        surface.commit();

        Ok((this, surface.id()))
    }
}

impl WaylandWindowStatePtr {
    pub fn handle(&self) -> AnyWindowHandle {
        self.state.borrow().handle
    }

    pub fn surface(&self) -> wl_surface::WlSurface {
        self.state.borrow().surface.clone()
    }

    pub fn toplevel(&self) -> Option<xdg_toplevel::XdgToplevel> {
        self.state.borrow().surface_state.toplevel().cloned()
    }

    /// The `xdg_surface` backing this window, if it has one. Used to anchor child popups.
    pub fn xdg_surface(&self) -> Option<xdg_surface::XdgSurface> {
        self.state.borrow().surface_state.xdg_surface().cloned()
    }

    /// The layer-shell surface backing this window, if it is one. Used to anchor child popups.
    pub fn layer_surface(&self) -> Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1> {
        self.state.borrow().surface_state.layer_surface().cloned()
    }

    /// This window's xdg window geometry in surface-local coordinates. Child popup anchor
    /// rectangles are relative to it, while gpui coordinates are surface-local.
    pub fn window_geometry(&self) -> Bounds<Pixels> {
        let state = self.state.borrow();
        inset_by_tiling(
            state.bounds.map_origin(|_| px(0.0)),
            state.inset(),
            state.tiling,
        )
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.state, &other.state)
    }

    pub fn add_child(&self, child: ObjectId, blocking: bool) {
        let mut state = self.state.borrow_mut();
        state.children.insert(child, blocking);
    }

    pub fn is_blocked(&self) -> bool {
        let state = self.state.borrow();
        state.children.values().any(|&blocking| blocking)
    }

    fn visibility_supported(&self) -> bool {
        let state = self.state.borrow();
        matches!(&state.surface_state, WaylandSurfaceState::Xdg(xdg) if xdg.dialog.is_none())
            && state.children.is_empty()
    }

    fn request_visibility(
        &self,
        visible: bool,
    ) -> anyhow::Result<Receiver<anyhow::Result<(u64, bool)>>> {
        anyhow::ensure!(
            self.visibility_supported(),
            "Close child dialogs and popups before hiding this window"
        );
        let receiver = self.state.borrow_mut().visibility.request(visible);
        self.advance_visibility();
        Ok(receiver)
    }

    fn visibility_sync(state: &WaylandWindowState, transition: u64, visible: bool) {
        let connection = wayland_client::Connection::from_backend(
            state
                .surface
                .backend()
                .upgrade()
                .expect("Live surface connection required"),
        );
        connection.display().sync(
            &state.globals.qh,
            VisibilitySync {
                surface: state.surface.id(),
                transition,
                visible,
            },
        );
    }

    fn unmap_surface(&self, transition: u64) {
        let mut state = self.state.borrow_mut();
        let surface = state.renderer.take_surface_for_visibility();
        let background = state
            .client
            .get_client()
            .borrow()
            .common
            .background_executor
            .clone();
        let foreground = state.globals.executor.clone();
        let window = self.clone();
        let retirement = background
            .spawn(async move {
                drop(surface);
            })
            .shared();
        state.retiring_surface = Some(retirement.clone());
        drop(state);
        foreground
            .spawn(async move {
                retirement.await;
                let mut state = window.state.borrow_mut();
                state.retiring_surface = None;
                if state.visibility.phase != VisibilityPhase::Draining(transition) {
                    return;
                }
                state.surface.attach(None, 0, 0);
                state.surface.commit();
                state.visibility.phase = VisibilityPhase::Unmapping(transition);
                Self::visibility_sync(&state, transition, false);
            })
            .detach();
    }

    fn advance_visibility(&self) {
        let mut state = self.state.borrow_mut();
        state.visibility.complete();
        match (state.visibility.phase, state.visibility.desired) {
            (VisibilityPhase::Visible, false) => {
                let transition = state.visibility.begin_unmap();
                drop(state);
                self.unmap_surface(transition);
            }
            (VisibilityPhase::Hidden, true) => {
                state.visibility.sequence = state.visibility.sequence.wrapping_add(1);
                let transition = state.visibility.sequence;
                state.visibility.phase = VisibilityPhase::Configuring(transition);
                state.acknowledged_first_configure = false;
                state.in_progress_configure = None;
                state.renderer_presented = false;
                state.resize_throttle = false;
                state.fixed_geometry_size = None;
                if let WaylandSurfaceState::Xdg(xdg) = &state.surface_state {
                    xdg.toplevel.set_title(state.title.clone());
                    if let Some(app_id) = &state.app_id {
                        xdg.toplevel.set_app_id(app_id.clone());
                    }
                    let parent = state.parent.as_ref().and_then(|parent| parent.toplevel());
                    xdg.toplevel.set_parent(parent.as_ref());
                    if let Some(minimum) = state.minimum_size {
                        xdg.toplevel.set_min_size(
                            f32::from(minimum.width) as i32,
                            f32::from(minimum.height) as i32,
                        );
                    }
                    let maximum = state.renderer.max_texture_size() as i32;
                    xdg.toplevel.set_max_size(maximum, maximum);
                    if state.maximized {
                        xdg.toplevel.set_maximized();
                    }
                    if state.fullscreen {
                        xdg.toplevel.set_fullscreen(state.fullscreen_output());
                    }
                    if let Some(decoration) = &xdg.decoration {
                        decoration.set_mode(state.decorations.to_xdg());
                    }
                }
                state.update_size_constraints();
                state.surface.commit();
            }
            _ => {}
        }
    }

    pub(crate) fn visibility_processed(&self, sync: &VisibilitySync) {
        let mut state = self.state.borrow_mut();
        if state.surface.id() != sync.surface
            || !state.visibility.processed(sync.transition, sync.visible)
        {
            return;
        }
        if !sync.visible {
            state.acknowledged_first_configure = false;
            state.in_progress_configure = None;
            state.renderer_presented = false;
        }
        drop(state);
        self.advance_visibility();
    }

    pub fn frame(&self) {
        let mut state = self.state.borrow_mut();
        if !state
            .visibility
            .phase
            .can_draw(state.acknowledged_first_configure)
        {
            return;
        }
        state.surface.frame(&state.globals.qh, state.surface.id());
        state.resize_throttle = false;
        let force_render = state.force_render_after_recovery;
        state.force_render_after_recovery = false;
        drop(state);

        let mut cb = self.callbacks.borrow_mut();
        if let Some(fun) = cb.request_frame.as_mut() {
            fun(RequestFrameOptions {
                force_render,
                ..Default::default()
            });
            self.update_ime_enabled();
        }
    }

    fn update_ime_enabled(&self) {
        let mut state = self.state.borrow_mut();
        if !state.active {
            return;
        }
        let client = state.client.clone();
        let ime_enabled = state
            .input_handler
            .as_mut()
            .map(|input_handler| input_handler.query_accepts_text_input())
            .unwrap_or(true);
        drop(state);
        if Some(ime_enabled) == client.ime_enabled() {
            return;
        }

        if ime_enabled {
            client.enable_ime();
        } else {
            client.disable_ime();
        }
    }

    pub fn handle_xdg_surface_event(&self, event: xdg_surface::Event) {
        if let xdg_surface::Event::Configure { serial } = event {
            {
                let state = self.state.borrow();
                // The null-buffer commit invalidates old serials before its sync callback arrives.
                if !state.visibility.phase.can_acknowledge_configure() {
                    return;
                }
                if matches!(state.visibility.phase, VisibilityPhase::Draining(_)) {
                    state.surface_state.ack_configure(serial);
                    return;
                }
            }
            {
                let mut state = self.state.borrow_mut();
                if let Some(window_controls) = state.in_progress_window_controls.take() {
                    state.window_controls = window_controls;

                    drop(state);
                    let mut callbacks = self.callbacks.borrow_mut();
                    if let Some(appearance_changed) = callbacks.appearance_changed.as_mut() {
                        appearance_changed();
                    }
                }
            }
            {
                let mut state = self.state.borrow_mut();

                if let Some(mut configure) = state.in_progress_configure.take() {
                    let got_unmaximized = state.maximized && !configure.maximized;
                    state.fullscreen = configure.fullscreen;
                    state.maximized = configure.maximized;
                    state.tiling = configure.tiling;
                    state.update_size_constraints();
                    // Limit interactive resizes to once per vblank
                    if configure.resizing && state.resize_throttle {
                        state.surface_state.ack_configure(serial);
                        return;
                    } else if configure.resizing {
                        state.resize_throttle = true;
                    }
                    if !configure.fullscreen && !configure.maximized {
                        configure.size = if got_unmaximized {
                            Some(state.window_bounds.size)
                        } else {
                            compute_outer_size(state.inset(), configure.size, state.tiling)
                        };
                        if let Some(size) = configure.size {
                            state.window_bounds = Bounds {
                                origin: Point::default(),
                                size,
                            };
                        }
                    }
                    drop(state);
                    if let Some(size) = configure.size {
                        self.resize(size);
                    }
                }
            }
            let mut state = self.state.borrow_mut();
            state.surface_state.ack_configure(serial);

            let window_geometry = inset_by_tiling(
                state.bounds.map_origin(|_| px(0.0)),
                state.inset(),
                state.tiling,
            )
            .map(|v| f32::from(v) as i32)
            .map_size(|v| if v <= 0 { 1 } else { v });

            state.surface_state.set_geometry(
                window_geometry.origin.x,
                window_geometry.origin.y,
                window_geometry.size.width,
                window_geometry.size.height,
            );

            if matches!(state.visibility.phase, VisibilityPhase::Configuring(_))
                && !state.acknowledged_first_configure
            {
                let raw = RawWindow {
                    window: state.surface.id().as_ptr().cast::<c_void>(),
                    display: state
                        .surface
                        .backend()
                        .upgrade()
                        .unwrap()
                        .display_ptr()
                        .cast::<c_void>(),
                };
                let result = if state.renderer.device_lost() {
                    state.renderer.recover(&raw)
                } else {
                    let gpu = state.client.get_client().borrow().gpu_context.clone();
                    let gpu = gpu.borrow();
                    gpu.as_ref()
                        .ok_or_else(|| anyhow::anyhow!("GPU unavailable during window restore"))
                        .and_then(|gpu| {
                            let config = WgpuSurfaceConfig {
                                size: state.renderer.viewport_size(),
                                transparent: state.is_transparent(),
                                preferred_present_mode: Some(wgpu::PresentMode::Mailbox),
                            };
                            state.renderer.replace_surface(&raw, config, &gpu.instance)
                        })
                };
                if let Err(error) = result {
                    let transition = state.visibility.fail_restore(error);
                    drop(state);
                    self.unmap_surface(transition);
                    return;
                }
                state.force_render_after_recovery = true;
            }
            let request_frame_callback = !state.acknowledged_first_configure;
            if request_frame_callback {
                state.acknowledged_first_configure = true;
                drop(state);
                self.frame();
            }
        }
    }

    pub fn handle_toplevel_decoration_event(&self, event: zxdg_toplevel_decoration_v1::Event) {
        if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
            match mode {
                WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide) => {
                    {
                        let mut state = self.state.borrow_mut();
                        state.decorations = WindowDecorations::Server;
                        state.update_size_constraints();
                    }
                    let callback = self.callbacks.borrow_mut().appearance_changed.take();
                    if let Some(mut fun) = callback {
                        fun();
                        self.callbacks.borrow_mut().appearance_changed = Some(fun);
                    }
                }
                WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ClientSide) => {
                    {
                        let mut state = self.state.borrow_mut();
                        state.decorations = WindowDecorations::Client;
                        state.update_size_constraints();
                    }
                    // Update background to be transparent
                    let callback = self.callbacks.borrow_mut().appearance_changed.take();
                    if let Some(mut fun) = callback {
                        fun();
                        self.callbacks.borrow_mut().appearance_changed = Some(fun);
                    }
                }
                WEnum::Value(_) => {
                    log::warn!("Unknown decoration mode");
                }
                WEnum::Unknown(v) => {
                    log::warn!("Unknown decoration mode: {}", v);
                }
            }
        }
    }

    pub fn handle_fractional_scale_event(&self, event: wp_fractional_scale_v1::Event) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            self.rescale(scale as f32 / 120.0);
        }
    }

    pub fn handle_toplevel_event(&self, event: xdg_toplevel::Event) -> bool {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let size = if width == 0 || height == 0 {
                    None
                } else {
                    Some(size(px(width as f32), px(height as f32)))
                };

                let states = extract_states::<xdg_toplevel::State>(&states);

                let mut tiling = Tiling::default();
                let mut fullscreen = false;
                let mut maximized = false;
                let mut resizing = false;

                for state in states {
                    match state {
                        xdg_toplevel::State::Maximized => {
                            maximized = true;
                        }
                        xdg_toplevel::State::Fullscreen => {
                            fullscreen = true;
                        }
                        xdg_toplevel::State::Resizing => resizing = true,
                        xdg_toplevel::State::TiledTop => {
                            tiling.top = true;
                        }
                        xdg_toplevel::State::TiledLeft => {
                            tiling.left = true;
                        }
                        xdg_toplevel::State::TiledRight => {
                            tiling.right = true;
                        }
                        xdg_toplevel::State::TiledBottom => {
                            tiling.bottom = true;
                        }
                        _ => {
                            // noop
                        }
                    }
                }

                if fullscreen || maximized {
                    tiling = Tiling::tiled();
                }

                let mut state = self.state.borrow_mut();
                state.in_progress_configure = Some(InProgressConfigure {
                    size,
                    fullscreen,
                    maximized,
                    resizing,
                    tiling,
                });

                false
            }
            xdg_toplevel::Event::Close => {
                let mut cb = self.callbacks.borrow_mut();
                if let Some(mut should_close) = cb.should_close.take() {
                    let result = (should_close)();
                    cb.should_close = Some(should_close);
                    if result {
                        drop(cb);
                        self.close();
                    }
                    result
                } else {
                    true
                }
            }
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                let mut window_controls = WindowControls {
                    maximize: false,
                    minimize: false,
                    fullscreen: false,
                    window_menu: false,
                };

                let states = extract_states::<xdg_toplevel::WmCapabilities>(&capabilities);

                for state in states {
                    match state {
                        xdg_toplevel::WmCapabilities::Maximize => {
                            window_controls.maximize = true;
                        }
                        xdg_toplevel::WmCapabilities::Minimize => {
                            window_controls.minimize = true;
                        }
                        xdg_toplevel::WmCapabilities::Fullscreen => {
                            window_controls.fullscreen = true;
                        }
                        xdg_toplevel::WmCapabilities::WindowMenu => {
                            window_controls.window_menu = true;
                        }
                        _ => {}
                    }
                }

                let mut state = self.state.borrow_mut();
                state.in_progress_window_controls = Some(window_controls);
                false
            }
            _ => false,
        }
    }

    pub fn handle_layersurface_event(&self, event: zwlr_layer_surface_v1::Event) -> bool {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                width,
                height,
                serial,
            } => {
                let size = if width == 0 || height == 0 {
                    None
                } else {
                    Some(size(px(width as f32), px(height as f32)))
                };

                let mut state = self.state.borrow_mut();
                state.in_progress_configure = Some(InProgressConfigure {
                    size,
                    fullscreen: false,
                    maximized: false,
                    resizing: false,
                    tiling: Tiling::default(),
                });
                drop(state);

                // just do the same thing we'd do as an xdg_surface
                self.handle_xdg_surface_event(xdg_surface::Event::Configure { serial });

                false
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // unlike xdg, we don't have a choice here: the surface is closing.
                true
            }
            _ => false,
        }
    }

    // Returns `true` if the popup should be closed.
    pub fn handle_popup_event(&self, event: xdg_popup::Event) -> bool {
        match event {
            // Only the size is needed, the position is the compositor's. The following
            // xdg_surface.configure applies the change.
            xdg_popup::Event::Configure { width, height, .. } => {
                let size = if width <= 0 || height <= 0 {
                    None
                } else {
                    Some(size(px(width as f32), px(height as f32)))
                };

                self.state.borrow_mut().in_progress_configure = Some(InProgressConfigure {
                    size,
                    fullscreen: false,
                    maximized: false,
                    resizing: false,
                    tiling: Tiling::default(),
                });

                false
            }
            xdg_popup::Event::PopupDone => true,
            // Precedes the reposition's Configure, which does the work. The token is not needed.
            xdg_popup::Event::Repositioned { .. } => false,
            _ => false,
        }
    }

    #[allow(clippy::mutable_key_type)]
    pub fn handle_surface_event(
        &self,
        event: wl_surface::Event,
        outputs: HashMap<ObjectId, Output>,
    ) {
        let mut state = self.state.borrow_mut();

        match event {
            wl_surface::Event::Enter { output } => {
                let id = output.id();

                let Some(output) = outputs.get(&id) else {
                    return;
                };

                state.outputs.insert(id, output.clone());

                let scale = state.primary_output_scale();
                state.update_subpixel_layout();

                // We use `PreferredBufferScale` instead to set the scale if it's available
                if state.surface.version() < wl_surface::EVT_PREFERRED_BUFFER_SCALE_SINCE {
                    state.surface.set_buffer_scale(scale);
                    drop(state);
                    self.rescale(scale as f32);
                }
            }
            wl_surface::Event::Leave { output } => {
                state.outputs.remove(&output.id());

                let scale = state.primary_output_scale();
                state.update_subpixel_layout();

                // We use `PreferredBufferScale` instead to set the scale if it's available
                if state.surface.version() < wl_surface::EVT_PREFERRED_BUFFER_SCALE_SINCE {
                    state.surface.set_buffer_scale(scale);
                    drop(state);
                    self.rescale(scale as f32);
                }
            }
            wl_surface::Event::PreferredBufferScale { factor } => {
                // We use `WpFractionalScale` instead to set the scale if it's available
                if state.globals.fractional_scale_manager.is_none() {
                    state.surface.set_buffer_scale(factor);
                    drop(state);
                    self.rescale(factor as f32);
                }
            }
            _ => {}
        }
    }

    pub fn handle_ime(&self, ime: ImeInput) {
        if self.is_blocked() {
            return;
        }
        let mut state = self.state.borrow_mut();
        if let Some(mut input_handler) = state.input_handler.take() {
            drop(state);
            match ime {
                ImeInput::InsertText(text) => {
                    input_handler.replace_text_in_range(None, &text);
                }
                ImeInput::SetMarkedText(text) => {
                    input_handler.replace_and_mark_text_in_range(None, &text, None);
                }
                ImeInput::UnmarkText => {
                    input_handler.unmark_text();
                }
                ImeInput::DeleteText => {
                    if let Some(marked) = input_handler.marked_text_range() {
                        input_handler.replace_text_in_range(Some(marked), "");
                    }
                }
            }
            self.state.borrow_mut().input_handler = Some(input_handler);
        }
    }

    pub fn get_ime_area(&self) -> Option<Bounds<Pixels>> {
        let mut state = self.state.borrow_mut();
        let mut bounds: Option<Bounds<Pixels>> = None;
        if let Some(mut input_handler) = state.input_handler.take() {
            drop(state);
            bounds = input_handler.ime_candidate_bounds();
            self.state.borrow_mut().input_handler = Some(input_handler);
        }
        bounds
    }

    pub fn set_size_and_scale(&self, size: Option<Size<Pixels>>, scale: Option<f32>) {
        let (size, scale) = {
            let mut state = self.state.borrow_mut();
            if size.is_none_or(|size| size == state.bounds.size)
                && scale.is_none_or(|scale| scale == state.scale)
            {
                return;
            }
            if let Some(size) = size {
                state.bounds.size = size;
            }
            if let Some(scale) = scale {
                state.scale = scale;
                state.update_size_constraints();
            }
            let device_bounds = state.bounds.to_device_pixels(state.scale);
            state.renderer.update_drawable_size(device_bounds.size);
            (state.bounds.size, state.scale)
        };

        let callback = self.callbacks.borrow_mut().resize.take();
        if let Some(mut fun) = callback {
            fun(size, scale);
            self.callbacks.borrow_mut().resize = Some(fun);
        }

        {
            let state = self.state.borrow();
            if let Some(viewport) = &state.viewport {
                viewport
                    .set_destination(f32::from(size.width) as i32, f32::from(size.height) as i32);
            }
        }
    }

    pub fn resize(&self, size: Size<Pixels>) {
        self.set_size_and_scale(Some(size), None);
    }

    pub fn rescale(&self, scale: f32) {
        self.set_size_and_scale(None, Some(scale));
    }

    pub fn close(&self) {
        let state = self.state.borrow();
        let client = state.client.get_client();
        let children = state.children.keys().cloned().collect::<Vec<_>>();
        drop(state);

        for child in children {
            let mut client_state = client.borrow_mut();
            let window = get_window(&mut client_state, &child);
            drop(client_state);

            if let Some(child) = window {
                child.close();
            }
        }
        let mut callbacks = self.callbacks.borrow_mut();
        if let Some(fun) = callbacks.close.take() {
            fun()
        }
    }

    pub fn handle_input(&self, input: PlatformInput) {
        if self.is_blocked() {
            return;
        }
        let callback = self.callbacks.borrow_mut().input.take();
        if let Some(mut fun) = callback {
            let result = fun(input.clone());
            self.callbacks.borrow_mut().input = Some(fun);
            if !result.propagate {
                return;
            }
        }
        if let PlatformInput::KeyDown(event) = input
            && event.keystroke.modifiers.is_subset_of(&Modifiers::shift())
            && let Some(key_char) = &event.keystroke.key_char
        {
            let mut state = self.state.borrow_mut();
            if let Some(mut input_handler) = state.input_handler.take() {
                drop(state);
                input_handler.replace_text_in_range(None, key_char);
                self.state.borrow_mut().input_handler = Some(input_handler);
            }
        }
    }

    pub fn set_focused(&self, focus: bool) {
        self.state.borrow_mut().active = focus;
        let callback = self.callbacks.borrow_mut().active_status_change.take();
        if let Some(mut fun) = callback {
            fun(focus);
            self.callbacks.borrow_mut().active_status_change = Some(fun);
        }
        if let Some(adapter) = self.state.borrow_mut().accesskit_adapter.as_mut() {
            adapter.update_window_focus_state(focus);
        }
    }

    pub fn set_hovered(&self, focus: bool) {
        let callback = self.callbacks.borrow_mut().hover_status_change.take();
        if let Some(mut fun) = callback {
            fun(focus);
            self.callbacks.borrow_mut().hover_status_change = Some(fun);
        }
    }

    pub fn set_appearance(&mut self, appearance: WindowAppearance) {
        self.state.borrow_mut().appearance = appearance;

        let callback = self.callbacks.borrow_mut().appearance_changed.take();
        if let Some(mut fun) = callback {
            fun();
            self.callbacks.borrow_mut().appearance_changed = Some(fun);
        }
    }

    pub fn set_button_layout(&self) {
        let callback = self.callbacks.borrow_mut().button_layout_changed.take();
        if let Some(mut fun) = callback {
            fun();
            self.callbacks.borrow_mut().button_layout_changed = Some(fun);
        }
    }

    pub fn primary_output_scale(&self) -> i32 {
        self.state.borrow_mut().primary_output_scale()
    }
}

fn extract_states<'a, S: TryFrom<u32> + 'a>(states: &'a [u8]) -> impl Iterator<Item = S> + 'a
where
    <S as TryFrom<u32>>::Error: 'a,
{
    states
        .chunks_exact(4)
        .flat_map(TryInto::<[u8; 4]>::try_into)
        .map(u32::from_ne_bytes)
        .flat_map(S::try_from)
}

impl rwh::HasWindowHandle for WaylandWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let surface = self.0.surface().id().as_ptr() as *mut libc::c_void;
        let c_ptr = NonNull::new(surface).ok_or(rwh::HandleError::Unavailable)?;
        let handle = rwh::WaylandWindowHandle::new(c_ptr);
        let raw_handle = rwh::RawWindowHandle::Wayland(handle);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(raw_handle) })
    }
}

impl rwh::HasDisplayHandle for WaylandWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let display = self
            .0
            .surface()
            .backend()
            .upgrade()
            .ok_or(rwh::HandleError::Unavailable)?
            .display_ptr() as *mut libc::c_void;

        let c_ptr = NonNull::new(display).ok_or(rwh::HandleError::Unavailable)?;
        let handle = rwh::WaylandDisplayHandle::new(c_ptr);
        let raw_handle = rwh::RawDisplayHandle::Wayland(handle);
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(raw_handle) })
    }
}

impl PlatformWindow for WaylandWindow {
    fn wayland_surface(&self) -> Option<wl_surface::WlSurface> {
        Some(self.0.surface())
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        self.borrow().maximized
    }

    fn window_bounds(&self) -> WindowBounds {
        let state = self.borrow();
        if state.fullscreen {
            WindowBounds::Fullscreen(state.window_bounds)
        } else if state.maximized {
            WindowBounds::Maximized(state.window_bounds)
        } else {
            drop(state);
            WindowBounds::Windowed(self.bounds())
        }
    }

    fn inner_window_bounds(&self) -> WindowBounds {
        let state = self.borrow();
        if state.fullscreen {
            WindowBounds::Fullscreen(state.window_bounds)
        } else if state.maximized {
            WindowBounds::Maximized(state.window_bounds)
        } else {
            let inset = state.inset();
            drop(state);
            WindowBounds::Windowed(self.bounds().inset(inset))
        }
    }

    fn content_size(&self) -> Size<Pixels> {
        self.borrow().bounds.size
    }

    fn resize(&mut self, size: Size<Pixels>) {
        let mut state = self.borrow_mut();
        let state_ptr = self.0.clone();

        // A popup's placement is the compositor's, so a resize re-runs the positioner and the
        // configure reply drives the buffer resize. Before the first configure the popup is
        // unmapped and cannot reposition, but the initial positioner already carries the size.
        if matches!(state.surface_state, WaylandSurfaceState::Popup(_)) {
            if state.acknowledged_first_configure {
                let parent_geometry = state
                    .parent
                    .as_ref()
                    .map(|parent| parent.window_geometry())
                    .unwrap_or_default();
                state
                    .surface_state
                    .reposition_popup(&state.globals, size, parent_geometry);
            }
            return;
        }

        if state.fixed_outer_size.is_some() {
            state.fixed_outer_size = Some(size);
            state.update_size_constraints();
        }

        // Keep window geometry consistent with configure handling. On Wayland, window geometry is
        // surface-local: resizing should not attempt to translate the window; the compositor
        // controls placement. We also account for client-side decoration insets and tiling.
        let window_geometry = inset_by_tiling(
            Bounds {
                origin: Point::default(),
                size,
            },
            state.inset(),
            state.tiling,
        )
        .map(|v| f32::from(v) as i32)
        .map_size(|v| if v <= 0 { 1 } else { v });

        state.surface_state.set_geometry(
            window_geometry.origin.x,
            window_geometry.origin.y,
            window_geometry.size.width,
            window_geometry.size.height,
        );

        state
            .globals
            .executor
            .spawn(async move { state_ptr.resize(size) })
            .detach();
    }

    fn scale_factor(&self) -> f32 {
        self.borrow().scale
    }

    fn appearance(&self) -> WindowAppearance {
        self.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        let state = self.borrow();
        state.display.as_ref().map(|(id, display)| {
            Rc::new(WaylandDisplay {
                id: id.clone(),
                name: display.name.clone(),
                bounds: display.bounds.to_pixels(state.scale),
            }) as Rc<dyn PlatformDisplay>
        })
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.borrow()
            .client
            .get_client()
            .borrow()
            .mouse_location
            .unwrap_or_default()
    }

    fn modifiers(&self) -> Modifiers {
        self.borrow().client.get_client().borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.borrow().client.get_client().borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<Receiver<usize>> {
        None
    }

    fn retained_visibility(&self) -> Option<bool> {
        self.0.visibility_supported().then(|| {
            let state = self.borrow();
            state.visibility.phase == VisibilityPhase::Visible && state.acknowledged_first_configure
        })
    }

    fn set_retained_visibility(
        &self,
        visible: bool,
    ) -> anyhow::Result<Receiver<anyhow::Result<(u64, bool)>>> {
        self.0.request_visibility(visible)
    }

    fn activate(&self) {
        // Try to request an activation token. Even though the activation is likely going to be rejected,
        // KWin and Mutter can use the app_id to visually indicate we're requesting attention.
        let state = self.borrow();
        if let (Some(activation), Some(app_id)) = (&state.globals.activation, state.app_id.clone())
        {
            state.client.set_pending_activation(state.surface.id());
            let token = activation.get_activation_token(&state.globals.qh, ());
            // The serial isn't exactly important here, since the activation is probably going to be rejected anyway.
            let serial = state.client.get_serial(SerialKind::MousePress);
            token.set_app_id(app_id);
            token.set_serial(serial, &state.globals.seat);
            token.set_surface(&state.surface);
            token.commit();
        }
    }

    fn request_attention(&self) {}

    fn is_active(&self) -> bool {
        self.borrow().active
    }

    fn is_hovered(&self) -> bool {
        self.borrow().hovered
    }

    fn set_title(&mut self, title: &str) {
        let mut state = self.borrow_mut();
        state.title = title.to_owned();
        if let Some(toplevel) = state.surface_state.toplevel() {
            toplevel.set_title(title.to_string());
        }
    }

    fn set_app_id(&mut self, app_id: &str) {
        let mut state = self.borrow_mut();
        if let Some(toplevel) = state.surface_state.toplevel() {
            toplevel.set_app_id(app_id.to_owned());
        }
        state.app_id = Some(app_id.to_owned());
    }

    fn set_background_appearance(&self, background_appearance: WindowBackgroundAppearance) {
        let mut state = self.borrow_mut();
        state.background_appearance = background_appearance;
        update_window(state);
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.borrow().background_appearance
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        let client = self.borrow().client.get_client();
        let state = client.borrow();
        state
            .gpu_context
            .borrow()
            .as_ref()
            .is_some_and(|ctx| ctx.supports_dual_source_blending())
    }

    fn minimize(&self) {
        if let Some(toplevel) = self.borrow().surface_state.toplevel() {
            toplevel.set_minimized();
        }
    }

    fn zoom(&self) {
        let state = self.borrow();
        if let Some(toplevel) = state.surface_state.toplevel() {
            if !state.maximized {
                toplevel.set_maximized();
            } else {
                toplevel.unset_maximized();
            }
        }
    }

    fn toggle_fullscreen(&self) {
        let state = self.borrow();
        if let Some(toplevel) = state.surface_state.toplevel() {
            if !state.fullscreen {
                toplevel.set_fullscreen(state.fullscreen_output());
            } else {
                toplevel.unset_fullscreen();
            }
        }
    }

    fn is_fullscreen(&self) -> bool {
        self.borrow().fullscreen
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> gpui::DispatchEventResult>) {
        self.0.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn on_button_layout_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().button_layout_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        let mut state = self.borrow_mut();
        if !state
            .visibility
            .phase
            .can_draw(state.acknowledged_first_configure)
        {
            return;
        }

        if state.renderer.device_lost() {
            let raw_window = RawWindow {
                window: state.surface.id().as_ptr().cast::<std::ffi::c_void>(),
                display: state
                    .surface
                    .backend()
                    .upgrade()
                    .unwrap()
                    .display_ptr()
                    .cast::<std::ffi::c_void>(),
            };
            match state.renderer.recover(&raw_window) {
                Ok(()) => {}
                Err(err) => {
                    log::warn!("GPU recovery failed, will retry on next frame: {err}");
                }
            }

            state.force_render_after_recovery = true;
            return;
        }

        state.renderer_presented = state.renderer.draw(scene);
        if state.renderer_presented
            && let VisibilityPhase::Configuring(transition) = state.visibility.phase
        {
            state.visibility.phase = VisibilityPhase::Remapping(transition);
            WaylandWindowStatePtr::visibility_sync(&state, transition, true);
        }

        if state.renderer.needs_redraw() {
            state.force_render_after_recovery = true;
        }
    }

    fn completed_frame(&self) {
        let mut state = self.borrow_mut();
        if !state
            .visibility
            .phase
            .can_draw(state.acknowledged_first_configure)
        {
            return;
        }

        // Work around a bug in old versions of wlroots where committing without a buffer attached
        // can cause invalid synchronization that leads to graphical corruption.
        if !state.renderer_presented {
            state.surface.commit();
        }

        state.renderer_presented = false;
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        let state = self.borrow();
        state.renderer.sprite_atlas().clone()
    }

    fn show_window_menu(&self, position: Point<Pixels>) {
        let state = self.borrow();
        let serial = state.client.get_serial(SerialKind::MousePress);
        if let Some(toplevel) = state.surface_state.toplevel() {
            toplevel.show_window_menu(
                &state.globals.seat,
                serial,
                f32::from(position.x) as i32,
                f32::from(position.y) as i32,
            );
        }
    }

    fn start_window_move(&self) {
        let state = self.borrow();
        let serial = state.client.get_serial(SerialKind::MousePress);
        if let Some(toplevel) = state.surface_state.toplevel() {
            toplevel._move(&state.globals.seat, serial);
        }
    }

    fn start_window_resize(&self, edge: gpui::ResizeEdge) {
        let state = self.borrow();
        if state.fixed_outer_size.is_some() {
            return;
        }
        if let Some(toplevel) = state.surface_state.toplevel() {
            toplevel.resize(
                &state.globals.seat,
                state.client.get_serial(SerialKind::MousePress),
                edge.to_xdg(),
            )
        }
    }

    fn set_exclusive_zone(&self, zone: Pixels) {
        let state = self.borrow();
        if state
            .surface_state
            .set_exclusive_zone(f32::from(zone) as i32)
        {
            // Commit to apply it immediately, otherwise it only takes effect
            // on the next frame.
            state.surface.commit();
        }
    }

    fn set_exclusive_edge(&self, edge: Anchor) {
        let state = self.borrow();
        if state.surface_state.set_exclusive_edge(edge) {
            // Commit to apply it immediately, otherwise it only takes effect
            // on the next frame.
            state.surface.commit();
        }
    }

    fn set_input_region(&self, region: Option<&[Bounds<Pixels>]>) {
        let state = self.borrow();
        match region {
            // No region means the whole surface receives input.
            None => state.surface.set_input_region(None),
            // A region restricts input to its rectangles. An empty region
            // receives no input at all.
            Some(rects) => {
                let wl_region = state
                    .globals
                    .compositor
                    .create_region(&state.globals.qh, ());
                for rect in rects {
                    let rect = rect.map(|pixels| f32::from(pixels) as i32);
                    wl_region.add(
                        rect.origin.x,
                        rect.origin.y,
                        rect.size.width,
                        rect.size.height,
                    );
                }
                state.surface.set_input_region(Some(&wl_region));
                wl_region.destroy();
            }
        }

        // Commit so the new input region applies immediately. Otherwise it
        // waits for the next frame, which could be the very click we want to
        // allow passing through.
        if state
            .visibility
            .phase
            .can_draw(state.acknowledged_first_configure)
        {
            state.surface.commit();
        }
    }

    fn window_decorations(&self) -> Decorations {
        let state = self.borrow();
        match state.decorations {
            WindowDecorations::Server => Decorations::Server,
            WindowDecorations::Client => Decorations::Client {
                tiling: state.tiling,
            },
        }
    }

    fn request_decorations(&self, decorations: WindowDecorations) {
        let mut state = self.borrow_mut();
        match state.surface_state.decoration().as_ref() {
            Some(decoration) => {
                decoration.set_mode(decorations.to_xdg());
                state.decorations = decorations;
                state.update_size_constraints();
                update_window(state);
            }
            None => {
                if matches!(decorations, WindowDecorations::Server) {
                    log::info!(
                        "Server-side decorations requested, but the Wayland server does not support them. Falling back to client-side decorations."
                    );
                }
                state.decorations = WindowDecorations::Client;
                state.update_size_constraints();
                update_window(state);
            }
        }
    }

    fn window_controls(&self) -> WindowControls {
        self.borrow().window_controls
    }

    fn set_client_inset(&self, inset: Pixels) {
        let mut state = self.borrow_mut();
        if Some(inset) != state.client_inset {
            state.client_inset = Some(inset);
            state.update_size_constraints();
            update_window(state);
        }
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        let state = self.borrow();
        state.client.update_ime_position(bounds);
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.borrow().renderer.gpu_specs().into()
    }

    fn play_system_bell(&self) {
        let state = self.borrow();
        let surface = if state.surface_state.toplevel().is_some() {
            Some(&state.surface)
        } else {
            None
        };
        if let Some(bell) = state.globals.system_bell.as_ref() {
            bell.ring(surface);
        }
    }

    fn a11y_init(&self, callbacks: gpui::A11yCallbacks) {
        let activation_handler = TrivialActivationHandler {
            callback: callbacks.activation,
        };
        let action_handler = TrivialActionHandler(callbacks.action);
        let deactivation_handler = TrivialDeactivationHandler {
            callback: callbacks.deactivation,
        };

        let adapter =
            accesskit_unix::Adapter::new(activation_handler, action_handler, deactivation_handler);

        self.borrow_mut().accesskit_adapter = Some(adapter);
    }

    fn a11y_tree_update(&self, tree_update: accesskit::TreeUpdate) {
        let mut state = self.borrow_mut();
        if let Some(adapter) = state.accesskit_adapter.as_mut() {
            adapter.update_if_active(|| tree_update);
        }
    }

    fn a11y_update_window_bounds(&self) {
        // Wayland doesn't expose window position, so this is a no-op
    }
}

struct TrivialActivationHandler {
    callback: Box<dyn Fn() -> Option<accesskit::TreeUpdate> + Send + 'static>,
}

impl accesskit::ActivationHandler for TrivialActivationHandler {
    fn request_initial_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        (self.callback)()
    }
}

struct TrivialActionHandler(Box<dyn Fn(accesskit::ActionRequest) + Send + 'static>);

impl accesskit::ActionHandler for TrivialActionHandler {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        (self.0)(request);
    }
}

struct TrivialDeactivationHandler {
    callback: Box<dyn Fn() + Send + 'static>,
}

impl accesskit::DeactivationHandler for TrivialDeactivationHandler {
    fn deactivate_accessibility(&mut self) {
        (self.callback)();
    }
}

fn update_window(mut state: RefMut<WaylandWindowState>) {
    let opaque = !state.is_transparent();

    state.renderer.update_transparency(!opaque);
    let opaque_area = state.window_bounds.map(|v| f32::from(v) as i32);
    opaque_area.inset(f32::from(state.inset()) as i32);

    let region = state
        .globals
        .compositor
        .create_region(&state.globals.qh, ());
    region.add(
        opaque_area.origin.x,
        opaque_area.origin.y,
        opaque_area.size.width,
        opaque_area.size.height,
    );

    // Note that rounded corners make this rectangle API hard to work with.
    // As this is common when using CSD, let's just disable this API.
    if state.background_appearance == WindowBackgroundAppearance::Opaque
        && state.decorations == WindowDecorations::Server
    {
        // Promise the compositor that this region of the window surface
        // contains no transparent pixels. This allows the compositor to skip
        // updating whatever is behind the surface for better performance.
        state.surface.set_opaque_region(Some(&region));
    } else {
        state.surface.set_opaque_region(None);
    }

    if let Some(ref blur_manager) = state.globals.blur_manager {
        if state.background_appearance == WindowBackgroundAppearance::Blurred {
            if state.blur.is_none() {
                let blur = blur_manager.create(&state.surface, &state.globals.qh, ());
                state.blur = Some(blur);
            }
            state.blur.as_ref().unwrap().commit();
        } else {
            // It probably doesn't hurt to clear the blur for opaque windows
            blur_manager.unset(&state.surface);
            if let Some(b) = state.blur.take() {
                b.release()
            }
        }
    }

    region.destroy();
}

pub(crate) trait WindowDecorationsExt {
    fn to_xdg(self) -> zxdg_toplevel_decoration_v1::Mode;
}

impl WindowDecorationsExt for WindowDecorations {
    fn to_xdg(self) -> zxdg_toplevel_decoration_v1::Mode {
        match self {
            WindowDecorations::Client => zxdg_toplevel_decoration_v1::Mode::ClientSide,
            WindowDecorations::Server => zxdg_toplevel_decoration_v1::Mode::ServerSide,
        }
    }
}

pub(crate) trait ResizeEdgeWaylandExt {
    fn to_xdg(self) -> xdg_toplevel::ResizeEdge;
}

impl ResizeEdgeWaylandExt for ResizeEdge {
    fn to_xdg(self) -> xdg_toplevel::ResizeEdge {
        match self {
            ResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
            ResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
            ResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
            ResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
            ResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
            ResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
            ResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
            ResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
        }
    }
}

/// The configuration event is in terms of the window geometry, which we are constantly
/// updating to account for the client decorations. But that's not the area we want to render
/// to, due to our intrusize CSD. So, here we calculate the 'actual' size, by adding back in the insets
fn compute_outer_size(
    inset: Pixels,
    new_size: Option<Size<Pixels>>,
    tiling: Tiling,
) -> Option<Size<Pixels>> {
    new_size.map(|mut new_size| {
        if !tiling.top {
            new_size.height += inset;
        }
        if !tiling.bottom {
            new_size.height += inset;
        }
        if !tiling.left {
            new_size.width += inset;
        }
        if !tiling.right {
            new_size.width += inset;
        }

        new_size
    })
}

fn inset_by_tiling(mut bounds: Bounds<Pixels>, inset: Pixels, tiling: Tiling) -> Bounds<Pixels> {
    if !tiling.top {
        bounds.origin.y += inset;
        bounds.size.height -= inset;
    }
    if !tiling.bottom {
        bounds.size.height -= inset;
    }
    if !tiling.left {
        bounds.origin.x += inset;
        bounds.size.width -= inset;
    }
    if !tiling.right {
        bounds.size.width -= inset;
    }

    bounds
}

fn fixed_window_geometry_size(
    outer_size: Size<Pixels>,
    inset: Pixels,
    tiling: Tiling,
    max_texture_size: u32,
    scale: f32,
) -> Size<i32> {
    let limit = (max_texture_size as f32 / scale.max(f32::EPSILON)).max(1.0);
    let outer_size = outer_size.map(|value| px(f32::from(value).clamp(1.0, limit)));
    inset_by_tiling(
        Bounds {
            origin: Point::default(),
            size: outer_size,
        },
        inset,
        tiling,
    )
    .size
    .map(|value| (f32::from(value) as i32).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_size_tracks_programmatic_expansion_in_logical_pixels() {
        for height in [395.0, 440.0, 520.0, 395.0] {
            assert_eq!(
                fixed_window_geometry_size(
                    size(px(330.0), px(height)),
                    px(0.0),
                    Tiling::default(),
                    8192,
                    2.0,
                ),
                size(330, height as i32),
            );
        }
    }

    #[test]
    fn fixed_size_uses_the_same_insets_as_xdg_window_geometry() {
        for tiling in [
            Tiling::default(),
            Tiling::tiled(),
            Tiling {
                top: true,
                left: true,
                ..Tiling::default()
            },
        ] {
            let outer = size(px(330.0), px(395.0));
            let constrained = fixed_window_geometry_size(outer, px(8.0), tiling, 8192, 2.0);
            assert_eq!(
                compute_outer_size(
                    px(8.0),
                    Some(constrained.map(|value| px(value as f32))),
                    tiling,
                ),
                Some(outer),
            );
        }
    }

    #[test]
    fn fixed_size_stays_positive_and_within_the_scaled_texture_limit() {
        assert_eq!(
            fixed_window_geometry_size(
                size(px(10000.0), px(10000.0)),
                px(8.0),
                Tiling::default(),
                8192,
                2.0,
            ),
            size(4080, 4080),
        );
        assert_eq!(
            fixed_window_geometry_size(
                size(px(0.0), px(10.0)),
                px(8.0),
                Tiling::default(),
                8192,
                1.0,
            ),
            size(1, 1),
        );
    }
}

#[cfg(test)]
mod retained_visibility_tests {
    use super::{RetainedVisibility, VisibilityPhase};
    use futures::executor::block_on;

    #[test]
    fn configure_acknowledgements_remain_valid_while_draining() {
        assert!(VisibilityPhase::Visible.can_acknowledge_configure());
        assert!(VisibilityPhase::Draining(1).can_acknowledge_configure());
        assert!(!VisibilityPhase::Draining(1).can_draw(true));
    }

    #[test]
    fn queued_configures_are_discarded_after_the_unmap_commit() {
        for phase in [VisibilityPhase::Unmapping(1), VisibilityPhase::Hidden] {
            assert!(!phase.can_acknowledge_configure());
            assert!(!phase.can_draw(true));
        }
    }

    #[test]
    fn a_fresh_restore_configure_is_required_before_drawing() {
        for phase in [
            VisibilityPhase::Configuring(2),
            VisibilityPhase::Remapping(2),
        ] {
            assert!(phase.can_acknowledge_configure());
            assert!(!phase.can_draw(false));
            assert!(phase.can_draw(true));
        }
    }

    #[test]
    fn normal_unmap_still_begins_draining_before_acknowledgement() {
        let mut visibility = RetainedVisibility::default();
        let mut hidden = visibility.request(false);
        let transition = visibility.begin_unmap();

        assert_eq!(transition, 2);
        assert_eq!(visibility.phase, VisibilityPhase::Draining(transition));
        assert!(visibility.phase.can_acknowledge_configure());
        assert!(!visibility.phase.can_draw(true));
        visibility.complete();
        assert!(hidden.try_recv().unwrap().is_none());
        assert!(!visibility.processed(transition, false));

        visibility.phase = VisibilityPhase::Unmapping(transition);
        assert!(visibility.processed(transition, false));
        visibility.complete();
        assert_eq!(block_on(hidden).unwrap().unwrap(), (1, false));
    }

    #[test]
    fn failed_restore_reports_error_and_unmaps_without_automatic_retry() {
        let mut visibility = RetainedVisibility {
            sequence: 2,
            phase: VisibilityPhase::Configuring(2),
            ..Default::default()
        };
        let shown = visibility.request(true);
        let transition = visibility.fail_restore(anyhow::anyhow!("transient renderer failure"));

        assert_eq!(
            block_on(shown).unwrap().unwrap_err().to_string(),
            "transient renderer failure"
        );
        assert_eq!(visibility.phase, VisibilityPhase::Draining(transition));
        assert!(!visibility.desired);
        assert!(!visibility.phase.can_draw(true));
        assert!(!visibility.processed(transition, false));

        visibility.phase = VisibilityPhase::Unmapping(transition);
        assert!(!visibility.phase.can_acknowledge_configure());
        assert!(visibility.processed(transition, false));
        visibility.complete();
        assert_eq!(visibility.phase, VisibilityPhase::Hidden);
        assert!(!visibility.desired);
        assert!(visibility.waiter.is_none());
    }

    #[test]
    fn show_after_restore_failure_waits_for_unmap_and_fresh_presentation() {
        let mut visibility = RetainedVisibility {
            sequence: 2,
            phase: VisibilityPhase::Configuring(2),
            ..Default::default()
        };
        let failed = visibility.request(true);
        let unmap = visibility.fail_restore(anyhow::anyhow!("transient renderer failure"));
        assert!(block_on(failed).unwrap().is_err());
        let mut shown = visibility.request(true);
        visibility.complete();
        assert!(shown.try_recv().unwrap().is_none());
        assert!(!visibility.processed(unmap, true));

        visibility.phase = VisibilityPhase::Unmapping(unmap);
        assert!(visibility.processed(unmap, false));
        visibility.complete();
        assert_eq!(visibility.phase, VisibilityPhase::Hidden);
        assert!(visibility.desired);
        assert!(shown.try_recv().unwrap().is_none());

        visibility.sequence = visibility.sequence.wrapping_add(1);
        let remap = visibility.sequence;
        visibility.phase = VisibilityPhase::Configuring(remap);
        assert!(!visibility.phase.can_draw(false));
        assert!(!visibility.processed(remap, true));
        visibility.complete();
        assert!(shown.try_recv().unwrap().is_none());

        visibility.phase = VisibilityPhase::Remapping(remap);
        assert!(visibility.processed(remap, true));
        visibility.complete();
        assert_eq!(block_on(shown).unwrap().unwrap(), (5, true));
    }

    #[test]
    fn hide_after_restore_failure_waits_for_the_cleanup_sync() {
        let mut visibility = RetainedVisibility {
            sequence: 2,
            phase: VisibilityPhase::Configuring(2),
            ..Default::default()
        };
        let failed = visibility.request(true);
        let transition = visibility.fail_restore(anyhow::anyhow!("transient renderer failure"));
        assert!(block_on(failed).unwrap().is_err());
        let mut hidden = visibility.request(false);

        visibility.complete();
        assert!(hidden.try_recv().unwrap().is_none());
        assert!(!visibility.processed(transition, false));
        visibility.phase = VisibilityPhase::Unmapping(transition);
        assert!(visibility.processed(transition, false));
        visibility.complete();
        assert_eq!(block_on(hidden).unwrap().unwrap(), (5, false));
    }

    #[test]
    fn repeated_restore_failures_require_new_explicit_requests() {
        let mut visibility = RetainedVisibility {
            phase: VisibilityPhase::Hidden,
            desired: false,
            ..Default::default()
        };

        for _ in 0..3 {
            let shown = visibility.request(true);
            visibility.sequence = visibility.sequence.wrapping_add(1);
            visibility.phase = VisibilityPhase::Configuring(visibility.sequence);
            let transition = visibility.fail_restore(anyhow::anyhow!("renderer still unavailable"));
            assert!(block_on(shown).unwrap().is_err());
            assert!(!visibility.desired);
            assert!(visibility.waiter.is_none());

            visibility.phase = VisibilityPhase::Unmapping(transition);
            assert!(visibility.processed(transition, false));
            visibility.complete();
            assert_eq!(visibility.phase, VisibilityPhase::Hidden);
            assert!(!visibility.desired);
            assert!(visibility.waiter.is_none());
        }
    }

    #[test]
    fn unmap_receipt_waits_for_matching_compositor_transition() {
        let mut visibility = RetainedVisibility::default();
        let mut receipt = visibility.request(false);
        visibility.phase = VisibilityPhase::Unmapping(2);
        visibility.complete();
        assert!(receipt.try_recv().unwrap().is_none());
        assert!(!visibility.processed(1, false));
        assert!(!visibility.processed(2, true));
        assert!(visibility.processed(2, false));
        visibility.complete();
        assert_eq!(block_on(receipt).unwrap().unwrap(), (1, false));
    }

    #[test]
    fn hide_then_restore_keeps_physical_unmap_before_remap() {
        let mut visibility = RetainedVisibility::default();
        let hidden = visibility.request(false);
        visibility.phase = VisibilityPhase::Unmapping(2);
        let mut shown = visibility.request(true);
        assert!(block_on(hidden).unwrap().is_err());
        assert!(visibility.processed(2, false));
        visibility.complete();
        assert!(shown.try_recv().unwrap().is_none());
        visibility.phase = VisibilityPhase::Remapping(4);
        assert!(visibility.processed(4, true));
        visibility.complete();
        assert_eq!(block_on(shown).unwrap().unwrap(), (2, true));
    }

    #[test]
    fn configure_without_first_present_cannot_acknowledge_restore() {
        let mut visibility = RetainedVisibility {
            phase: VisibilityPhase::Hidden,
            desired: false,
            ..Default::default()
        };
        let mut shown = visibility.request(true);
        visibility.phase = VisibilityPhase::Configuring(2);
        assert!(!visibility.processed(2, true));
        visibility.complete();
        assert!(shown.try_recv().unwrap().is_none());
        visibility.phase = VisibilityPhase::Remapping(2);
        assert!(visibility.processed(2, true));
        visibility.complete();
        assert!(block_on(shown).unwrap().unwrap().1);
    }

    #[test]
    fn queued_draw_and_completed_frame_are_blocked_through_unmap() {
        for phase in [
            VisibilityPhase::Draining(1),
            VisibilityPhase::Unmapping(1),
            VisibilityPhase::Hidden,
        ] {
            assert!(!phase.can_draw(true));
            assert!(!phase.can_draw(false));
        }
        assert!(!VisibilityPhase::Configuring(1).can_draw(false));
        assert!(VisibilityPhase::Configuring(1).can_draw(true));
    }

    #[test]
    fn close_never_fabricates_a_visibility_acknowledgement() {
        let mut visibility = RetainedVisibility::default();
        let receipt = visibility.request(false);
        visibility.fail(anyhow::anyhow!("window closed"));
        assert!(block_on(receipt).unwrap().is_err());
    }

    #[test]
    fn a_late_unmap_cannot_override_a_new_remap() {
        let mut visibility = RetainedVisibility {
            phase: VisibilityPhase::Remapping(8),
            ..Default::default()
        };
        assert!(!visibility.processed(6, false));
        assert_eq!(visibility.phase, VisibilityPhase::Remapping(8));
    }
}

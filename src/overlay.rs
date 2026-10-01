use std::fmt;

use smithay_client_toolkit::compositor::{CompositorState, Region, SurfaceData};
use smithay_client_toolkit::error::GlobalError;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface, LayerSurfaceData,
};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{Transform, WlOutput};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Dispatch, QueueHandle};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

use crate::geometry::Size;

#[derive(Debug)]
pub enum OverlayError {
    Region(GlobalError),
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OverlayError::Region(e) => write!(f, "cannot create the input region: {e}"),
        }
    }
}

impl std::error::Error for OverlayError {}

const NAMESPACE: &str = "eve-preview";
const MARGIN_TOP: i32 = 8;
const MARGIN_RIGHT: i32 = 8;

/// The thumbnail's layer surface, its viewport and its empty input region.
pub struct Overlay {
    layer: LayerSurface,
    viewport: WpViewport,
    size: Size,
    configured: bool,
}

impl Overlay {
    /// Creates the surface and sends the initial commit with no buffer.
    pub fn new<D>(
        qh: &QueueHandle<D>,
        compositor: &CompositorState,
        layer_shell: &LayerShell,
        viewporter: &WpViewporter,
        output: &WlOutput,
        size: Size,
    ) -> Result<Overlay, OverlayError>
    where
        D: Dispatch<WlSurface, SurfaceData<()>>
            + Dispatch<ZwlrLayerSurfaceV1, LayerSurfaceData>
            + Dispatch<WpViewport, ()>
            + 'static,
    {
        let surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some(NAMESPACE),
            Some(output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
        layer.set_margin(MARGIN_TOP, MARGIN_RIGHT, 0, 0);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(size.width, size.height);
        let region = Region::new(compositor).map_err(OverlayError::Region)?;
        layer
            .wl_surface()
            .set_input_region(Some(region.wl_region()));
        let viewport = viewporter.get_viewport(layer.wl_surface(), qh, ());
        layer.wl_surface().commit();
        Ok(Overlay {
            layer,
            viewport,
            size,
            configured: false,
        })
    }

    pub fn set_configured(&mut self) {
        self.configured = true;
    }

    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// Requests in this order: transform, attach, viewport source and destination, layer size
    /// when it changed, damage, commit. It does not use sctk's `attach`, which adds an offset.
    pub fn present(&mut self, buffer: &WlBuffer, buffer_size: Size, size: Size, y_invert: bool) {
        let surface = self.layer.wl_surface();
        surface.set_buffer_transform(if y_invert {
            Transform::Flipped180
        } else {
            Transform::Normal
        });
        surface.attach(Some(buffer), 0, 0);
        self.viewport.set_source(
            0.0,
            0.0,
            f64::from(buffer_size.width),
            f64::from(buffer_size.height),
        );
        self.viewport
            .set_destination(size.width as i32, size.height as i32);
        if size != self.size {
            self.layer.set_size(size.width, size.height);
            self.size = size;
        }
        surface.damage_buffer(0, 0, buffer_size.width as i32, buffer_size.height as i32);
        surface.commit();
    }

    /// Attaches the on-screen buffer again with full damage. Sends no other state.
    pub fn recommit(&mut self, buffer: &WlBuffer, buffer_size: Size) {
        let surface = self.layer.wl_surface();
        surface.attach(Some(buffer), 0, 0);
        surface.damage_buffer(0, 0, buffer_size.width as i32, buffer_size.height as i32);
        surface.commit();
    }

    /// Destroys the viewport, then the layer surface, then the `wl_surface`.
    pub fn destroy(self) {
        self.viewport.destroy();
        drop(self.layer);
    }
}

use std::fmt;

use smithay_client_toolkit::compositor::{CompositorState, Region, SurfaceData};
use smithay_client_toolkit::error::GlobalError;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface, LayerSurfaceData,
};
use smithay_client_toolkit::shm::slot::{ActivateSlotError, Buffer, CreateBufferError, SlotPool};
use smithay_client_toolkit::shm::{CreatePoolError, Shm};
use smithay_client_toolkit::subcompositor::{SubcompositorState, SubsurfaceData};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{Transform, WlOutput};
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Dispatch, QueueHandle};
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

use crate::config::MAX_OPACITY;
use crate::geometry::{BYTES_PER_PIXEL, Point, Size};

#[derive(Debug)]
pub enum OverlayError {
    Region(GlobalError),
    Pool(CreatePoolError),
    Buffer(CreateBufferError),
    Activate(ActivateSlotError),
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OverlayError::Region(e) => write!(f, "cannot create the input region: {e}"),
            OverlayError::Pool(e) => write!(f, "cannot create the shm pool: {e}"),
            OverlayError::Buffer(e) => write!(f, "cannot create the chrome buffer: {e}"),
            OverlayError::Activate(e) => write!(f, "cannot attach the chrome buffer: {e}"),
        }
    }
}

impl std::error::Error for OverlayError {}

const NAMESPACE: &str = "hypr-eve-preview";
const MIN_POOL_BYTES: usize = 4096;

/// One thumbnail's layer surface and viewport, plus the chrome subsurface with its own
/// viewport and shm pool.
pub struct Overlay {
    layer: LayerSurface,
    viewport: WpViewport,
    chrome_subsurface: WlSubsurface,
    chrome_surface: WlSurface,
    chrome_viewport: WpViewport,
    chrome_buffer: Option<Buffer>,
    alpha: WpAlphaModifierSurfaceV1,
    pool: SlotPool,
    size: Size,
    position: Point,
    configured: bool,
}

/// The bound globals `Overlay::new` reads.
pub struct Globals<'a> {
    pub compositor: &'a CompositorState,
    pub subcompositor: &'a SubcompositorState,
    pub layer_shell: &'a LayerShell,
    pub viewporter: &'a WpViewporter,
    pub shm: &'a Shm,
    pub output: &'a WlOutput,
    pub alpha: &'a WpAlphaModifierV1,
}

impl Overlay {
    /// Creates both surfaces and sends the initial layer commit with no buffer. The layer's
    /// input region is left unset; the chrome's is empty before its first commit. `factor` is the
    /// alpha multiplier from `alpha_factor`, applied to the surface before its first commit.
    pub fn new<D>(
        qh: &QueueHandle<D>,
        globals: &Globals<'_>,
        position: Point,
        size: Size,
        factor: u32,
    ) -> Result<Overlay, OverlayError>
    where
        D: Dispatch<WlSurface, SurfaceData<()>>
            + Dispatch<ZwlrLayerSurfaceV1, LayerSurfaceData>
            + Dispatch<WpViewport, ()>
            + Dispatch<WlSubsurface, SubsurfaceData>
            + Dispatch<WpAlphaModifierSurfaceV1, ()>
            + 'static,
    {
        let surface = globals.compositor.create_surface(qh);
        let layer = globals.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some(NAMESPACE),
            Some(globals.output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_margin(position.y as i32, 0, 0, position.x as i32);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(size.width, size.height);

        let (chrome_subsurface, chrome_surface) = globals
            .subcompositor
            .create_subsurface(layer.wl_surface().clone(), qh);
        chrome_subsurface.set_position(0, 0);
        let region = Region::new(globals.compositor).map_err(OverlayError::Region)?;
        chrome_surface.set_input_region(Some(region.wl_region()));
        let chrome_viewport = globals.viewporter.get_viewport(&chrome_surface, qh, ());
        let pool_bytes =
            (size.width as usize * size.height as usize * BYTES_PER_PIXEL).max(MIN_POOL_BYTES);
        let pool = SlotPool::new(pool_bytes, globals.shm).map_err(OverlayError::Pool)?;

        let viewport = globals.viewporter.get_viewport(layer.wl_surface(), qh, ());
        let alpha = globals.alpha.get_surface(layer.wl_surface(), qh, ());
        alpha.set_multiplier(factor);
        layer.wl_surface().commit();
        Ok(Overlay {
            layer,
            viewport,
            chrome_subsurface,
            chrome_surface,
            chrome_viewport,
            chrome_buffer: None,
            alpha,
            pool,
            size,
            position,
            configured: false,
        })
    }

    /// The layer surface's `wl_surface`, for pointer routing.
    pub fn surface(&self) -> &WlSurface {
        self.layer.wl_surface()
    }

    pub fn set_configured(&mut self) {
        self.configured = true;
    }

    pub fn is_configured(&self) -> bool {
        self.configured
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn position(&self) -> Point {
        self.position
    }

    /// Takes a new pool buffer of `buffer` pixels, lets `draw` fill it, and commits it on the
    /// chrome surface with `logical` as the viewport destination. A layer commit applies it.
    pub fn draw_chrome(
        &mut self,
        logical: Size,
        buffer: Size,
        draw: impl FnOnce(&mut [u8], Size),
    ) -> Result<(), OverlayError> {
        let (width, height) = (buffer.width as i32, buffer.height as i32);
        let (new, canvas) = self
            .pool
            .create_buffer(
                width,
                height,
                BYTES_PER_PIXEL as i32 * width,
                Format::Argb8888,
            )
            .map_err(OverlayError::Buffer)?;
        draw(canvas, buffer);
        self.chrome_viewport
            .set_destination(logical.width as i32, logical.height as i32);
        new.attach_to(&self.chrome_surface)
            .map_err(OverlayError::Activate)?;
        self.chrome_surface.damage_buffer(0, 0, width, height);
        self.chrome_surface.commit();
        // An active previous buffer is destroyed by sctk when the compositor releases it.
        self.chrome_buffer = Some(new);
        Ok(())
    }

    /// Requests in this order: transform, attach, viewport source and destination, layer size
    /// and margins when the size changed, damage, commit. It does not use sctk's `attach`,
    /// which adds an offset.
    pub fn present(
        &mut self,
        buffer: &WlBuffer,
        buffer_size: Size,
        size: Size,
        position: Point,
        y_invert: bool,
    ) {
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
            self.layer
                .set_margin(position.y as i32, 0, 0, position.x as i32);
            self.size = size;
            self.position = position;
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

    /// Margins and a layer commit; attaches nothing.
    pub fn move_to(&mut self, position: Point) {
        self.layer
            .set_margin(position.y as i32, 0, 0, position.x as i32);
        self.position = position;
        self.layer.wl_surface().commit();
    }

    /// Layer size, viewport destination and margins in one layer commit; attaches nothing.
    pub fn resize(&mut self, size: Size, position: Point) {
        self.layer.set_size(size.width, size.height);
        self.viewport
            .set_destination(size.width as i32, size.height as i32);
        self.layer
            .set_margin(position.y as i32, 0, 0, position.x as i32);
        self.size = size;
        self.position = position;
        self.layer.wl_surface().commit();
    }

    /// Sends the multiplier only; the next layer commit applies it.
    pub fn set_alpha(&mut self, factor: u32) {
        self.alpha.set_multiplier(factor);
    }

    /// Layer commit that applies a chrome-only render.
    pub fn commit(&mut self) {
        self.layer.wl_surface().commit();
    }

    /// Destroys the alpha object first, then the chrome viewport, subsurface, surface and pool,
    /// then the viewport, the layer surface and the `wl_surface`.
    pub fn destroy(self) {
        self.alpha.destroy();
        self.chrome_viewport.destroy();
        self.chrome_subsurface.destroy();
        self.chrome_surface.destroy();
        drop(self.chrome_buffer);
        drop(self.pool);
        self.viewport.destroy();
        drop(self.layer);
    }
}

/// The protocol multiplier for an opacity of `percent`, at most `MAX_OPACITY`: `u32::MAX` scaled
/// and rounded down.
pub fn alpha_factor(percent: u32) -> u32 {
    (u64::from(u32::MAX) * u64::from(percent.min(MAX_OPACITY)) / u64::from(MAX_OPACITY)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_factor_cases() {
        let cases = [
            ("zero", 0, 0),
            ("one", 1, 42_949_672),
            ("half", 50, 2_147_483_647),
            ("ninety-nine", 99, 4_252_017_622),
            ("full", 100, 4_294_967_295),
            ("above max", 101, 4_294_967_295),
        ];
        for (name, percent, want) in cases {
            assert_eq!(alpha_factor(percent), want, "{name}");
        }
    }
}

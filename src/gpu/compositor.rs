//! Same-frame Windows compositor backdrop for the low-latency renderer.
//!
//! The desktop-duplication renderer remains available as a compatibility and
//! pixel-exact fallback.  This path deliberately leaves backdrop ownership in
//! DWM: HostBackdrop is sampled during composition, so scrolling behind a note
//! cannot wait on LiquidNotes' capture, shader, message-loop, and present chain.

use std::sync::Mutex;

use windows::core::*;
use windows::Foundation::{IPropertyValue, PropertyValue};
use windows::Graphics::Effects::{
    IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectSource_Impl, IGraphicsEffect_Impl,
};
use windows::System::DispatcherQueueController;
use windows::Win32::Foundation::{E_BOUNDS, E_INVALIDARG, HWND};
use windows::Win32::Graphics::Direct2D::Common::D2D1_BORDER_MODE_HARD;
use windows::Win32::Graphics::Direct2D::{
    CLSID_D2D1GaussianBlur, D2D1_GAUSSIANBLUR_OPTIMIZATION_SPEED,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;
use windows::Win32::System::WinRT::Composition::{ICompositorDesktopInterop, ICompositorInterop};
use windows::Win32::System::WinRT::Graphics::Direct2D::{
    IGraphicsEffectD2D1Interop, IGraphicsEffectD2D1Interop_Impl, GRAPHICS_EFFECT_PROPERTY_MAPPING,
    GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT,
};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
};
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionEffectFactory, CompositionGeometricClip, CompositionRoundedRectangleGeometry,
    CompositionSurfaceBrush, Compositor, ContainerVisual, SpriteVisual,
};
use windows_numerics::{Vector2, Vector3};

const BACKDROP_SOURCE: &str = "backdrop";

/// Minimal dependency-free Win2D-compatible Gaussian descriptor.  Windows
/// Composition consumes these three standard D2D properties directly; no
/// custom shader or packaged Win2D runtime is involved.
#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct GaussianBlurEffect {
    name: Mutex<HSTRING>,
    source: IGraphicsEffectSource,
    sigma: f32,
}

impl IGraphicsEffectSource_Impl for GaussianBlurEffect_Impl {}

impl IGraphicsEffect_Impl for GaussianBlurEffect_Impl {
    fn Name(&self) -> Result<HSTRING> {
        Ok(self.name.lock().unwrap().clone())
    }

    fn SetName(&self, name: &HSTRING) -> Result<()> {
        *self.name.lock().unwrap() = name.clone();
        Ok(())
    }
}

impl IGraphicsEffectD2D1Interop_Impl for GaussianBlurEffect_Impl {
    fn GetEffectId(&self) -> Result<GUID> {
        Ok(CLSID_D2D1GaussianBlur)
    }

    fn GetNamedPropertyMapping(
        &self,
        name: &PCWSTR,
        index: *mut u32,
        mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> Result<()> {
        let name = unsafe { name.to_string()? };
        let property = match name.as_str() {
            "BlurAmount" => 0,
            "Optimization" => 1,
            "BorderMode" => 2,
            _ => return Err(Error::from_hresult(E_INVALIDARG)),
        };
        unsafe {
            index.write(property);
            mapping.write(GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT);
        }
        Ok(())
    }

    fn GetPropertyCount(&self) -> Result<u32> {
        Ok(3)
    }

    fn GetProperty(&self, index: u32) -> Result<IPropertyValue> {
        let value = match index {
            0 => PropertyValue::CreateSingle(self.sigma.max(0.0))?,
            1 => PropertyValue::CreateUInt32(D2D1_GAUSSIANBLUR_OPTIMIZATION_SPEED.0 as u32)?,
            2 => PropertyValue::CreateUInt32(D2D1_BORDER_MODE_HARD.0 as u32)?,
            _ => return Err(Error::from_hresult(E_BOUNDS)),
        };
        value.cast()
    }

    fn GetSource(&self, index: u32) -> Result<IGraphicsEffectSource> {
        if index == 0 {
            Ok(self.source.clone())
        } else {
            Err(Error::from_hresult(E_BOUNDS))
        }
    }

    fn GetSourceCount(&self) -> Result<u32> {
        Ok(1)
    }
}

pub struct HostCompositor {
    compositor: Compositor,
    desktop: ICompositorDesktopInterop,
    interop: ICompositorInterop,
    blur_factory: CompositionEffectFactory,
    // A current-thread DispatcherQueue is required when hosting the Visual
    // Layer in a classic Win32 message loop.  Keep its controller alive.
    _queue: DispatcherQueueController,
}

pub struct HostSurface {
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    backdrop: SpriteVisual,
    _content: SpriteVisual,
    clip_geometry: CompositionRoundedRectangleGeometry,
    _clip: CompositionGeometricClip,
    _surface_brush: CompositionSurfaceBrush,
}

impl HostCompositor {
    pub fn new(sigma: f32) -> Result<Self> {
        unsafe {
            let queue = CreateDispatcherQueueController(DispatcherQueueOptions {
                dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                threadType: DQTYPE_THREAD_CURRENT,
                apartmentType: DQTAT_COM_STA,
            })?;
            let compositor = Compositor::new()?;
            let desktop: ICompositorDesktopInterop = compositor.cast()?;
            let interop: ICompositorInterop = compositor.cast()?;

            let parameter = windows::UI::Composition::CompositionEffectSourceParameter::Create(
                &HSTRING::from(BACKDROP_SOURCE),
            )?;
            let source: IGraphicsEffectSource = parameter.cast()?;
            let effect: IGraphicsEffect = GaussianBlurEffect {
                name: Mutex::new(HSTRING::from("LiquidNotesBlur")),
                source,
                sigma,
            }
            .into();
            let blur_factory = compositor.CreateEffectFactory(&effect)?;

            Ok(Self {
                compositor,
                desktop,
                interop,
                blur_factory,
                _queue: queue,
            })
        }
    }

    pub fn create_surface(
        &self,
        hwnd: HWND,
        swapchain: &IDXGISwapChain1,
        width: u32,
        height: u32,
        corner_radius: f32,
        backdrop_enabled: bool,
    ) -> Result<HostSurface> {
        unsafe {
            let target = self.desktop.CreateDesktopWindowTarget(hwnd, true)?;
            let root = self.compositor.CreateContainerVisual()?;
            let backdrop = self.compositor.CreateSpriteVisual()?;
            let content = self.compositor.CreateSpriteVisual()?;

            let host_brush = self.compositor.CreateHostBackdropBrush()?;
            let blur_brush = self.blur_factory.CreateBrush()?;
            blur_brush.SetSourceParameter(&HSTRING::from(BACKDROP_SOURCE), &host_brush)?;
            backdrop.SetBrush(&blur_brush)?;

            let surface = self
                .interop
                .CreateCompositionSurfaceForSwapChain(swapchain)?;
            let surface_brush = self.compositor.CreateSurfaceBrushWithSurface(&surface)?;
            content.SetBrush(&surface_brush)?;

            let geometry = self.compositor.CreateRoundedRectangleGeometry()?;
            let clip = self.compositor.CreateGeometricClipWithGeometry(&geometry)?;
            backdrop.SetClip(&clip)?;

            let children = root.Children()?;
            if backdrop_enabled {
                children.InsertAtBottom(&backdrop)?;
            }
            children.InsertAtTop(&content)?;
            target.SetRoot(&root)?;

            let surface = HostSurface {
                _target: target,
                root,
                backdrop,
                _content: content,
                clip_geometry: geometry,
                _clip: clip,
                _surface_brush: surface_brush,
            };
            surface.set_size(width, height, corner_radius)?;
            Ok(surface)
        }
    }
}

impl HostSurface {
    pub fn set_size(&self, width: u32, height: u32, corner_radius: f32) -> Result<()> {
        let size = Vector2 {
            X: width as f32,
            Y: height as f32,
        };
        self.root.SetSize(size)?;
        self.backdrop.SetSize(size)?;
        self._content.SetSize(size)?;
        self.clip_geometry.SetSize(size)?;
        let radius = corner_radius.max(0.0).min(0.5 * width.min(height) as f32);
        self.clip_geometry.SetCornerRadius(Vector2 {
            X: radius,
            Y: radius,
        })?;
        Ok(())
    }

    pub fn set_reveal(&self, reveal: f32) -> Result<()> {
        self.backdrop.SetOpacity(reveal.clamp(0.0, 1.0))
    }

    pub fn set_rotation(&self, degrees: f32, cx: f32, cy: f32) -> Result<()> {
        self.root.SetCenterPoint(Vector3 {
            X: cx,
            Y: cy,
            Z: 0.0,
        })?;
        self.root.SetRotationAngleInDegrees(degrees)
    }
}

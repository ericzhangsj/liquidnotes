use windows::core::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

impl Gpu {
    pub fn new() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        Ok(Self {
            device: device.unwrap(),
            context: context.unwrap(),
        })
    }

    pub fn dxgi_device(&self) -> Result<IDXGIDevice> {
        self.device.cast()
    }
}

/// Compile an HLSL entry point; on failure return the compiler log as the error.
pub fn compile_shader(src: &str, entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut blob: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let hr = unsafe {
        D3DCompile(
            src.as_ptr() as _,
            src.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut blob,
            Some(&mut errors),
        )
    };
    if let Err(e) = hr {
        let log = errors
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        return Err(Error::new(e.code(), format!("HLSL compile failed: {log}")));
    }
    Ok(blob.unwrap())
}

pub fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

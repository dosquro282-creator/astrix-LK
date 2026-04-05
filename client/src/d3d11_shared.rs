#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use thiserror::Error;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource};

#[derive(Debug, Error)]
pub enum D3d11SharedTextureError {
    #[error("Windows: {0}")]
    Win(#[from] windows::core::Error),
    #[error("create shared texture returned null")]
    CreateTextureNull,
    #[error("open shared texture returned null")]
    OpenSharedTextureNull,
}

pub struct SharedKeyedTextureRing<const N: usize> {
    pub owner_textures: [ID3D11Texture2D; N],
    pub owner_mutexes: [IDXGIKeyedMutex; N],
    pub opened_textures: [ID3D11Texture2D; N],
    pub opened_mutexes: [IDXGIKeyedMutex; N],
}

pub fn create_shared_keyed_texture_ring<const N: usize>(
    owner_device: &ID3D11Device,
    opened_device: &ID3D11Device,
    base_desc: &D3D11_TEXTURE2D_DESC,
) -> Result<SharedKeyedTextureRing<N>, D3d11SharedTextureError> {
    let mut owner_textures: [Option<ID3D11Texture2D>; N] = std::array::from_fn(|_| None);
    let mut owner_mutexes: [Option<IDXGIKeyedMutex>; N] = std::array::from_fn(|_| None);
    let mut opened_textures: [Option<ID3D11Texture2D>; N] = std::array::from_fn(|_| None);
    let mut opened_mutexes: [Option<IDXGIKeyedMutex>; N] = std::array::from_fn(|_| None);

    for i in 0..N {
        let mut desc = *base_desc;
        desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32;

        let mut owner_tex = None;
        unsafe {
            owner_device.CreateTexture2D(
                &desc,
                None,
                Some(std::ptr::from_mut(&mut owner_tex)),
            )?;
        }
        let owner_tex = owner_tex.ok_or(D3d11SharedTextureError::CreateTextureNull)?;
        let owner_mutex: IDXGIKeyedMutex = owner_tex.cast()?;
        let dxgi_resource: IDXGIResource = owner_tex.cast()?;
        let handle = unsafe { dxgi_resource.GetSharedHandle()? };

        let mut opened_tex: Option<ID3D11Texture2D> = None;
        unsafe {
            opened_device.OpenSharedResource(handle, &mut opened_tex)?;
        }
        let opened_tex = opened_tex.ok_or(D3d11SharedTextureError::OpenSharedTextureNull)?;
        let opened_mutex: IDXGIKeyedMutex = opened_tex.cast()?;

        owner_textures[i] = Some(owner_tex);
        owner_mutexes[i] = Some(owner_mutex);
        opened_textures[i] = Some(opened_tex);
        opened_mutexes[i] = Some(opened_mutex);
    }

    Ok(SharedKeyedTextureRing {
        owner_textures: owner_textures.map(|t| t.expect("shared owner texture missing")),
        owner_mutexes: owner_mutexes.map(|m| m.expect("shared owner mutex missing")),
        opened_textures: opened_textures.map(|t| t.expect("shared opened texture missing")),
        opened_mutexes: opened_mutexes.map(|m| m.expect("shared opened mutex missing")),
    })
}

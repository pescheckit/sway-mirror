use anyhow::{bail, Result};
use nix::libc;
use std::os::unix::io::{AsFd, FromRawFd, OwnedFd};
use wayland_client::{
    protocol::{wl_buffer::WlBuffer, wl_shm, wl_shm_pool::WlShmPool},
    QueueHandle, WEnum,
};

use crate::wayland::AppState;

pub const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241; // AR24
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258; // XR24
pub const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241; // AB24
pub const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258; // XB24

/// 8-bit 4-channel formats we know how to upload from shared memory
pub const SUPPORTED_SHM_FORMATS: [u32; 4] = [
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ABGR8888,
];

/// wl_shm uses 0/1 for ARGB8888/XRGB8888 and DRM fourcc codes for the rest
pub fn wl_shm_to_drm(format: u32) -> u32 {
    match format {
        0 => DRM_FORMAT_ARGB8888,
        1 => DRM_FORMAT_XRGB8888,
        f => f,
    }
}

pub fn drm_to_wl_shm(format: u32) -> u32 {
    match format {
        DRM_FORMAT_ARGB8888 => 0,
        DRM_FORMAT_XRGB8888 => 1,
        f => f,
    }
}

pub fn wenum_raw(format: WEnum<wl_shm::Format>) -> u32 {
    match format {
        WEnum::Value(f) => f as u32,
        WEnum::Unknown(v) => v,
    }
}

/// Render a fourcc code as its 4-letter name (e.g. "XR24")
pub fn fourcc_name(format: u32) -> String {
    format.to_le_bytes().iter().map(|&b| b as char).collect()
}

/// A wl_buffer backed by a memfd mapping that we can read back on the CPU
pub struct ShmBuffer {
    pub buffer: WlBuffer,
    pool: WlShmPool,
    ptr: *mut u8,
    size: usize,
    _fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// DRM fourcc
    pub format: u32,
}

impl ShmBuffer {
    pub fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<AppState>,
        width: u32,
        height: u32,
        stride: u32,
        drm_format: u32,
    ) -> Result<Self> {
        let size = stride as usize * height as usize;
        if size == 0 {
            bail!(
                "Invalid shm buffer size {}x{} stride {}",
                width,
                height,
                stride
            );
        }

        let fd = unsafe { libc::memfd_create(c"sway-mirror".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            bail!("memfd_create failed: {}", std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        if unsafe { libc::ftruncate(std::os::unix::io::AsRawFd::as_raw_fd(&fd), size as i64) } < 0 {
            bail!("ftruncate failed: {}", std::io::Error::last_os_error());
        }

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            bail!("mmap failed: {}", std::io::Error::last_os_error());
        }

        let wl_format = wl_shm::Format::try_from(drm_to_wl_shm(drm_format))
            .map_err(|_| anyhow::anyhow!("Unknown wl_shm format {:#x}", drm_format))?;

        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_format,
            qh,
            (),
        );

        Ok(Self {
            buffer,
            pool,
            ptr: ptr as *mut u8,
            size,
            _fd: fd,
            width,
            height,
            stride,
            format: drm_format,
        })
    }

    pub fn matches(&self, width: u32, height: u32, stride: u32, drm_format: u32) -> bool {
        self.width == width
            && self.height == height
            && self.stride == stride
            && self.format == drm_format
    }

    pub fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shm_format_mapping_roundtrips() {
        assert_eq!(wl_shm_to_drm(0), DRM_FORMAT_ARGB8888);
        assert_eq!(wl_shm_to_drm(1), DRM_FORMAT_XRGB8888);
        assert_eq!(wl_shm_to_drm(DRM_FORMAT_XBGR8888), DRM_FORMAT_XBGR8888);
        for f in SUPPORTED_SHM_FORMATS {
            assert_eq!(wl_shm_to_drm(drm_to_wl_shm(f)), f);
        }
    }

    #[test]
    fn fourcc_names() {
        assert_eq!(fourcc_name(DRM_FORMAT_XRGB8888), "XR24");
        assert_eq!(fourcc_name(DRM_FORMAT_ABGR8888), "AB24");
    }
}

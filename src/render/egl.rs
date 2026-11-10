use anyhow::{bail, Context, Result};
use khronos_egl as egl;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};

use crate::capture::shm::{
    DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888,
};
use crate::capture::{fourcc_name, CapturedFrame, Frame, ShmFrame, DRM_FORMAT_MOD_INVALID};
use crate::render::ScaleMode;

// EGL extensions for dmabuf import
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
// Per-plane attributes: [fd, offset, pitch, modifier_lo, modifier_hi]
const EGL_DMA_BUF_PLANE_ATTRIBS: [[i32; 5]; 4] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_NO_CONTEXT: *mut c_void = std::ptr::null_mut();

const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

type CreateImageKhr =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type DestroyImageKhr = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type ImageTargetTexture2dOes = unsafe extern "C" fn(u32, *mut c_void);
type QueryDmaBufModifiersExt =
    unsafe extern "C" fn(*mut c_void, i32, i32, *mut u64, *mut u32, *mut i32) -> u32;

const VERTEX_SHADER: &str = r#"#version 100
attribute vec2 pos;
attribute vec2 tex;
uniform float u_flip;
varying vec2 v_tex;
void main() {
    gl_Position = vec4(pos, 0.0, 1.0);
    v_tex = vec2(tex.x, mix(tex.y, 1.0 - tex.y, u_flip));
}
"#;

// Alpha is forced to 1: we mirror an opaque desktop onto an opaque surface
const FRAGMENT_SHADER_2D: &str = r#"#version 100
precision mediump float;
varying vec2 v_tex;
uniform sampler2D u_texture;
uniform float u_swizzle;
void main() {
    vec4 c = texture2D(u_texture, v_tex);
    gl_FragColor = vec4(mix(c.rgb, c.bgr, u_swizzle), 1.0);
}
"#;

const FRAGMENT_SHADER_EXTERNAL: &str = r#"#version 100
#extension GL_OES_EGL_image_external : require
precision mediump float;
varying vec2 v_tex;
uniform samplerExternalOES u_texture;
void main() {
    gl_FragColor = vec4(texture2D(u_texture, v_tex).rgb, 1.0);
}
"#;

struct Program {
    id: u32,
    u_texture: i32,
    u_flip: i32,
    u_swizzle: i32,
}

/// A texture ready to be drawn, plus how to draw it
struct BoundTexture {
    target: u32,
    texture: u32,
    external: bool,
    swizzle: bool,
    width: u32,
    height: u32,
    y_invert: bool,
    image: Option<*mut c_void>,
}

pub struct EglContext {
    pub egl: egl::DynamicInstance<egl::EGL1_5>,
    pub display: egl::Display,
    pub context: egl::Context,
    pub config: egl::Config,
    /// EGL_EXT_image_dma_buf_import_modifiers is available
    pub supports_modifiers: bool,
    create_image: Option<CreateImageKhr>,
    destroy_image: Option<DestroyImageKhr>,
    query_modifiers: Option<QueryDmaBufModifiersExt>,
    image_target: Option<ImageTargetTexture2dOes>,
    // GL state
    vao: u32,
    program_2d: Option<Program>,
    program_external: Option<Program>,
    dmabuf_texture: u32,
    external_texture: u32,
    shm_texture: u32,
    shm_texture_size: (u32, u32),
    repack: Vec<u8>,
    /// format -> [(modifier, external_only)]
    modifier_cache: HashMap<u32, Vec<(u64, bool)>>,
}

impl EglContext {
    pub fn new(wayland_display: *mut c_void) -> Result<Self> {
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .context("Failed to load EGL")?;

        // Get EGL display from Wayland
        let display = unsafe { egl.get_display(wayland_display) }
            .ok_or_else(|| anyhow::anyhow!("Failed to get EGL display"))?;

        egl.initialize(display)
            .context("Failed to initialize EGL")?;

        // Choose config
        let config_attribs = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES2_BIT,
            egl::NONE,
        ];

        let config = egl
            .choose_first_config(display, &config_attribs)
            .context("Failed to choose EGL config")?
            .ok_or_else(|| anyhow::anyhow!("No suitable EGL config found"))?;

        // Bind OpenGL ES API
        egl.bind_api(egl::OPENGL_ES_API)
            .context("Failed to bind OpenGL ES API")?;

        // Create context
        let context_attribs = [
            egl::CONTEXT_MAJOR_VERSION,
            2,
            egl::CONTEXT_MINOR_VERSION,
            0,
            egl::NONE,
        ];

        let context = egl
            .create_context(display, config, None, &context_attribs)
            .context("Failed to create EGL context")?;

        let extensions = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let has_ext = |name: &str| extensions.split_whitespace().any(|e| e == name);

        let supports_dmabuf = has_ext("EGL_EXT_image_dma_buf_import");
        let supports_modifiers = has_ext("EGL_EXT_image_dma_buf_import_modifiers");
        if !supports_dmabuf {
            eprintln!("Warning: EGL_EXT_image_dma_buf_import not supported; zero-copy disabled");
        } else if !supports_modifiers {
            eprintln!(
                "Warning: EGL_EXT_image_dma_buf_import_modifiers not supported; \
                 tiled buffers may render incorrectly"
            );
        }

        let proc = |name: &str| egl.get_proc_address(name).map(|p| p as *const c_void);
        let (create_image, destroy_image, image_target, query_modifiers) = unsafe {
            (
                proc("eglCreateImageKHR")
                    .filter(|_| supports_dmabuf)
                    .map(|p| std::mem::transmute::<*const c_void, CreateImageKhr>(p)),
                proc("eglDestroyImageKHR")
                    .map(|p| std::mem::transmute::<*const c_void, DestroyImageKhr>(p)),
                proc("glEGLImageTargetTexture2DOES")
                    .map(|p| std::mem::transmute::<*const c_void, ImageTargetTexture2dOes>(p)),
                proc("eglQueryDmaBufModifiersEXT")
                    .filter(|_| supports_modifiers)
                    .map(|p| std::mem::transmute::<*const c_void, QueryDmaBufModifiersExt>(p)),
            )
        };

        Ok(Self {
            egl,
            display,
            context,
            config,
            supports_modifiers,
            create_image,
            destroy_image,
            query_modifiers,
            image_target,
            vao: 0,
            program_2d: None,
            program_external: None,
            dmabuf_texture: 0,
            external_texture: 0,
            shm_texture: 0,
            shm_texture_size: (0, 0),
            repack: Vec::new(),
            modifier_cache: HashMap::new(),
        })
    }

    pub fn make_current(&self, surface: egl::Surface) -> Result<()> {
        self.egl
            .make_current(
                self.display,
                Some(surface),
                Some(surface),
                Some(self.context),
            )
            .context("Failed to make EGL context current")?;
        Ok(())
    }

    pub fn make_current_surfaceless(&self) -> Result<()> {
        self.egl
            .make_current(self.display, None, None, Some(self.context))
            .context("Failed to make surfaceless context current")?;
        Ok(())
    }

    pub fn create_window_surface(
        &self,
        native_window: egl::NativeWindowType,
    ) -> Result<egl::Surface> {
        let surface = unsafe {
            self.egl
                .create_window_surface(self.display, self.config, native_window, None)
        }
        .context("Failed to create EGL window surface")?;
        Ok(surface)
    }

    pub fn swap_buffers(&self, surface: egl::Surface) -> Result<()> {
        self.egl
            .swap_buffers(self.display, surface)
            .context("Failed to swap buffers")?;
        Ok(())
    }

    pub fn init_gl(&mut self) -> Result<()> {
        unsafe {
            gl::load_with(|s| {
                self.egl
                    .get_proc_address(s)
                    .map(|p| p as *const c_void)
                    .unwrap_or(std::ptr::null())
            });

            let gl_extensions = {
                let p = gl::GetString(gl::EXTENSIONS);
                if p.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(p as *const _).to_string_lossy().into_owned()
                }
            };

            self.program_2d = Some(self.link_program(FRAGMENT_SHADER_2D)?);
            if gl_extensions
                .split_whitespace()
                .any(|e| e == "GL_OES_EGL_image_external")
            {
                match self.link_program(FRAGMENT_SHADER_EXTERNAL) {
                    Ok(p) => self.program_external = Some(p),
                    Err(e) => eprintln!("Warning: external texture shader unavailable: {e:#}"),
                }
            }

            // Create VAO and VBO
            let mut vao = 0;
            gl::GenVertexArrays(1, &mut vao);
            self.vao = vao;
            gl::BindVertexArray(vao);

            // Full screen quad
            let vertices: [f32; 16] = [
                // pos      // tex
                -1.0, -1.0, 0.0, 1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0,
            ];

            let mut vbo = 0;
            gl::GenBuffers(1, &mut vbo);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as isize,
                vertices.as_ptr() as *const c_void,
                gl::STATIC_DRAW,
            );

            // Attribute locations are bound to 0/1 for every program
            let stride = (4 * std::mem::size_of::<f32>()) as i32;
            gl::EnableVertexAttribArray(0);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, stride, std::ptr::null());
            gl::EnableVertexAttribArray(1);
            gl::VertexAttribPointer(
                1,
                2,
                gl::FLOAT,
                gl::FALSE,
                stride,
                (2 * std::mem::size_of::<f32>()) as *const c_void,
            );

            let mut textures = [0u32; 3];
            gl::GenTextures(3, textures.as_mut_ptr());
            self.dmabuf_texture = textures[0];
            self.external_texture = textures[1];
            self.shm_texture = textures[2];
        }

        Ok(())
    }

    unsafe fn link_program(&self, fragment_src: &str) -> Result<Program> {
        let vs = self.compile_shader(gl::VERTEX_SHADER, VERTEX_SHADER)?;
        let fs = self.compile_shader(gl::FRAGMENT_SHADER, fragment_src)?;

        let id = gl::CreateProgram();
        gl::AttachShader(id, vs);
        gl::AttachShader(id, fs);
        gl::BindAttribLocation(id, 0, c"pos".as_ptr());
        gl::BindAttribLocation(id, 1, c"tex".as_ptr());
        gl::LinkProgram(id);
        gl::DeleteShader(vs);
        gl::DeleteShader(fs);

        let mut status = 0;
        gl::GetProgramiv(id, gl::LINK_STATUS, &mut status);
        if status == 0 {
            gl::DeleteProgram(id);
            bail!("Failed to link shader program");
        }

        Ok(Program {
            id,
            u_texture: gl::GetUniformLocation(id, c"u_texture".as_ptr()),
            u_flip: gl::GetUniformLocation(id, c"u_flip".as_ptr()),
            u_swizzle: gl::GetUniformLocation(id, c"u_swizzle".as_ptr()),
        })
    }

    unsafe fn compile_shader(&self, shader_type: u32, source: &str) -> Result<u32> {
        let shader = gl::CreateShader(shader_type);
        let source_ptr = source.as_ptr() as *const gl::types::GLchar;
        let source_len = source.len() as i32;
        gl::ShaderSource(shader, 1, &source_ptr, &source_len);
        gl::CompileShader(shader);

        // Check compile status
        let mut status = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut status);
        if status == 0 {
            let mut len = 0;
            gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len.max(1) as usize];
            gl::GetShaderInfoLog(
                shader,
                len,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut gl::types::GLchar,
            );
            gl::DeleteShader(shader);
            bail!("Shader compile error: {}", String::from_utf8_lossy(&buf));
        }

        Ok(shader)
    }

    /// Modifiers EGL can import for `format`, with their external_only flag
    pub fn query_modifiers(&mut self, format: u32) -> Vec<(u64, bool)> {
        if let Some(cached) = self.modifier_cache.get(&format) {
            return cached.clone();
        }
        let mut result = Vec::new();
        if let Some(query) = self.query_modifiers {
            let dpy = self.display.as_ptr();
            unsafe {
                let mut count = 0;
                if query(
                    dpy,
                    format as i32,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut count,
                ) != 0
                    && count > 0
                {
                    let mut mods = vec![0u64; count as usize];
                    let mut external = vec![0u32; count as usize];
                    if query(
                        dpy,
                        format as i32,
                        count,
                        mods.as_mut_ptr(),
                        external.as_mut_ptr(),
                        &mut count,
                    ) != 0
                    {
                        result = mods
                            .into_iter()
                            .zip(external)
                            .take(count as usize)
                            .map(|(m, e)| (m, e != 0))
                            .collect();
                    }
                }
            }
        }
        self.modifier_cache.insert(format, result.clone());
        result
    }

    /// Importable modifiers per format, for allocating capture buffers
    pub fn importable_modifiers(&mut self, formats: &[u32]) -> HashMap<u32, Vec<u64>> {
        formats
            .iter()
            .map(|&f| {
                (
                    f,
                    self.query_modifiers(f)
                        .into_iter()
                        .map(|(m, _)| m)
                        .collect(),
                )
            })
            .collect()
    }

    fn is_external_only(&mut self, format: u32, modifier: u64) -> bool {
        if modifier == DRM_FORMAT_MOD_INVALID {
            return false;
        }
        self.query_modifiers(format)
            .iter()
            .any(|&(m, external)| m == modifier && external)
    }

    unsafe fn import_dmabuf(&mut self, frame: &CapturedFrame) -> Result<BoundTexture> {
        let (Some(create_image), Some(image_target)) = (self.create_image, self.image_target)
        else {
            bail!("EGL dmabuf import is not supported by this driver");
        };
        if frame.planes.is_empty() {
            bail!("dmabuf frame without planes");
        }
        if frame.planes.len() > EGL_DMA_BUF_PLANE_ATTRIBS.len() {
            bail!("Unsupported dmabuf plane count: {}", frame.planes.len());
        }

        let mut attribs: Vec<i32> = vec![
            EGL_WIDTH,
            frame.width as i32,
            EGL_HEIGHT,
            frame.height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            frame.format as i32,
        ];

        // Tiled/compressed layouts (Intel Tile4+CCS, AMD DCC) need the modifier and all planes
        let pass_modifier = self.supports_modifiers && frame.modifier != DRM_FORMAT_MOD_INVALID;

        for (plane, keys) in frame.planes.iter().zip(EGL_DMA_BUF_PLANE_ATTRIBS.iter()) {
            attribs.extend_from_slice(&[
                keys[0],
                plane.fd,
                keys[1],
                plane.offset as i32,
                keys[2],
                plane.stride as i32,
            ]);
            if pass_modifier {
                attribs.extend_from_slice(&[
                    keys[3],
                    (frame.modifier & 0xffff_ffff) as u32 as i32,
                    keys[4],
                    (frame.modifier >> 32) as u32 as i32,
                ]);
            }
        }
        attribs.push(egl::NONE);

        let image = create_image(
            self.display.as_ptr(),
            EGL_NO_CONTEXT,
            EGL_LINUX_DMA_BUF_EXT,
            std::ptr::null_mut(),
            attribs.as_ptr(),
        );
        if image.is_null() {
            bail!(
                "Failed to create EGL image from dmabuf ({} modifier 0x{:016x}, {} planes, EGL error {:?})",
                fourcc_name(frame.format),
                frame.modifier,
                frame.planes.len(),
                self.egl.get_error()
            );
        }

        // Some modifiers (NVIDIA, Mali AFBC) can only be sampled as GL_TEXTURE_EXTERNAL_OES
        let external = self.is_external_only(frame.format, frame.modifier);
        if external && self.program_external.is_none() {
            self.destroy_image(image);
            bail!("dmabuf needs GL_OES_EGL_image_external, which is not supported");
        }
        let (target, texture) = if external {
            (GL_TEXTURE_EXTERNAL_OES, self.external_texture)
        } else {
            (gl::TEXTURE_2D, self.dmabuf_texture)
        };

        while gl::GetError() != gl::NO_ERROR {}
        gl::BindTexture(target, texture);
        image_target(target, image);
        let err = gl::GetError();
        if err != gl::NO_ERROR {
            self.destroy_image(image);
            bail!("glEGLImageTargetTexture2DOES failed (GL error {:#x})", err);
        }
        set_texture_params(target);

        Ok(BoundTexture {
            target,
            texture,
            external,
            swizzle: false,
            width: frame.width,
            height: frame.height,
            y_invert: frame.y_invert,
            image: Some(image),
        })
    }

    unsafe fn upload_shm(&mut self, frame: &ShmFrame) -> Result<BoundTexture> {
        // [AX]RGB8888 is BGRA in memory; swap red/blue in the shader
        let swizzle = match frame.format {
            DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => true,
            DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => false,
            f => bail!("Unsupported shm format {} ({:#x})", fourcc_name(f), f),
        };

        let row = frame.width as usize * 4;
        let height = frame.height as usize;
        let stride = frame.stride as usize;
        if frame.data.len() < stride * height || stride < row {
            bail!("shm frame smaller than its stride/height");
        }

        // GLES2 has no GL_UNPACK_ROW_LENGTH: repack padded rows
        let pixels: &[u8] = if stride == row {
            &frame.data[..row * height]
        } else {
            self.repack.resize(row * height, 0);
            for y in 0..height {
                self.repack[y * row..(y + 1) * row]
                    .copy_from_slice(&frame.data[y * stride..y * stride + row]);
            }
            &self.repack
        };

        gl::BindTexture(gl::TEXTURE_2D, self.shm_texture);
        gl::PixelStorei(gl::UNPACK_ALIGNMENT, 4);
        if self.shm_texture_size != (frame.width, frame.height) {
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA as i32,
                frame.width as i32,
                frame.height as i32,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                pixels.as_ptr() as *const c_void,
            );
            set_texture_params(gl::TEXTURE_2D);
            self.shm_texture_size = (frame.width, frame.height);
        } else {
            gl::TexSubImage2D(
                gl::TEXTURE_2D,
                0,
                0,
                0,
                frame.width as i32,
                frame.height as i32,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                pixels.as_ptr() as *const c_void,
            );
        }

        Ok(BoundTexture {
            target: gl::TEXTURE_2D,
            texture: self.shm_texture,
            external: false,
            swizzle,
            width: frame.width,
            height: frame.height,
            y_invert: frame.y_invert,
            image: None,
        })
    }

    fn destroy_image(&self, image: *mut c_void) {
        if let Some(destroy) = self.destroy_image {
            unsafe {
                destroy(self.display.as_ptr(), image);
            }
        }
    }

    /// Import `frame` once and draw it to all targets; on error nothing was drawn
    pub fn render(
        &mut self,
        frame: &Frame,
        targets: &[(egl::Surface, i32, i32)],
        scale_mode: ScaleMode,
    ) -> Result<()> {
        let Some(&(first, _, _)) = targets.first() else {
            return Ok(());
        };
        self.make_current(first)?;

        let bound = unsafe {
            match frame {
                Frame::Dmabuf(f) => self.import_dmabuf(f)?,
                Frame::Shm(f) => self.upload_shm(f)?,
            }
        };

        let result = (|| {
            for &(surface, width, height) in targets {
                self.make_current(surface)?;
                unsafe { self.draw(&bound, width, height, scale_mode) };
                self.swap_buffers(surface)?;
            }
            Ok(())
        })();

        if let Some(image) = bound.image {
            self.destroy_image(image);
        }
        result
    }

    unsafe fn draw(&self, tex: &BoundTexture, width: i32, height: i32, scale_mode: ScaleMode) {
        let (vp_x, vp_y, vp_w, vp_h) =
            compute_viewport(scale_mode, tex.width, tex.height, width, height);

        // Clear entire surface to black first
        gl::Viewport(0, 0, width, height);
        gl::ClearColor(0.0, 0.0, 0.0, 1.0);
        gl::Clear(gl::COLOR_BUFFER_BIT);

        gl::Viewport(vp_x, vp_y, vp_w, vp_h);

        let program = if tex.external {
            self.program_external.as_ref().unwrap()
        } else {
            self.program_2d.as_ref().unwrap()
        };
        gl::UseProgram(program.id);
        gl::ActiveTexture(gl::TEXTURE0);
        gl::BindTexture(tex.target, tex.texture);
        gl::Uniform1i(program.u_texture, 0);
        gl::Uniform1f(program.u_flip, if tex.y_invert { 1.0 } else { 0.0 });
        if program.u_swizzle >= 0 {
            gl::Uniform1f(program.u_swizzle, if tex.swizzle { 1.0 } else { 0.0 });
        }

        gl::BindVertexArray(self.vao);
        gl::DrawArrays(gl::TRIANGLE_STRIP, 0, 4);
    }
}

unsafe fn set_texture_params(target: u32) {
    gl::TexParameteri(target, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
    gl::TexParameteri(target, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
    gl::TexParameteri(target, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
    gl::TexParameteri(target, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
}

/// Viewport (x, y, w, h) for drawing a src_w x src_h image on a dst_w x dst_h surface
pub fn compute_viewport(
    scale_mode: ScaleMode,
    src_w: u32,
    src_h: u32,
    dst_w: i32,
    dst_h: i32,
) -> (i32, i32, i32, i32) {
    let src_aspect = src_w as f32 / src_h as f32;
    let dst_aspect = dst_w as f32 / dst_h as f32;

    match scale_mode {
        // Fill entire target, ignore aspect ratio
        ScaleMode::Stretch => (0, 0, dst_w, dst_h),
        // Preserve aspect ratio, fit within target (letterbox/pillarbox)
        ScaleMode::Fit => {
            if src_aspect > dst_aspect {
                let vp_h = (dst_w as f32 / src_aspect) as i32;
                (0, (dst_h - vp_h) / 2, dst_w, vp_h)
            } else {
                let vp_w = (dst_h as f32 * src_aspect) as i32;
                ((dst_w - vp_w) / 2, 0, vp_w, dst_h)
            }
        }
        // Preserve aspect ratio, fill target completely (crops edges)
        ScaleMode::Fill => {
            if src_aspect > dst_aspect {
                let vp_w = (dst_h as f32 * src_aspect) as i32;
                ((dst_w - vp_w) / 2, 0, vp_w, dst_h)
            } else {
                let vp_h = (dst_w as f32 / src_aspect) as i32;
                (0, (dst_h - vp_h) / 2, dst_w, vp_h)
            }
        }
        // Display at 1:1 pixel ratio, centered (no scaling)
        ScaleMode::Center => {
            let (w, h) = (src_w as i32, src_h as i32);
            ((dst_w - w) / 2, (dst_h - h) / 2, w, h)
        }
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_fit_letterboxes_and_pillarboxes() {
        // 16:10 onto 16:9 -> pillarbox
        assert_eq!(
            compute_viewport(ScaleMode::Fit, 1920, 1200, 1920, 1080),
            (96, 0, 1728, 1080)
        );
        // 16:9 onto portrait -> letterbox
        assert_eq!(
            compute_viewport(ScaleMode::Fit, 1920, 1080, 1080, 1920),
            (0, 656, 1080, 607)
        );
    }

    #[test]
    fn viewport_fill_stretch_center() {
        assert_eq!(
            compute_viewport(ScaleMode::Fill, 1920, 1200, 1920, 1080),
            (0, -60, 1920, 1200)
        );
        assert_eq!(
            compute_viewport(ScaleMode::Stretch, 1920, 1200, 3840, 2160),
            (0, 0, 3840, 2160)
        );
        assert_eq!(
            compute_viewport(ScaleMode::Center, 1920, 1080, 3840, 2160),
            (960, 540, 1920, 1080)
        );
    }
}

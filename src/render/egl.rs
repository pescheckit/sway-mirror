use anyhow::{bail, Context, Result};
use khronos_egl as egl;
use std::ffi::c_void;

use crate::capture::CapturedFrame;
use crate::render::ScaleMode;

pub struct EglContext {
    pub egl: egl::DynamicInstance<egl::EGL1_5>,
    pub display: egl::Display,
    pub context: egl::Context,
    pub config: egl::Config,
    // OpenGL state
    pub program: u32,
    pub vao: u32,
    pub texture: u32,
}

// EGL extensions for dmabuf import
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_NO_CONTEXT: *mut c_void = std::ptr::null_mut();

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

        Ok(Self {
            egl,
            display,
            context,
            config,
            program: 0,
            vao: 0,
            texture: 0,
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

            // Create shader program
            let vs_src = r#"
                #version 100
                attribute vec2 pos;
                attribute vec2 tex;
                varying vec2 v_tex;
                void main() {
                    gl_Position = vec4(pos, 0.0, 1.0);
                    v_tex = tex;
                }
            "#;

            let fs_src = r#"
                #version 100
                precision mediump float;
                varying vec2 v_tex;
                uniform sampler2D u_texture;
                void main() {
                    gl_FragColor = texture2D(u_texture, v_tex);
                }
            "#;

            let vs = self.compile_shader(gl::VERTEX_SHADER, vs_src)?;
            let fs = self.compile_shader(gl::FRAGMENT_SHADER, fs_src)?;

            self.program = gl::CreateProgram();
            gl::AttachShader(self.program, vs);
            gl::AttachShader(self.program, fs);
            gl::LinkProgram(self.program);

            // Check link status
            let mut status = 0;
            gl::GetProgramiv(self.program, gl::LINK_STATUS, &mut status);
            if status == 0 {
                bail!("Failed to link shader program");
            }

            gl::DeleteShader(vs);
            gl::DeleteShader(fs);

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

            let pos_loc = gl::GetAttribLocation(self.program, b"pos\0".as_ptr() as *const i8);
            let tex_loc = gl::GetAttribLocation(self.program, b"tex\0".as_ptr() as *const i8);

            gl::EnableVertexAttribArray(pos_loc as u32);
            gl::VertexAttribPointer(
                pos_loc as u32,
                2,
                gl::FLOAT,
                gl::FALSE,
                (4 * std::mem::size_of::<f32>()) as i32,
                std::ptr::null(),
            );

            gl::EnableVertexAttribArray(tex_loc as u32);
            gl::VertexAttribPointer(
                tex_loc as u32,
                2,
                gl::FLOAT,
                gl::FALSE,
                (4 * std::mem::size_of::<f32>()) as i32,
                (2 * std::mem::size_of::<f32>()) as *const c_void,
            );

            // Create texture
            let mut texture = 0;
            gl::GenTextures(1, &mut texture);
            self.texture = texture;
        }

        Ok(())
    }

    unsafe fn compile_shader(&self, shader_type: u32, source: &str) -> Result<u32> {
        let shader = gl::CreateShader(shader_type);
        let source_ptr = source.as_ptr() as *const i8;
        let source_len = source.len() as i32;
        gl::ShaderSource(shader, 1, &source_ptr, &source_len);
        gl::CompileShader(shader);

        // Check compile status
        let mut status = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut status);
        if status == 0 {
            let mut len = 0;
            gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl::GetShaderInfoLog(
                shader,
                len,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut i8,
            );
            bail!("Shader compile error: {}", String::from_utf8_lossy(&buf));
        }

        Ok(shader)
    }

    pub fn render_frame(
        &self,
        frame: &CapturedFrame,
        surface: egl::Surface,
        width: i32,
        height: i32,
        scale_mode: ScaleMode,
    ) -> Result<()> {
        self.make_current(surface)?;

        unsafe {
            let src_w = frame.width as f32;
            let src_h = frame.height as f32;
            let dst_w = width as f32;
            let dst_h = height as f32;
            let src_aspect = src_w / src_h;
            let dst_aspect = dst_w / dst_h;

            let (vp_x, vp_y, vp_w, vp_h) = match scale_mode {
                ScaleMode::Stretch => {
                    // Fill entire target, ignore aspect ratio
                    (0, 0, width, height)
                }
                ScaleMode::Fit => {
                    // Preserve aspect ratio, fit within target (letterbox/pillarbox)
                    if src_aspect > dst_aspect {
                        // Source is wider - letterbox (black bars top/bottom)
                        let vp_h = (dst_w / src_aspect) as i32;
                        let vp_y = (height - vp_h) / 2;
                        (0, vp_y, width, vp_h)
                    } else {
                        // Source is taller - pillarbox (black bars left/right)
                        let vp_w = (dst_h * src_aspect) as i32;
                        let vp_x = (width - vp_w) / 2;
                        (vp_x, 0, vp_w, height)
                    }
                }
                ScaleMode::Fill => {
                    // Preserve aspect ratio, fill target completely (crops edges)
                    if src_aspect > dst_aspect {
                        // Source is wider - extend beyond left/right edges
                        let vp_w = (dst_h * src_aspect) as i32;
                        let vp_x = (width - vp_w) / 2;
                        (vp_x, 0, vp_w, height)
                    } else {
                        // Source is taller - extend beyond top/bottom edges
                        let vp_h = (dst_w / src_aspect) as i32;
                        let vp_y = (height - vp_h) / 2;
                        (0, vp_y, width, vp_h)
                    }
                }
                ScaleMode::Center => {
                    // Display at 1:1 pixel ratio, centered (no scaling)
                    let vp_w = frame.width as i32;
                    let vp_h = frame.height as i32;
                    let vp_x = (width - vp_w) / 2;
                    let vp_y = (height - vp_h) / 2;
                    (vp_x, vp_y, vp_w, vp_h)
                }
            };

            // Clear entire surface to black first
            gl::Viewport(0, 0, width, height);
            gl::ClearColor(0.0, 0.0, 0.0, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);

            // Set viewport for rendering
            gl::Viewport(vp_x, vp_y, vp_w, vp_h);

            // Import dmabuf as EGL image and bind to texture
            if !frame.planes.is_empty() {
                let plane = &frame.planes[0];

                let attribs: [i32; 13] = [
                    EGL_WIDTH,
                    frame.width as i32,
                    EGL_HEIGHT,
                    frame.height as i32,
                    EGL_LINUX_DRM_FOURCC_EXT,
                    frame.format as i32,
                    EGL_DMA_BUF_PLANE0_FD_EXT,
                    plane.fd,
                    EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                    plane.offset as i32,
                    EGL_DMA_BUF_PLANE0_PITCH_EXT,
                    plane.stride as i32,
                    egl::NONE as i32,
                ];

                // Use eglCreateImageKHR
                type CreateImageKHR = unsafe extern "C" fn(
                    egl::Display,
                    *mut c_void, // EGLContext as raw pointer
                    u32,
                    *mut c_void,
                    *const i32,
                ) -> *mut c_void;

                let create_image: CreateImageKHR = std::mem::transmute(
                    self.egl
                        .get_proc_address("eglCreateImageKHR")
                        .ok_or_else(|| anyhow::anyhow!("eglCreateImageKHR not found"))?,
                );

                let image = create_image(
                    self.display,
                    EGL_NO_CONTEXT,
                    EGL_LINUX_DMA_BUF_EXT,
                    std::ptr::null_mut(),
                    attribs.as_ptr(),
                );

                if image.is_null() {
                    bail!("Failed to create EGL image from dmabuf");
                }

                // Bind to texture
                gl::BindTexture(gl::TEXTURE_2D, self.texture);

                type ImageTargetTexture2DOES = unsafe extern "C" fn(u32, *mut c_void);
                let image_target: ImageTargetTexture2DOES = std::mem::transmute(
                    self.egl
                        .get_proc_address("glEGLImageTargetTexture2DOES")
                        .ok_or_else(|| anyhow::anyhow!("glEGLImageTargetTexture2DOES not found"))?,
                );
                image_target(gl::TEXTURE_2D, image);

                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);

                // Render
                gl::UseProgram(self.program);
                gl::BindVertexArray(self.vao);
                gl::DrawArrays(gl::TRIANGLE_STRIP, 0, 4);

                // Destroy EGL image
                type DestroyImageKHR = unsafe extern "C" fn(egl::Display, *mut c_void) -> u32;
                let destroy_image: DestroyImageKHR = std::mem::transmute(
                    self.egl
                        .get_proc_address("eglDestroyImageKHR")
                        .ok_or_else(|| anyhow::anyhow!("eglDestroyImageKHR not found"))?,
                );
                destroy_image(self.display, image);
            }
        }

        self.swap_buffers(surface)?;
        Ok(())
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

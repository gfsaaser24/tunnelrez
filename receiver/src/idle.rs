//! Idle screen ("waiting for PC on ip:port"), drawn with EGL + scissored glClear and a 5x7 bitmap font.
//! EGL (not `ANativeWindow_lock`) because `eglDestroySurface` disconnects from the window, so MediaCodec can
//! connect to it afterwards.

use ndk::native_window::NativeWindow;
use std::os::raw::c_void;

type EGLDisplay = *mut c_void;
type EGLConfig = *mut c_void;
type EGLSurface = *mut c_void;
type EGLContext = *mut c_void;

#[link(name = "EGL")]
extern "C" {
    fn eglGetDisplay(id: *mut c_void) -> EGLDisplay;
    fn eglInitialize(d: EGLDisplay, major: *mut i32, minor: *mut i32) -> u32;
    fn eglChooseConfig(d: EGLDisplay, attr: *const i32, cfg: *mut EGLConfig, n: i32, num: *mut i32) -> u32;
    fn eglGetConfigAttrib(d: EGLDisplay, cfg: EGLConfig, attr: i32, v: *mut i32) -> u32;
    fn eglCreateWindowSurface(d: EGLDisplay, cfg: EGLConfig, win: *mut c_void, attr: *const i32) -> EGLSurface;
    fn eglCreateContext(d: EGLDisplay, cfg: EGLConfig, share: EGLContext, attr: *const i32) -> EGLContext;
    fn eglMakeCurrent(d: EGLDisplay, draw: EGLSurface, read: EGLSurface, ctx: EGLContext) -> u32;
    fn eglQuerySurface(d: EGLDisplay, s: EGLSurface, attr: i32, v: *mut i32) -> u32;
    fn eglSwapBuffers(d: EGLDisplay, s: EGLSurface) -> u32;
    fn eglDestroySurface(d: EGLDisplay, s: EGLSurface) -> u32;
    fn eglDestroyContext(d: EGLDisplay, c: EGLContext) -> u32;
    fn eglGetError() -> i32;
}

#[link(name = "GLESv2")]
extern "C" {
    fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    fn glClear(mask: u32);
    fn glEnable(cap: u32);
    fn glDisable(cap: u32);
    fn glScissor(x: i32, y: i32, w: i32, h: i32);
    fn glViewport(x: i32, y: i32, w: i32, h: i32);
}

const EGL_NONE: i32 = 0x3038;
const EGL_RED_SIZE: i32 = 0x3024;
const EGL_GREEN_SIZE: i32 = 0x3023;
const EGL_BLUE_SIZE: i32 = 0x3022;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_WINDOW_BIT: i32 = 0x0004;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
const EGL_NATIVE_VISUAL_ID: i32 = 0x302E;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_SCISSOR_TEST: u32 = 0x0C11;

/// One line of idle-screen text.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Line<'a> {
    pub(crate) text: &'a str,
    /// Pixels per font dot.
    pub(crate) scale: i32,
    pub(crate) rgb: [f32; 3],
}

fn egl_error(what: &str) -> String {
    // SAFETY: no preconditions.
    let e = unsafe { eglGetError() };
    format!("{what} {e:#x}")
}

/// An EGL window surface + GLES2 context, current on this thread while alive. Drop tears both down so
/// the window is free for MediaCodec afterwards (the last swapped frame stays on screen).
struct Target {
    d: EGLDisplay,
    surf: EGLSurface,
    ctx: EGLContext,
}

impl Target {
    fn new(win: &NativeWindow) -> Result<Target, String> {
        // SAFETY: EGL_DEFAULT_DISPLAY (null) is always a valid argument.
        let d = unsafe { eglGetDisplay(std::ptr::null_mut()) };
        // SAFETY: `d` is checked for null first; null version out-pointers are allowed.
        if d.is_null() || unsafe { eglInitialize(d, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
            return Err(egl_error("eglInitialize"));
        }
        let attr = [
            EGL_SURFACE_TYPE,
            EGL_WINDOW_BIT,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_NONE,
        ];
        let mut cfg: EGLConfig = std::ptr::null_mut();
        let mut n = 0;
        // SAFETY: `attr` is EGL_NONE-terminated; `cfg` has room for the 1 config asked for; `n` is valid.
        if unsafe { eglChooseConfig(d, attr.as_ptr(), &raw mut cfg, 1, &raw mut n) } == 0 || n < 1 {
            return Err(egl_error("eglChooseConfig"));
        }
        let mut fmt = 0;
        // SAFETY: `cfg` came from eglChooseConfig on `d`; `fmt` is a valid out-pointer.
        unsafe { eglGetConfigAttrib(d, cfg, EGL_NATIVE_VISUAL_ID, &raw mut fmt) };
        let wp = win.ptr().as_ptr();
        // SAFETY: `wp` is a live window (borrowed for this call); 0x0 keeps the window's own size.
        unsafe { ndk_sys::ANativeWindow_setBuffersGeometry(wp, 0, 0, fmt) };
        // SAFETY: `wp` is live and not connected to another producer (caller holds the decoder write lock).
        let surf = unsafe { eglCreateWindowSurface(d, cfg, wp.cast(), [EGL_NONE].as_ptr()) };
        if surf.is_null() {
            return Err(egl_error("eglCreateWindowSurface"));
        }
        let ctx_attr = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        // SAFETY: `cfg` is from `d`; no share context; `ctx_attr` is EGL_NONE-terminated.
        let ctx = unsafe { eglCreateContext(d, cfg, std::ptr::null_mut(), ctx_attr.as_ptr()) };
        if ctx.is_null() {
            let e = egl_error("eglCreateContext");
            // SAFETY: `surf` was created above on `d` and is destroyed once.
            unsafe { eglDestroySurface(d, surf) };
            return Err(e);
        }
        // SAFETY: surface and context are live and belong to `d`.
        unsafe { eglMakeCurrent(d, surf, surf, ctx) };
        Ok(Target { d, surf, ctx })
    }

    fn size(&self) -> (i32, i32) {
        let (mut w, mut h) = (0, 0);
        // SAFETY: live surface; `w` is a valid out-pointer.
        unsafe { eglQuerySurface(self.d, self.surf, EGL_WIDTH, &raw mut w) };
        // SAFETY: live surface; `h` is a valid out-pointer.
        unsafe { eglQuerySurface(self.d, self.surf, EGL_HEIGHT, &raw mut h) };
        (w, h)
    }

    fn swap(&self) {
        // SAFETY: live surface, current on this thread.
        unsafe { eglSwapBuffers(self.d, self.surf) };
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let null = std::ptr::null_mut();
        // SAFETY: releases the current context of this thread; always valid.
        unsafe { eglMakeCurrent(self.d, null, null, null) };
        // SAFETY: created in Target::new, destroyed once.
        unsafe { eglDestroySurface(self.d, self.surf) };
        // SAFETY: created in Target::new, destroyed once.
        unsafe { eglDestroyContext(self.d, self.ctx) };
        // Do not eglTerminate: the display connection is process-wide and cheap to keep.
    }
}

// Safe wrappers for the GL calls used here. They take plain values; without a current context they only
// set a GL error, so they cannot cause UB.

fn gl_clear_color(rgb: [f32; 3]) {
    // SAFETY: value arguments only (see above).
    unsafe { glClearColor(rgb[0], rgb[1], rgb[2], 1.0) };
}

fn gl_clear() {
    // SAFETY: value arguments only.
    unsafe { glClear(GL_COLOR_BUFFER_BIT) };
}

#[derive(Clone, Copy, Debug)]
enum Scissor {
    On,
    Off,
}

fn gl_scissor_test(s: Scissor) {
    match s {
        // SAFETY: value arguments only.
        Scissor::On => unsafe { glEnable(GL_SCISSOR_TEST) },
        // SAFETY: value arguments only.
        Scissor::Off => unsafe { glDisable(GL_SCISSOR_TEST) },
    }
}

fn gl_scissor(x: i32, y: i32, w: i32, h: i32) {
    // SAFETY: value arguments only.
    unsafe { glScissor(x, y, w, h) };
}

fn gl_viewport(w: i32, h: i32) {
    // SAFETY: value arguments only.
    unsafe { glViewport(0, 0, w, h) };
}

/// Draw the idle screen once. `win` must not be connected to another producer (no decoder).
///
/// # Errors
///
/// EGL setup failed (the message names the call and the EGL error).
pub(crate) fn draw(win: &NativeWindow, lines: &[Line<'_>]) -> Result<(), String> {
    let target = Target::new(win)?;
    let (w, h) = target.size();
    gl_viewport(w, h);
    gl_scissor_test(Scissor::Off);
    gl_clear_color([0.06, 0.07, 0.10]);
    gl_clear();
    gl_scissor_test(Scissor::On);

    // Vertically centered block of lines; each line is 7 dots high plus 4 dots of spacing.
    let total_h: i32 = lines.iter().map(|l| l.scale * 11).sum();
    let mut y_top = (h - total_h) / 2;
    for line in lines {
        let scale = line.scale;
        let chars = i32::try_from(line.text.chars().count()).unwrap_or(0);
        let x0 = (w - chars * 6 * scale) / 2;
        gl_clear_color(line.rgb);
        for (ci, ch) in (0i32..).zip(line.text.chars()) {
            for (col, bits) in (0i32..).zip(glyph(ch)) {
                for row in 0..7i32 {
                    if bits >> row & 1 == 1 {
                        let px = x0 + (ci * 6 + col) * scale;
                        let py_top = y_top + row * scale;
                        gl_scissor(px, h - py_top - scale, scale, scale);
                        gl_clear();
                    }
                }
            }
        }
        y_top += scale * 11;
    }
    gl_scissor_test(Scissor::Off);
    target.swap();
    Ok(())
}

/// 5x7 font, column-major, bit 0 = top row.
fn glyph(c: char) -> [u8; 5] {
    match c.to_ascii_uppercase() {
        '0' => [0x3E, 0x51, 0x49, 0x45, 0x3E],
        '1' => [0x00, 0x42, 0x7F, 0x40, 0x00],
        '2' => [0x42, 0x61, 0x51, 0x49, 0x46],
        '3' => [0x21, 0x41, 0x45, 0x4B, 0x31],
        '4' => [0x18, 0x14, 0x12, 0x7F, 0x10],
        '5' => [0x27, 0x45, 0x45, 0x45, 0x39],
        '6' => [0x3C, 0x4A, 0x49, 0x49, 0x30],
        '7' => [0x01, 0x71, 0x09, 0x05, 0x03],
        '8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        '9' => [0x06, 0x49, 0x49, 0x29, 0x1E],
        'A' => [0x7E, 0x11, 0x11, 0x11, 0x7E],
        'B' => [0x7F, 0x49, 0x49, 0x49, 0x36],
        'C' => [0x3E, 0x41, 0x41, 0x41, 0x22],
        'D' => [0x7F, 0x41, 0x41, 0x22, 0x1C],
        'E' => [0x7F, 0x49, 0x49, 0x49, 0x41],
        'F' => [0x7F, 0x09, 0x09, 0x09, 0x01],
        'G' => [0x3E, 0x41, 0x49, 0x49, 0x7A],
        'H' => [0x7F, 0x08, 0x08, 0x08, 0x7F],
        'I' => [0x00, 0x41, 0x7F, 0x41, 0x00],
        'J' => [0x20, 0x40, 0x41, 0x3F, 0x01],
        'K' => [0x7F, 0x08, 0x14, 0x22, 0x41],
        'L' => [0x7F, 0x40, 0x40, 0x40, 0x40],
        'M' => [0x7F, 0x02, 0x0C, 0x02, 0x7F],
        'N' => [0x7F, 0x04, 0x08, 0x10, 0x7F],
        'O' => [0x3E, 0x41, 0x41, 0x41, 0x3E],
        'P' => [0x7F, 0x09, 0x09, 0x09, 0x06],
        'Q' => [0x3E, 0x41, 0x51, 0x21, 0x5E],
        'R' => [0x7F, 0x09, 0x19, 0x29, 0x46],
        'S' => [0x46, 0x49, 0x49, 0x49, 0x31],
        'T' => [0x01, 0x01, 0x7F, 0x01, 0x01],
        'U' => [0x3F, 0x40, 0x40, 0x40, 0x3F],
        'V' => [0x1F, 0x20, 0x40, 0x20, 0x1F],
        'W' => [0x3F, 0x40, 0x38, 0x40, 0x3F],
        'X' => [0x63, 0x14, 0x08, 0x14, 0x63],
        'Y' => [0x07, 0x08, 0x70, 0x08, 0x07],
        'Z' => [0x61, 0x51, 0x49, 0x45, 0x43],
        ':' => [0x00, 0x36, 0x36, 0x00, 0x00],
        '.' => [0x00, 0x60, 0x60, 0x00, 0x00],
        '-' => [0x08, 0x08, 0x08, 0x08, 0x08],
        '/' => [0x20, 0x10, 0x08, 0x04, 0x02],
        _ => [0; 5],
    }
}

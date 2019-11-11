// surfman/surfman/src/platform/macos/cgl/context.rs
//
//! Wrapper for Core OpenGL contexts.

use crate::context::{CREATE_CONTEXT_MUTEX, ContextID};
use crate::gl::Gl;
use crate::platform::macos::system::connection::Connection as SystemConnection;
use crate::surface::Framebuffer;
use crate::{ContextAttributeFlags, ContextAttributes, Error, GLVersion, SurfaceInfo};
use super::device::Device;
use super::error::ToWindowingApiError;
use super::surface::Surface;

use cgl::{CGLChoosePixelFormat, CGLContextObj, CGLCreateContext, CGLDescribePixelFormat};
use cgl::{CGLDestroyContext, CGLError, CGLGetCurrentContext, CGLGetPixelFormat};
use cgl::{CGLPixelFormatAttribute, CGLPixelFormatObj, CGLReleasePixelFormat, CGLRetainPixelFormat};
use cgl::{CGLSetCurrentContext, kCGLPFAAllowOfflineRenderers, kCGLPFAAlphaSize, kCGLPFADepthSize};
use cgl::{kCGLPFAStencilSize, kCGLPFAOpenGLProfile};
use core_foundation::base::TCFType;
use core_foundation::bundle::CFBundleGetBundleWithIdentifier;
use core_foundation::bundle::CFBundleGetFunctionPointerForName;
use core_foundation::bundle::CFBundleRef;
use core_foundation::string::CFString;
use std::mem;
use std::os::raw::c_void;
use std::ptr;
use std::str::FromStr;
use std::thread;

// No CGL error occurred.
#[allow(non_upper_case_globals)]
const kCGLNoError: CGLError = 0;

// Choose a renderer compatible with GL 1.0.
#[allow(non_upper_case_globals)]
const kCGLOGLPVersion_Legacy: CGLPixelFormatAttribute = 0x1000;
// Choose a renderer capable of GL3.2 or later.
#[allow(non_upper_case_globals)]
const kCGLOGLPVersion_3_2_Core: CGLPixelFormatAttribute = 0x3200;
// Choose a renderer capable of GL4.1 or later.
#[allow(non_upper_case_globals)]
const kCGLOGLPVersion_GL4_Core: CGLPixelFormatAttribute = 0x4100;

static OPENGL_FRAMEWORK_IDENTIFIER: &'static str = "com.apple.opengl";

thread_local! {
    pub static GL_FUNCTIONS: Gl = Gl::load_with(get_proc_address);
}

thread_local! {
    static OPENGL_FRAMEWORK: CFBundleRef = {
        unsafe {
            let framework_identifier: CFString =
                FromStr::from_str(OPENGL_FRAMEWORK_IDENTIFIER).unwrap();
            let framework =
                CFBundleGetBundleWithIdentifier(framework_identifier.as_concrete_TypeRef());
            assert!(!framework.is_null());
            framework
        }
    };
}

/// An OpenGL context on macOS.
/// 
/// OpenGL contexts must be explicitly destroyed with `Device::destroy_context()`, or a panic
/// occurs.
/// 
/// Contexts are specific to the device that created them and cannot be used with any other device.
/// They are also not thread-safe, just as devices are not.
pub struct Context {
    pub(crate) native_context: Box<dyn NativeContext>,
    pub(crate) id: ContextID,
    framebuffer: Framebuffer<Surface>,
}

pub(crate) trait NativeContext {
    fn cgl_context(&self) -> CGLContextObj;
    fn is_destroyed(&self) -> bool;
    unsafe fn destroy(&mut self);
}

impl Drop for Context {
    #[inline]
    fn drop(&mut self) {
        if !self.native_context.is_destroyed() && !thread::panicking() {
            panic!("Contexts must be destroyed explicitly with `destroy_context`!")
        }
    }
}

/// Options that control OpenGL rendering.
/// 
/// This corresponds to a "pixel format" object in many APIs. These are thread-safe.
pub struct ContextDescriptor {
    cgl_pixel_format: CGLPixelFormatObj,
}

impl Drop for ContextDescriptor {
    // These have been verified to be thread-safe.
    #[inline]
    fn drop(&mut self) {
        unsafe {
            CGLReleasePixelFormat(self.cgl_pixel_format);
        }
    }
}

impl Clone for ContextDescriptor {
    #[inline]
    fn clone(&self) -> ContextDescriptor {
        unsafe {
            ContextDescriptor {
                cgl_pixel_format: CGLRetainPixelFormat(self.cgl_pixel_format),
            }
        }
    }
}

unsafe impl Send for ContextDescriptor {}

impl Device {
    /// Creates an OpenGL context descriptor object from the given set of attributes.
    /// 
    /// This context descriptor can be later used to create an OpenGL context for rendering.
    pub fn create_context_descriptor(&self, attributes: &ContextAttributes)
                                     -> Result<ContextDescriptor, Error> {
        let profile = if attributes.version.major >= 4 {
            kCGLOGLPVersion_GL4_Core
        } else if attributes.version.major == 3 {
            kCGLOGLPVersion_3_2_Core
        } else {
            kCGLOGLPVersion_Legacy
        };

        let flags = attributes.flags;
        let alpha_size   = if flags.contains(ContextAttributeFlags::ALPHA)   { 8  } else { 0 };
        let depth_size   = if flags.contains(ContextAttributeFlags::DEPTH)   { 24 } else { 0 };
        let stencil_size = if flags.contains(ContextAttributeFlags::STENCIL) { 8  } else { 0 };

        let mut cgl_pixel_format_attributes = vec![
            kCGLPFAOpenGLProfile, profile,
            kCGLPFAAlphaSize,     alpha_size,
            kCGLPFADepthSize,     depth_size,
            kCGLPFAStencilSize,   stencil_size,
        ];

        // This means "opt into the integrated GPU".
        //
        // https://supermegaultragroovy.com/2016/12/10/auto-graphics-switching/
        if self.adapter().0.is_low_power {
            cgl_pixel_format_attributes.push(kCGLPFAAllowOfflineRenderers);
        }

        cgl_pixel_format_attributes.extend_from_slice(&[0, 0]);

        unsafe {
            let (mut cgl_pixel_format, mut cgl_pixel_format_count) = (ptr::null_mut(), 0);
            let err = CGLChoosePixelFormat(cgl_pixel_format_attributes.as_ptr(),
                                           &mut cgl_pixel_format,
                                           &mut cgl_pixel_format_count);
            if err != kCGLNoError {
                return Err(Error::PixelFormatSelectionFailed(err.to_windowing_api_error()));
            }
            if cgl_pixel_format_count == 0 {
                return Err(Error::NoPixelFormatFound);
            }

            Ok(ContextDescriptor { cgl_pixel_format })
        }
    }

    /// Opens the device and OpenGL context corresponding to the current CGL context.
    ///
    /// The native context is not retained, as there is no way to do this in the CGL API. It is the
    /// caller's responsibility to keep it alive for the lifetime of the resulting context.
    ///
    /// This method is designed to allow `surfman` to deal with contexts created outside the
    /// library; for example, by Glutin. It's legal to use this method to wrap a context rendering
    /// to any target: either a window or a pbuffer. The target is opaque to `surfman`; the library
    /// will not modify or try to detect the render target. This means that any of the methods that
    /// query or modify the surface—e.g. `bind_surface_to_context`—will fail if called with a
    /// context object created via this method.
    pub unsafe fn from_current_context() -> Result<(Device, Context), Error> {
        let mut next_context_id = CREATE_CONTEXT_MUTEX.lock().unwrap();

        // Create the context.
        let context = Context {
            native_context: Box::new(UnsafeCGLContextRef::current()),
            id: *next_context_id,
            framebuffer: Framebuffer::External,
        };
        next_context_id.0 += 1;

        // Create the device.
        let connection = SystemConnection::new()?;
        let adapter = connection.create_adapter()?;
        let device = connection.create_device(&adapter)?;

        Ok((Device(device), context))
    }

    /// Creates an OpenGL context from the given descriptor.
    /// 
    /// The context must be destroyed with `Device::destroy_context()` when it is no longer needed,
    /// or a panic will occur.
    /// 
    /// The context will be local to this device and cannot be used with any other.
    pub fn create_context(&mut self, descriptor: &ContextDescriptor) -> Result<Context, Error> {
        // Take a lock so that we're only creating one context at a time. This serves two purposes:
        //
        // 1. CGLChoosePixelFormat fails, returning `kCGLBadConnection`, if multiple threads try to
        //    open a display connection simultaneously.
        // 2. The first thread to create a context needs to load the GL function pointers.
        let mut next_context_id = CREATE_CONTEXT_MUTEX.lock().unwrap();

        unsafe {
            // Create the CGL context.
            let mut cgl_context = ptr::null_mut();
            let err = CGLCreateContext(descriptor.cgl_pixel_format,
                                       ptr::null_mut(),
                                       &mut cgl_context);
            if err != kCGLNoError {
                return Err(Error::ContextCreationFailed(err.to_windowing_api_error()));
            }

            debug_assert_ne!(cgl_context, ptr::null_mut());
            let native_context = Box::new(OwnedCGLContext { cgl_context });

            let err = CGLSetCurrentContext(native_context.cgl_context());
            if err != kCGLNoError {
                return Err(Error::MakeCurrentFailed(err.to_windowing_api_error()));
            }

            // Wrap and return the context.
            let context = Context {
                native_context,
                id: *next_context_id,
                framebuffer: Framebuffer::None,
            };
            next_context_id.0 += 1;
            Ok(context)
        }
    }

    /// Destroys an OpenGL context.
    pub fn destroy_context(&self, context: &mut Context) -> Result<(), Error> {
        if context.native_context.is_destroyed() {
            return Ok(());
        }

        if let Framebuffer::Surface(surface) = mem::replace(&mut context.framebuffer,
                                                            Framebuffer::None) {
            self.destroy_surface(context, surface)?;
        }

        unsafe {
            context.native_context.destroy();
        }

        Ok(())
    }

    /// Returns the descriptor that the context was created with.
    #[inline]
    pub fn context_descriptor(&self, context: &Context) -> ContextDescriptor {
        unsafe {
            let mut cgl_pixel_format = CGLGetPixelFormat(context.native_context.cgl_context());
            cgl_pixel_format = CGLRetainPixelFormat(cgl_pixel_format);
            ContextDescriptor { cgl_pixel_format }
        }
    }

    /// Makes the context the current rendering context for this thread.
    /// 
    /// After calling this method, OpenGL rendering commands will render via this context to the
    /// currently-bound surface.
    pub fn make_context_current(&self, context: &Context) -> Result<(), Error> {
        unsafe {
            let err = CGLSetCurrentContext(context.native_context.cgl_context());
            if err != kCGLNoError {
                return Err(Error::MakeCurrentFailed(err.to_windowing_api_error()));
            }
            Ok(())
        }
    }

    /// Makes this thread have no current rendering context.
    /// 
    /// You should not call OpenGL rendering commands after calling this method until another
    /// context becomes current.
    pub fn make_no_context_current(&self) -> Result<(), Error> {
        unsafe {
            let err = CGLSetCurrentContext(ptr::null_mut());
            if err != kCGLNoError {
                return Err(Error::MakeCurrentFailed(err.to_windowing_api_error()));
            }
            Ok(())
        }
    }

    pub(crate) fn temporarily_make_context_current(&self, context: &Context)
                                                   -> Result<CurrentContextGuard, Error> {
        let guard = CurrentContextGuard::new();
        self.make_context_current(context)?;
        Ok(guard)
    }

    /// Attaches a surface to this context.
    /// 
    /// Once this context becomes current, OpenGL rendering commands will render to the surface.
    pub fn bind_surface_to_context(&self, context: &mut Context, new_surface: Surface)
                                   -> Result<(), Error> {
        match context.framebuffer {
            Framebuffer::External => return Err(Error::ExternalRenderTarget),
            Framebuffer::Surface(_) => return Err(Error::SurfaceAlreadyBound),
            Framebuffer::None => {}
        }

        if new_surface.context_id != context.id {
            return Err(Error::IncompatibleSurface);
        }

        context.framebuffer = Framebuffer::Surface(new_surface);
        Ok(())
    }

    /// Removes any attached surface from this context and returns it.
    /// 
    /// Once you call this method, OpenGL rendering commands will fail until a new surface is
    /// attached. (You can still use OpenGL commands that don't render to the default framebuffer,
    /// though, as long as the context is current.)
    pub fn unbind_surface_from_context(&self, context: &mut Context)
                                       -> Result<Option<Surface>, Error> {
        match context.framebuffer {
            Framebuffer::External => return Err(Error::ExternalRenderTarget),
            Framebuffer::None | Framebuffer::Surface(_) => {}
        }

        match mem::replace(&mut context.framebuffer, Framebuffer::None) {
            Framebuffer::External => unreachable!(),
            Framebuffer::None => Ok(None),
            Framebuffer::Surface(surface) => {
                // Make sure all changes are synchronized. Apple requires this.
                //
                // TODO(pcwalton): Use `glClientWaitSync` instead to avoid starving the window
                // server.
                GL_FUNCTIONS.with(|gl| {
                    let _guard = self.temporarily_make_context_current(context)?;
                    unsafe {
                        gl.Flush();
                    }
                    Ok(Some(surface))
                })
            }
        }
    }

    /// Returns the attributes that the given context descriptor represents.
    pub fn context_descriptor_attributes(&self, context_descriptor: &ContextDescriptor)
                                         -> ContextAttributes {
        unsafe {
            let alpha_size = get_pixel_format_attribute(context_descriptor, kCGLPFAAlphaSize);
            let depth_size = get_pixel_format_attribute(context_descriptor, kCGLPFADepthSize);
            let stencil_size = get_pixel_format_attribute(context_descriptor, kCGLPFAStencilSize);
            let gl_profile = get_pixel_format_attribute(context_descriptor, kCGLPFAOpenGLProfile);

            let mut attribute_flags = ContextAttributeFlags::empty();
            attribute_flags.set(ContextAttributeFlags::ALPHA, alpha_size != 0);
            attribute_flags.set(ContextAttributeFlags::DEPTH, depth_size != 0);
            attribute_flags.set(ContextAttributeFlags::STENCIL, stencil_size != 0);

            let version = GLVersion::new(((gl_profile >> 12) & 0xf) as u8,
                                        ((gl_profile >> 8) & 0xf) as u8);

            return ContextAttributes { flags: attribute_flags, version };
        }

        unsafe fn get_pixel_format_attribute(context_descriptor: &ContextDescriptor,
                                             attribute: CGLPixelFormatAttribute)
                                             -> i32 {
            let mut value = 0;
            let err = CGLDescribePixelFormat(context_descriptor.cgl_pixel_format,
                                             0,
                                             attribute,
                                             &mut value);
            debug_assert_eq!(err, kCGLNoError);
            value
        }
    }

    /// Fetches the implementation address of an OpenGL symbol for this context.
    /// 
    /// The symbol name should include the `gl` prefix, if any. OpenGL symbols are local to a
    /// context and should be reloaded if switching contexts.
    #[inline]
    pub fn get_proc_address(&self, _: &Context, symbol_name: &str) -> *const c_void {
        get_proc_address(symbol_name)
    }

    /// Retrieves various information about the surface currently attached to this context, if any.
    pub fn context_surface_info(&self, context: &Context) -> Result<Option<SurfaceInfo>, Error> {
        match context.framebuffer {
            Framebuffer::None => Ok(None),
            Framebuffer::External => Err(Error::ExternalRenderTarget),
            Framebuffer::Surface(ref surface) => Ok(Some(self.surface_info(surface))),
        }
    }

    /// Returns an ID that refers to this context.
    /// 
    /// Context IDs are unique to all currently-live contexts. If a context is destroyed, then
    /// subsequently-created contexts may reuse the same ID.
    #[inline]
    pub fn context_id(&self, context: &Context) -> ContextID {
        context.id
    }
}

struct OwnedCGLContext {
    cgl_context: CGLContextObj,
}

impl NativeContext for OwnedCGLContext {
    #[inline]
    fn cgl_context(&self) -> CGLContextObj {
        debug_assert!(!self.is_destroyed());
        self.cgl_context
    }

    #[inline]
    fn is_destroyed(&self) -> bool {
        self.cgl_context.is_null()
    }

    unsafe fn destroy(&mut self) {
        assert!(!self.is_destroyed());
        CGLSetCurrentContext(ptr::null_mut());
        CGLDestroyContext(self.cgl_context);
        self.cgl_context = ptr::null_mut();
    }
}

struct UnsafeCGLContextRef {
    cgl_context: CGLContextObj,
}

impl UnsafeCGLContextRef {
    #[inline]
    unsafe fn current() -> UnsafeCGLContextRef {
        let cgl_context = CGLGetCurrentContext();
        assert!(!cgl_context.is_null());
        UnsafeCGLContextRef { cgl_context }
    }
}

impl NativeContext for UnsafeCGLContextRef {
    #[inline]
    fn cgl_context(&self) -> CGLContextObj {
        self.cgl_context
    }

    #[inline]
    fn is_destroyed(&self) -> bool {
        self.cgl_context.is_null()
    }

    unsafe fn destroy(&mut self) {
        assert!(!self.is_destroyed());
        self.cgl_context = ptr::null_mut();
    }
}

fn get_proc_address(symbol_name: &str) -> *const c_void {
    OPENGL_FRAMEWORK.with(|framework| {
        unsafe {
            let symbol_name: CFString = FromStr::from_str(symbol_name).unwrap();
            CFBundleGetFunctionPointerForName(*framework, symbol_name.as_concrete_TypeRef())
        }
    })
}

#[must_use]
pub(crate) struct CurrentContextGuard {
    old_cgl_context: CGLContextObj,
}

impl Drop for CurrentContextGuard {
    fn drop(&mut self) {
        unsafe {
            CGLSetCurrentContext(self.old_cgl_context);
        }
    }
}

impl CurrentContextGuard {
    fn new() -> CurrentContextGuard {
        unsafe {
            CurrentContextGuard { old_cgl_context: CGLGetCurrentContext() }
        }
    }
}
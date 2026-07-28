//! Safe wrappers over libghostty-vt.
//!
//! Each type owns one C handle and frees it on drop. Nothing here spawns
//! processes or draws: this layer is purely terminal state plus encoders.

mod render;
mod terminal;

pub mod keys;
pub use keys::KeyEncoder;
pub use render::RenderState;
pub use terminal::{Terminal, TerminalCallbacks};

use crate::sys;
use crate::{Error, Result};

/// Convert a `GhosttyResult` into a `Result`, naming the call for diagnostics.
pub(crate) fn check(call: &'static str, result: sys::GhosttyResult) -> Result<()> {
    if result == sys::GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(Error::Vt { call, code: result.0 })
    }
}

/// Several libghostty structs are versioned by a leading `size` field, which
/// the C headers fill via `GHOSTTY_INIT_SIZED`. Rust has no such macro, so
/// zero the struct and stamp the size before every read.
pub(crate) fn sized<T>() -> T {
    let mut value: T = unsafe { std::mem::zeroed() };
    // SAFETY: every sized struct in this API starts with `size: usize`, which
    // is what the header's GHOSTTY_INIT_SIZED macro writes.
    unsafe {
        let ptr = std::ptr::addr_of_mut!(value) as *mut usize;
        ptr.write(std::mem::size_of::<T>());
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sized_stamps_the_leading_size_field() {
        let style: sys::GhosttyStyle = sized();
        assert_eq!(style.size, std::mem::size_of::<sys::GhosttyStyle>());
        let colors: sys::GhosttyRenderStateColors = sized();
        assert_eq!(colors.size, std::mem::size_of::<sys::GhosttyRenderStateColors>());
    }

    #[test]
    fn check_maps_success_and_failure() {
        assert!(check("x", sys::GHOSTTY_SUCCESS).is_ok());
        let err = check("x", sys::GhosttyResult(-3)).unwrap_err();
        assert!(matches!(err, Error::Vt { code: -3, .. }));
    }
}

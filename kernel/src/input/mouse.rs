//! Normalized mouse codes for UInputL.
//!
//! Following the evdev numbering so the numbers stay familiar: axes are the
//! `REL_*` constants, buttons the `BTN_*` constants.  A mouse driver submits
//! `InputEvent`s with `InputType::Mouse` and one of these codes; `value` is
//! a signed delta for axes (movement since the last report) and 1/0 for
//! buttons.

/// Relative X movement (signed delta).
pub const REL_X: u32 = 0;
/// Relative Y movement (signed delta).
pub const REL_Y: u32 = 1;
/// Vertical wheel (signed delta, up is positive).
pub const REL_WHEEL: u32 = 8;

/// Left mouse button.
pub const BTN_LEFT: u32 = 0x110;
/// Right mouse button.
pub const BTN_RIGHT: u32 = 0x111;
/// Middle mouse button.
pub const BTN_MIDDLE: u32 = 0x112;

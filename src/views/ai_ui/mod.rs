//! The look of the AI interface (the AI section's tasks and the terminal's
//! copilot): Markdown with highlighted code blocks, tool cards, status
//! icons and colors, filters, skeletons and banners. The parts without a
//! window ([`markdown`], [`shell`], [`status`]) are tested on their own.

pub mod markdown;
pub mod shell;
pub mod status;
mod widgets;

pub use widgets::*;

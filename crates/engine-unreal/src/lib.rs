//! Unreal Engine 5 adapter: compresses raw IoStore containers.

pub mod adapter;
pub mod detect;
pub mod toc;
pub mod writer;

pub use adapter::UnrealAdapter;
pub use writer::UnrealIoStore;

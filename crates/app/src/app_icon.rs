//! Kuery's embedded identity, shared by the interface and desktop platforms.

use gpui_kit::WindowOptions;
use gpui_kit::component::TitleBar;

pub const PATH: &str = "branding/kuery.png";
pub const PNG: &[u8] = include_bytes!("../assets/branding/kuery.png");

/// Sets the Dock icon even when the app runs through `cargo run`.
#[cfg(target_os = "macos")]
pub fn init() {
    use objc2::{AllocAnyThread as _, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let main_thread = MainThreadMarker::new().expect("initialize the icon on the main thread");
    let data = NSData::with_bytes(PNG);
    let image = NSImage::initWithData(NSImage::alloc(), &data).expect("decode the Kuery icon");
    // SAFETY: called on the main thread with a valid, non-null image.
    unsafe {
        NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&image));
    }
}

#[cfg(not(target_os = "macos"))]
pub fn init() {}

pub fn window_options() -> WindowOptions {
    WindowOptions {
        app_id: Some("kuery".into()),
        #[cfg(target_os = "linux")]
        icon: Some({
            use std::sync::{Arc, OnceLock};

            static ICON: OnceLock<Arc<image::RgbaImage>> = OnceLock::new();
            ICON.get_or_init(|| {
                Arc::new(
                    image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
                        .expect("decode the Kuery icon")
                        .into_rgba8(),
                )
            })
            .clone()
        }),
        ..TitleBar::window_options()
    }
}

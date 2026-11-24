//! Tests of the functions that change the process environment.
//!
//! `main` calls them before any thread exists, because C libraries read the
//! environment without a lock. They live in this binary of their own, with a
//! single test, so the environment they change is not read by any other test
//! while they run (#397).

use xearthlayer::system::{disown_display, exclude_implicit_vulkan_layers};

const LAYERS: &str = "VK_LOADER_LAYERS_DISABLE";

#[test]
fn process_environment_is_prepared_for_a_headless_gpu_process() {
    // #358: the Vulkan loader's device-select layer and the NVIDIA ICD open
    // the display at instance creation whatever backends wgpu asks for. With
    // no display named there is nothing to connect to.
    std::env::set_var("DISPLAY", ":9");
    std::env::set_var("WAYLAND_DISPLAY", "wayland-9");
    disown_display();
    assert_eq!(std::env::var_os("DISPLAY"), None);
    assert_eq!(std::env::var_os("WAYLAND_DISPLAY"), None);

    // #397: no implicit layer (MangoHud and other overlays) is loaded into
    // the process.
    std::env::remove_var(LAYERS);
    exclude_implicit_vulkan_layers();
    assert_eq!(std::env::var(LAYERS).as_deref(), Ok("~implicit~"));

    // A value the user set is theirs.
    std::env::set_var(LAYERS, "VK_LAYER_KHRONOS_validation");
    exclude_implicit_vulkan_layers();
    assert_eq!(
        std::env::var(LAYERS).as_deref(),
        Ok("VK_LAYER_KHRONOS_validation")
    );
}

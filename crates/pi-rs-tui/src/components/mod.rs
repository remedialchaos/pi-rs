// The built-in components, ported from `packages/tui/src/components/`.

pub mod alt_screen_flash;
pub mod r#box;
pub mod cancellable_loader;
pub mod container;
pub mod editor;
pub mod h_stack;
pub mod image;
pub mod input;
pub mod loader;
pub mod markdown;
pub mod mouse_region;
pub mod scroll_view;
pub mod select_list;
pub mod settings_list;
pub mod spacer;
pub mod stack;
pub mod text;
pub mod truncated_text;
pub mod v_stack;

pub type TextStyleFn = std::rc::Rc<dyn Fn(&str) -> String>;

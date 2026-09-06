//! Dock panels. Each panel (collections / tags / asset grid / inspector)
//! lives in its own file; shared rendering helpers are in `common`.

macro_rules! panel {
    ($name:ident, $title:literal) => {
        pub struct $name {
            focus_handle: FocusHandle,
            controller: Entity<LibraryController>,
        }

        impl BasePanel for $name {
            fn panel_name(&self) -> &'static str {
                stringify!($name)
            }
            fn closable(&self, _: &App) -> bool {
                false
            }
        }

        impl DockPanel for $name {
            fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                $title
            }

            fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
                None
            }
        }

        impl EventEmitter<PanelEvent> for $name {}

        impl Focusable for $name {
            fn focus_handle(&self, _: &App) -> FocusHandle {
                self.focus_handle.clone()
            }
        }
    };
}

pub mod common;
mod explorer;
mod inspector;
mod tags_panel;
mod workspace;

pub use explorer::ExplorerPanel;
pub use inspector::InspectorPanel;
pub use tags_panel::TagsPanel;
pub use workspace::WorkspacePanel;

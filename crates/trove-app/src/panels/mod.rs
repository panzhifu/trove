//! Dock panels. Each panel (collections / tags / asset grid / inspector)
//! lives in its own file; shared rendering helpers are in `common`.

/// Declare a dock panel with the two fields every panel shares.
///
/// Panels that need to cache something read out of the store (because `render`
/// runs every frame and the read is not free) pass those fields as a third
/// argument. They stay private to the panel's own module, so the panel is the
/// only thing that can touch them:
///
/// ```ignore
/// panel!(FoldersPanel, title, rows: Option<(u64, Vec<(String, u64)>)>);
/// ```
///
/// A panel declared this way must also write
/// `fn title_controls(&self, cx: &mut Context<Self>) -> Option<Div>`: the
/// macro forwards the dock's `title_suffix` to it, so what each title bar
/// carries besides its name stays the panel's own business (see
/// [`search_box`], which every panel puts there).
macro_rules! panel {
    ($name:ident, $title:expr $(, $extra_field:ident: $extra_ty:ty)*) => {
        pub struct $name {
            focus_handle: FocusHandle,
            controller: Entity<LibraryController>,
            $($extra_field: $extra_ty,)*
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

            /// Delegated to the panel's own `title_controls`, which is where a
            /// panel says what its title bar carries besides the name — a
            /// search box, an add button, nothing. The macro has no opinion,
            /// so it cannot impose one on a panel that wants something else.
            fn title_suffix(
                &mut self,
                _: &mut Window,
                cx: &mut Context<Self>,
            ) -> Option<impl IntoElement> {
                self.title_controls(cx)
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
mod folders;
mod inspector;
pub mod search_box;
mod tags_panel;
pub mod workspace;
mod workspace_search;

pub use explorer::ExplorerPanel;
pub use folders::FoldersPanel;
pub use inspector::InspectorPanel;
pub use tags_panel::TagsPanel;
pub use workspace::WorkspacePanel;

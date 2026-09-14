//! The workspace panel's toolbar surfaces: the dock title bar (title +
//! suffix), the in-panel filter tools, and the floating batch-action bar.

mod filters;
mod selection;
mod title;

pub(crate) use filters::{
    add_filter_button, format_filter, kind_filter, kind_key, rating_filter, shape_filter,
    tag_filter, title_controls,
};
pub(crate) use selection::selection_toolbar;

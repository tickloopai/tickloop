use super::{
    console::wrap_text,
    conversation_picker::ellipsize,
    style::{Role, RoleLine},
};

pub(super) fn clipped(text: &str, width: usize, role: Role) -> RoleLine {
    RoleLine::role(ellipsize(&super::safe_line(text), width), role)
}

pub(super) fn wrapped(text: &str, width: usize, role: Role) -> Vec<RoleLine> {
    if width == 0 {
        return Vec::new();
    }
    text.split('\n')
        .flat_map(|line| wrap_text(&super::safe_line(line), width))
        .map(|text| RoleLine::role(ellipsize(&text, width), role))
        .collect()
}

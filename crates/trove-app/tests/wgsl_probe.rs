//! One-off: validate the preview shader with naga, offline.
#[test]
fn preview_shader_validates() {
    let source = include_str!("../src/components/preview/gpu3d.wgsl");
    let module = naga::front::wgsl::parse_str(source).expect("wgsl parses");
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    validator.validate(&module).expect("wgsl validates");
    println!("shader ok");
}

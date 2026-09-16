//! Dev helper: render the font specimen card for a font file to inspect the
//! three-row layout (Latin / CJK / digits). Usage:
//! `cargo run --example font_card_preview -- <font.ttf> [<font2.ttf> ...]`
//!
//! Cards land in `target/tmp/font-card-preview/<n>.jpg`.

fn main() {
    let fonts: Vec<_> = std::env::args().skip(1).collect();
    if fonts.is_empty() {
        eprintln!("usage: font_card_preview <font.ttf> [<font2.ttf> ...]");
        std::process::exit(1);
    }
    let dir = std::path::Path::new("target/tmp/font-card-preview");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    for (i, font) in fonts.iter().enumerate() {
        let sha = format!("{:0>64}", i.to_string());
        match trove_core::media::thumb::ensure(
            dir,
            &sha,
            trove_core::model::AssetKind::Font,
            std::path::Path::new(font),
        ) {
            Some(out) => println!("{font} -> {}", out.display()),
            None => println!("{font} -> not parseable, no card"),
        }
    }
}

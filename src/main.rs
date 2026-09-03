use trove_core::{AssetId, DecodedImage, Library, Rating, Sha256};

fn main() {
    let lib = Library::new("我的素材库".into());
    println!("库: {} ({})", lib.name, lib.id);

    let hash = Sha256::hash(b"trove");
    println!("sha256(\"trove\") = {} (shard {})", hash.to_hex(), hash.shard());

    let rating = Rating::new(5).expect("5 是合法评分");
    println!("rating = {} (rated: {})", rating.get(), rating.is_rated());

    let id = AssetId::new();
    println!("asset id = {id}");

    let img = DecodedImage::new(2, 1, vec![0u8; 8]);
    println!("decoded image valid: {}", img.validate().is_ok());
}

use clap::Parser;
use image::{
    imageops::{resize, FilterType},
    ImageBuffer, Luma,
};
use nasadem::Tile;
use std::path::PathBuf;

#[derive(Debug, Parser)]
struct Cmd {
    /// Source NASADEM hgt file.
    tile: PathBuf,
    /// Write output z-order tile to this directory.
    dest: Option<PathBuf>,
}

fn scale_to_pow2(img: &ImageBuffer<Luma<u16>, Vec<u16>>) -> ImageBuffer<Luma<u16>, Vec<u16>> {
    let (scaled_x, scaled_y) = match img.dimensions() {
        (1201, 1201) => (1024, 1024),
        (3601, 3601) => (2048, 2048),
        other => panic!("dimensions {other:?} are not expected for SRTM data."),
    };
    resize(img, scaled_x, scaled_y, FilterType::Lanczos3)
}

fn main() {
    let cli = Cmd::parse();
    let tile = Tile::load(&cli.tile).unwrap();
    let img = tile.to_image();
    let scaled = scale_to_pow2(&img);
    let out = cli.dest.unwrap_or_else(|| {
        let mut out = cli.tile.clone();
        if out.is_dir() {
            let name = cli.tile.file_name().unwrap();
            out.push(name);
        } else {
            out.set_extension("png");
        }
        out
    });
    scaled.save(out).unwrap();
}

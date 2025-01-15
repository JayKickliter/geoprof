#![allow(dead_code)]

//! NASADEM evelation (`.hgt`) file format.
//!
//! # References
//!
//! 1. [30-Meter SRTM Tile Downloader](https://dwtkns.com/srtm30m)
//! 1. [HGT file layout](https://www.researchgate.net/profile/Pierre-Boulanger-4/publication/228924813/figure/fig8/AS:300852653903880@1448740270695/Description-of-a-HGT-file-structure-The-name-file-in-this-case-is-N20W100HGT.png)
//! 1. [Archive Team](http://fileformats.archiveteam.org/index.php?title=HGT&oldid=17250)
//! 1. [SRTM Collection User Guide](https://lpdaac.usgs.gov/documents/179/SRTM_User_Guide_V3.pdf)

mod error;

pub use crate::error::NasademError;
use byteorder::{BigEndian as BE, ReadBytesExt};
use geo::{
    geometry::{Coord, Polygon},
    polygon,
};
use memmap2::Mmap;
use std::{
    fs::File,
    io::BufReader,
    mem::size_of,
    path::Path,
    sync::atomic::{AtomicI16, Ordering},
};

/// Base floating point type used for all coordinates and calculations.
///
/// Note: this _could_ be a generic parameter, but doing so makes the
/// library more complicated. While f32 vs f64 does make a measurable
/// differnece when walking paths across tiles (see `Profile` type in
/// the `terrain` crate), benchmarking shows that switching NASADEMs
/// to `f32` has no effect.
pub type C = f64;

const ARCSEC_PER_DEG: C = 3600.0;
const HALF_ARCSEC: C = 1.0 / (2.0 * 3600.0);

pub struct Tile {
    /// Southwest corner of the tile.
    ///
    /// Specificlly, the _center_ of the SW most sample of the tile.
    sw_corner_center: Coord<C>,

    /// Northeast corner of the tile.
    ///
    /// Specificlly, the _center_ of the NE most sample of the tile.
    ne_corner_center: Coord<C>,

    /// Arcseconds per sample.
    resolution: u8,

    /// Number of (rows, columns) in this tile.
    dimensions: (usize, usize),

    /// Lowest elevation sample in this tile.
    min_elevation: AtomicI16,

    /// Highest elevation sample in this tile.
    max_elevation: AtomicI16,

    /// Elevation samples.
    samples: SampleStore,
}

enum SampleStore {
    Tombstone,
    InMem(Box<[i16]>),
    MemMap(Mmap),
}

impl SampleStore {
    fn get_unchecked(&self, index: usize) -> i16 {
        match self {
            Self::Tombstone => 0,
            Self::InMem(samples) => samples[index],
            Self::MemMap(raw) => {
                let start = index * size_of::<u16>();
                let end = start + size_of::<u16>();
                let bytes = &mut &raw.as_ref()[start..end];
                bytes.read_i16::<BE>().unwrap()
            }
        }
    }

    /// Returns the lowest elevation sample in this data.
    fn min(&self) -> i16 {
        match self {
            Self::Tombstone => 0,
            Self::InMem(samples) => samples.iter().min().copied().unwrap(),
            Self::MemMap(raw) => (*raw)
                .chunks_exact(2)
                .map(|mut bytes| (&mut bytes).read_i16::<BE>().unwrap())
                .min()
                .unwrap(),
        }
    }

    /// Returns the highest elevation sample in this data.
    pub fn max(&self) -> i16 {
        match self {
            Self::Tombstone => 0,
            Self::InMem(samples) => samples.iter().max().copied().unwrap(),
            Self::MemMap(raw) => (*raw)
                .chunks_exact(2)
                .map(|mut bytes| (&mut bytes).read_i16::<BE>().unwrap())
                .max()
                .unwrap(),
        }
    }
}

impl Tile {
    /// Returns a Tile read into memory from the file at `path`.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, NasademError> {
        let (resolution, dimensions @ (cols, rows)) = extract_resolution(&path)?;
        let sw_corner_center = {
            let Coord { x, y } = parse_sw_corner(&path)?;
            Coord {
                x: C::from(x),
                y: C::from(y),
            }
        };

        #[allow(clippy::cast_precision_loss)]
        let ne_corner_center = Coord {
            y: sw_corner_center.y + (dimensions.0 as C * C::from(resolution)) / ARCSEC_PER_DEG,
            x: sw_corner_center.x + (dimensions.1 as C * C::from(resolution)) / ARCSEC_PER_DEG,
        };

        let mut file = BufReader::new(File::open(path)?);

        let samples = {
            let mut sample_store = Vec::with_capacity(cols * rows);

            for _ in 0..(cols * rows) {
                let sample = file.read_i16::<BE>()?;
                sample_store.push(sample);
            }

            assert_eq!(sample_store.len(), dimensions.0 * dimensions.1);
            SampleStore::InMem(sample_store.into_boxed_slice())
        };

        let min_elevation = i16::MAX.into();
        let max_elevation = i16::MAX.into();

        Ok(Self {
            sw_corner_center,
            ne_corner_center,
            resolution,
            dimensions,
            min_elevation,
            max_elevation,
            samples,
        })
    }

    /// Returns a Tile using the memory-mapped file as storage.
    pub fn memmap<P: AsRef<Path>>(path: P) -> Result<Self, NasademError> {
        let (resolution, dimensions @ (cols, rows)) = extract_resolution(&path)?;
        let sw_corner_center = {
            let Coord { x, y } = parse_sw_corner(&path)?;
            Coord {
                x: C::from(x),
                y: C::from(y),
            }
        };

        #[allow(clippy::cast_precision_loss)]
        let ne_corner_center = Coord {
            y: sw_corner_center.y + (cols as C * C::from(resolution)) / ARCSEC_PER_DEG,
            x: sw_corner_center.x + (rows as C * C::from(resolution)) / ARCSEC_PER_DEG,
        };

        let samples = {
            let file = File::open(path)?;
            let mmap = unsafe { Mmap::map(&file)? };
            SampleStore::MemMap(mmap)
        };

        let min_elevation = i16::MAX.into();
        let max_elevation = i16::MAX.into();

        Ok(Self {
            sw_corner_center,
            ne_corner_center,
            resolution,
            dimensions,
            min_elevation,
            max_elevation,
            samples,
        })
    }

    pub fn tombstone(sw_corner: Coord<i16>) -> Self {
        let sw_corner_center = Coord {
            x: C::from(sw_corner.x),
            y: C::from(sw_corner.y),
        };

        let (resolution, dimensions) = (3, (1201, 1201));

        #[allow(clippy::cast_precision_loss)]
        let ne_corner_center = Coord {
            y: sw_corner_center.y as C + (dimensions.0 as C * C::from(resolution)) / ARCSEC_PER_DEG,
            x: sw_corner_center.x as C + (dimensions.1 as C * C::from(resolution)) / ARCSEC_PER_DEG,
        };

        let samples = SampleStore::Tombstone;
        let min_elevation = i16::MAX.into();
        let max_elevation = i16::MAX.into();

        Self {
            sw_corner_center,
            ne_corner_center,
            resolution,
            dimensions,
            min_elevation,
            max_elevation,
            samples,
        }
    }

    /// Returns this tile's (x, y) dimensions.
    pub fn dimensions(&self) -> (usize, usize) {
        self.dimensions
    }

    /// Returns the number of samples in this tile.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        let (x, y) = self.dimensions();
        x * y
    }

    /// Returns the lowest elevation sample in this tile.
    pub fn min_elevation(&self) -> i16 {
        let mut min_elevation = self.min_elevation.load(Ordering::Relaxed);
        if min_elevation == i16::MAX {
            min_elevation = self.samples.min();
            self.min_elevation.store(min_elevation, Ordering::SeqCst);
        };
        min_elevation
    }

    /// Returns the highest elevation sample in this tile.
    pub fn max_elevation(&self) -> i16 {
        let mut max_elevation = self.max_elevation.load(Ordering::Relaxed);
        if max_elevation == i16::MAX {
            max_elevation = self.samples.max();
            self.max_elevation.store(max_elevation, Ordering::SeqCst);
        };
        max_elevation
    }

    /// Rreturns this tile's resolution in arcseconds per sample.
    pub fn resolution(&self) -> u8 {
        self.resolution
    }

    /// Returns the sample at the given geo coordinates.
    pub fn get(&self, coord: Coord<C>) -> Option<i16> {
        let (idx_x, idx_y) = self.coord_to_xy(coord);
        #[allow(clippy::cast_possible_wrap)]
        if 0 <= idx_x
            && idx_x < self.dimensions().0 as isize
            && 0 <= idx_y
            && idx_y < self.dimensions().1 as isize
        {
            #[allow(clippy::cast_sign_loss)]
            let idx_1d = self.xy_to_linear_index((idx_x as usize, idx_y as usize));
            Some(self.samples.get_unchecked(idx_1d))
        } else {
            None
        }
    }

    /// Returns the sample at the given geo coordinates.
    pub fn get_unchecked(&self, coord: Coord<C>) -> i16 {
        let (idx_x, idx_y) = self.coord_to_xy(coord);
        #[allow(clippy::cast_sign_loss)]
        let idx_1d = self.xy_to_linear_index((idx_x as usize, idx_y as usize));
        self.samples.get_unchecked(idx_1d)
    }

    /// Returns and iterator over `self`'s grid squares.
    pub fn iter(&self) -> impl Iterator<Item = Sample<'_>> + '_ {
        (0..(self.dimensions().0 * self.dimensions().1)).map(|index| Sample { tile: self, index })
    }

    /// Returns this tile's outline as a polygon.
    pub fn polygon(&self) -> Polygon {
        let delta = C::from(self.resolution) * HALF_ARCSEC;
        let n = self.ne_corner_center.y + delta;
        let e = self.sw_corner_center.x + delta;
        let s = self.sw_corner_center.y - delta;
        let w = self.sw_corner_center.x - delta;

        polygon![
            (x: w, y: s),
            (x: e, y: s),
            (x: e, y: n),
            (x: w, y: n),
            (x: w, y: s),
        ]
    }
}

/// Private API
impl Tile {
    fn get_xy(&self, (x, y): (usize, usize)) -> i16 {
        let idx_1d = self.xy_to_linear_index((x, y));
        self.samples.get_unchecked(idx_1d)
    }

    fn coord_to_xy(&self, coord: Coord<C>) -> (isize, isize) {
        let c = ARCSEC_PER_DEG / C::from(self.resolution);
        // TODO: do we need to compensate for cell width. If so, does
        //       the following accomplish that? It seems to in the
        //       Mt. Washington test.
        let sample_center_compensation = 1. / (c * 2.);
        let cc = sample_center_compensation;
        #[allow(clippy::cast_possible_truncation)]
        let x = ((coord.x - self.sw_corner_center.x + cc) * c) as isize;
        #[allow(clippy::cast_possible_truncation)]
        let y = ((coord.y - self.sw_corner_center.y + cc) * c) as isize;
        (x, y)
    }

    fn linear_index_to_xy(&self, idx: usize) -> (usize, usize) {
        let y = idx / self.dimensions().0;
        let x = idx % self.dimensions().1;
        (x, self.dimensions().1 - 1 - y)
    }

    fn xy_to_linear_index(&self, (x, y): (usize, usize)) -> usize {
        self.dimensions().0 * (self.dimensions().1 - y - 1) + x
    }

    fn xy_to_polygon(&self, (x, y): (usize, usize)) -> Polygon<C> {
        #[allow(clippy::cast_precision_loss)]
        let center = Coord {
            x: self.sw_corner_center.x + (x as C * C::from(self.resolution)) / ARCSEC_PER_DEG,
            y: self.sw_corner_center.y + (y as C * C::from(self.resolution)) / ARCSEC_PER_DEG,
        };
        polygon(&center, C::from(self.resolution))
    }
}

/// Generate a `res`-arcsecond square around `center`.
fn polygon(center: &Coord<C>, res: C) -> Polygon<C> {
    let delta = res * HALF_ARCSEC;
    let n = center.y + delta;
    let e = center.x + delta;
    let s = center.y - delta;
    let w = center.x - delta;
    polygon![
        (x: w, y: s),
        (x: e, y: s),
        (x: e, y: n),
        (x: w, y: n),
        (x: w, y: s),
    ]
}

/// A NASADEM elevation sample.
pub struct Sample<'a> {
    /// The parent [Tile] this grid square belongs to.
    tile: &'a Tile,
    /// Index into parent's evelation data corresponding to tbhis grid
    /// square.
    index: usize,
}

impl<'a> Sample<'a> {
    pub fn elevation(&self) -> i16 {
        self.tile.samples.get_unchecked(self.index)
    }

    pub fn polygon(&self) -> Polygon {
        self.tile.xy_to_polygon(self.index())
    }

    pub fn index(&self) -> (usize, usize) {
        self.tile.linear_index_to_xy(self.index)
    }
}

impl<'a> std::cmp::PartialEq for Sample<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && std::ptr::eq(self, other)
    }
}

impl<'a> std::cmp::Eq for Sample<'a> {}

fn extract_resolution<P: AsRef<Path>>(path: P) -> Result<(u8, (usize, usize)), NasademError> {
    const RES_1_ARCSECONDS_FILE_LEN: u64 = 3601 * 3601 * size_of::<u16>() as u64;
    const RES_3_ARCSECONDS_FILE_LEN: u64 = 1201 * 1201 * size_of::<u16>() as u64;
    match path.as_ref().metadata().map(|m| m.len())? {
        RES_1_ARCSECONDS_FILE_LEN => Ok((1, (3601, 3601))),
        RES_3_ARCSECONDS_FILE_LEN => Ok((3, (1201, 1201))),
        invalid_len => Err(NasademError::HgtLen(
            invalid_len,
            path.as_ref().to_path_buf(),
        )),
    }
}

fn parse_sw_corner<P: AsRef<Path>>(path: P) -> Result<Coord<i16>, NasademError> {
    let mk_err = || NasademError::HgtName(path.as_ref().to_owned());
    let name = path
        .as_ref()
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(mk_err)?;
    if name.len() != 7 {
        return Err(mk_err());
    }
    let lat_sign = match &name[0..1] {
        "N" | "n" => 1,
        "S" | "s" => -1,
        _ => return Err(mk_err()),
    };
    let lat = lat_sign * name[1..3].parse::<i16>().map_err(|_| mk_err())?;
    let lon_sign = match &name[3..4] {
        "E" | "e" => 1,
        "W" | "w" => -1,
        _ => return Err(mk_err()),
    };
    let lon = lon_sign * name[4..7].parse::<i16>().map_err(|_| mk_err())?;
    Ok(Coord { x: lon, y: lat })
}

#[cfg(test)]
mod _1_arc_second {
    use super::{
        extract_resolution, parse_sw_corner, BufReader, Coord, File, ReadBytesExt, Tile, BE,
    };
    use std::path::PathBuf;

    fn one_arcsecond_dir() -> PathBuf {
        [
            env!("CARGO_MANIFEST_DIR"),
            "..",
            "data",
            "nasadem",
            "1arcsecond",
        ]
        .iter()
        .collect()
    }

    #[test]
    fn test_parse_hgt_name() {
        let mut path = one_arcsecond_dir();
        path.push("N44W072.hgt");
        let sw_corner = parse_sw_corner(&path).unwrap();
        let resolution = extract_resolution(&path).unwrap();
        assert_eq!(sw_corner, Coord { x: -72, y: 44 });
        assert_eq!(resolution, (1, (3601, 3601)));
    }

    #[test]
    fn test_tile_open() {
        let mut path = one_arcsecond_dir();
        path.push("N44W072.hgt");
        Tile::load(path).unwrap();
    }

    #[test]
    fn test_out_of_bounds_get_returns_none() {
        let mut path = one_arcsecond_dir();
        path.push("N44W072.hgt");
        let tile = Tile::load(path).unwrap();
        // Assert coordinate a smidge north of tile returns None.
        assert_eq!(tile.get(Coord { x: -71.5, y: 45.1 }), None);
        // Assert coordinate a smidge east of tile returns None.
        assert_eq!(tile.get(Coord { x: -70.9, y: 44.5 }), None);
        // Assert coordinate a smidge south of tile returns None.
        assert_eq!(tile.get(Coord { x: -71.5, y: 43.9 }), None);
        // Assert coordinate a smidge west of tile returns None.
        assert_eq!(tile.get(Coord { x: -72.1, y: 44.5 }), None);
    }

    #[test]
    fn test_tile_index() {
        let mut path = one_arcsecond_dir();
        path.push("N44W072.hgt");
        let raw_file_samples = {
            let mut file_data = Vec::new();
            let mut file = BufReader::new(File::open(&path).unwrap());
            while let Ok(sample) = file.read_i16::<BE>() {
                file_data.push(sample);
            }
            file_data
        };
        let parsed_tile = Tile::load(&path).unwrap();
        let mapped_tile = Tile::memmap(&path).unwrap();
        let mut idx = 0;
        for row in (0..3601).rev() {
            for col in 0..3601 {
                assert_eq!(raw_file_samples[idx], parsed_tile.get_xy((col, row)));
                assert_eq!(raw_file_samples[idx], mapped_tile.get_xy((col, row)));
                idx += 1;
            }
        }
    }

    #[test]
    fn test_tile_geo_index() {
        let mut path = one_arcsecond_dir();
        path.push("N44W072.hgt");
        let tile = Tile::load(&path).unwrap();
        let mt_washington = Coord {
            y: 44.2705,
            x: -71.30325,
        };
        assert_eq!(tile.get_unchecked(mt_washington), tile.max_elevation());
    }

    #[test]
    fn test_tile_index_conversions() {
        let mut path = one_arcsecond_dir();
        path.push("N44W072.hgt");
        let tile = Tile::load(&path).unwrap();
        for row in (0..3601).rev() {
            for col in 0..3601 {
                let d1 = tile.xy_to_linear_index((col, row));
                let roundtrip_2d = tile.linear_index_to_xy(d1);
                assert_eq!((col, row), roundtrip_2d);
            }
        }
    }
}

#[cfg(test)]
mod _3_arc_second {
    use super::{
        extract_resolution, parse_sw_corner, BufReader, Coord, File, Polygon, ReadBytesExt, Tile,
        BE,
    };
    use geo::geometry::LineString;
    use std::path::PathBuf;

    fn three_arcsecond_dir() -> PathBuf {
        [
            env!("CARGO_MANIFEST_DIR"),
            "..",
            "data",
            "nasadem",
            "3arcsecond",
        ]
        .iter()
        .collect()
    }

    #[test]
    fn test_parse_hgt_name() {
        let mut path = three_arcsecond_dir();
        path.push("N44W072.hgt");
        let sw_corner = parse_sw_corner(&path).unwrap();
        let resolution = extract_resolution(&path).unwrap();
        assert_eq!(sw_corner, Coord { x: -72, y: 44 });
        assert_eq!(resolution, (3, (1201, 1201)));
    }

    #[test]
    fn test_tile_open() {
        let mut path = three_arcsecond_dir();
        path.push("N44W072.hgt");
        Tile::load(path).unwrap();
    }

    #[test]
    fn test_out_of_bounds_get_returns_none() {
        let mut path = three_arcsecond_dir();
        path.push("N44W072.hgt");
        let tile = Tile::load(path).unwrap();
        // Assert coordinate a smidge north of tile returns None.
        assert_eq!(tile.get(Coord { x: -71.5, y: 45.1 }), None);
        // Assert coordinate a smidge east of tile returns None.
        assert_eq!(tile.get(Coord { x: -70.9, y: 44.5 }), None);
        // Assert coordinate a smidge south of tile returns None.
        assert_eq!(tile.get(Coord { x: -71.5, y: 43.9 }), None);
        // Assert coordinate a smidge west of tile returns None.
        assert_eq!(tile.get(Coord { x: -72.1, y: 44.5 }), None);
    }

    #[test]
    fn test_tile_index() {
        let mut path = three_arcsecond_dir();
        path.push("N44W072.hgt");
        let tile = Tile::load(&path).unwrap();
        let raw_file_samples = {
            let mut file_data = Vec::new();
            let mut raw = BufReader::new(File::open(path).unwrap());
            while let Ok(sample) = raw.read_i16::<BE>() {
                file_data.push(sample);
            }
            file_data
        };
        let mut idx = 0;
        for row in (0..1201).rev() {
            for col in 0..1201 {
                assert_eq!(raw_file_samples[idx], tile.get_xy((col, row)));
                idx += 1;
            }
        }
    }

    // #[test]
    // fn test_tile_geo_index() {
    //     let mut path = three_arcsecond_dir();
    //     path.push("N44W072.hgt");
    //     let tile = Tile::parse(&path).unwrap();
    //     let mt_washington = Coord {
    //         y: 44.2705,
    //         x: -71.30325,
    //     };
    //     // TODO: is there an error in indexing or is the 3 arc-second
    //     //       dataset smeared?
    //     assert_eq!(tile.get_coord(mt_washington), tile.max_elevation());
    // }

    #[test]
    fn test_tile_index_conversions() {
        let mut path = three_arcsecond_dir();
        path.push("N44W072.hgt");
        let parsed_tile = Tile::load(&path).unwrap();
        for row in (0..1201).rev() {
            for col in 0..1201 {
                let d1 = parsed_tile.xy_to_linear_index((col, row));
                let roundtrip_2d = parsed_tile.linear_index_to_xy(d1);
                assert_eq!((col, row), roundtrip_2d);
            }
        }
    }

    #[test]
    fn test_xy_to_polygon() {
        let mut path = three_arcsecond_dir();
        path.push("N44W072.hgt");
        let parsed_tile = Tile::load(&path).unwrap();
        assert_eq!(
            parsed_tile.xy_to_polygon((0, 0)),
            Polygon::new(
                LineString::from(vec![
                    (-72.000_416_666_666_67, 43.999_583_333_333_334),
                    (-71.999_583_333_333_33, 43.999_583_333_333_334),
                    (-71.999_583_333_333_33, 44.000_416_666_666_666),
                    (-72.000_416_666_666_67, 44.000_416_666_666_666),
                    (-72.000_416_666_666_67, 43.999_583_333_333_334),
                ]),
                vec![],
            )
        );
    }
}

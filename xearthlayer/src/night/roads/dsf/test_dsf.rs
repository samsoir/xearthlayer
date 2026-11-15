//! Builds minimal DSFs: HEAD/PROP, DEFN/NETW, GEOD/PO32+SC32 (raw
//! encoding unless asked), CMDS. The MD5 footer is 16 zero bytes (the
//! reader does not check it, as X-Plane's loader is the authority).
pub struct Builder {
    pub props: Vec<(String, String)>,
    pub nets: Vec<String>,
    /// Pools: planes of raw u32 values, their (scale, offset) and the
    /// plane encoding.
    pub pools: Vec<(Vec<Vec<u32>>, Vec<(f32, f32)>, u8)>,
    pub cmds: Vec<u8>,
    /// GEOD children written before the PO32/SC32 pairs (16-bit POOL
    /// and SCAL in real meshes).
    pub geod_extra: Vec<([u8; 4], Vec<u8>)>,
    /// Top-level atoms written after CMDS (DEMS in real meshes).
    pub trailing: Vec<([u8; 4], Vec<u8>)>,
}

pub fn atom(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::from_be_bytes(*id).to_le_bytes().to_vec();
    out.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn strings(s: &[String]) -> Vec<u8> {
    s.iter().flat_map(|x| x.bytes().chain([0])).collect()
}

fn encode_plane(v: &[u32], enc: u8) -> Vec<u8> {
    let mut vals = v.to_vec();
    if enc == 1 || enc == 3 {
        let mut prev = 0u32;
        for x in vals.iter_mut() {
            let d = x.wrapping_sub(prev);
            prev = *x;
            *x = d;
        }
    }
    let mut out = vec![enc];
    if enc == 0 || enc == 1 {
        for x in vals {
            out.extend_from_slice(&x.to_le_bytes());
        }
    } else {
        // Literal runs only keep the builder simple; repeat runs are
        // tested on the plane reader directly.
        for chunk in vals.chunks(127) {
            out.push(chunk.len() as u8);
            for x in chunk {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
    }
    out
}

impl Builder {
    pub fn new(lat: i32, lon: i32) -> Self {
        Self {
            props: vec![
                ("sim/west".into(), lon.to_string()),
                ("sim/south".into(), lat.to_string()),
                ("sim/east".into(), (lon + 1).to_string()),
                ("sim/north".into(), (lat + 1).to_string()),
            ],
            nets: vec![],
            pools: vec![],
            cmds: vec![],
            geod_extra: vec![],
            trailing: vec![],
        }
    }

    /// A pool of points (lon, lat, junction) as 4 planes with exact
    /// degree scaling over the tile: scale 1, offset = corner.
    pub fn pool(&mut self, lat: i32, lon: i32, pts: &[(f64, f64, u32)], enc: u8) -> u16 {
        let q = |v: f64, o: i32| (((v - f64::from(o)) * 4294967295.0).round()) as u32;
        let planes = vec![
            pts.iter().map(|p| q(p.0, lon)).collect(),
            pts.iter().map(|p| q(p.1, lat)).collect(),
            pts.iter().map(|_| 0u32).collect(),
            pts.iter().map(|p| p.2).collect(),
        ];
        let scales = vec![(1.0, lon as f32), (1.0, lat as f32), (0.0, 0.0), (0.0, 0.0)];
        self.pools.push((planes, scales, enc));
        (self.pools.len() - 1) as u16
    }

    /// One road chain of `subtype` through absolute `(lon, lat)` points,
    /// junctions at both ends, in its own raw-encoded pool. Sets the
    /// network definition. Used by Tasks 9 and 10.
    pub fn road(&mut self, lat: i32, lon: i32, subtype: u8, pts: &[(f64, f64)]) -> &mut Self {
        let n = pts.len();
        let with_j: Vec<(f64, f64, u32)> = pts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let j = if i == 0 || i + 1 == n {
                    (i + 1) as u32
                } else {
                    0
                };
                (p.0, p.1, j)
            })
            .collect();
        let pool = self.pool(lat, lon, &with_j, 0);
        self.nets = vec!["lib/g10/roads.net".into()];
        self.cmds.push(1);
        self.cmds.extend(pool.to_le_bytes());
        self.cmds.push(6);
        self.cmds.push(subtype);
        self.cmds.push(9);
        self.cmds.push(n as u8);
        for i in 0..n as u16 {
            self.cmds.extend(i.to_le_bytes());
        }
        self
    }

    /// The 10 degree block folder of a tile: `+30-130` for (37, -123).
    pub fn block(lat: i32, lon: i32) -> String {
        crate::night::store::block_name(lat, lon)
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut out = b"XPLNEDSF".to_vec();
        out.extend_from_slice(&1u32.to_le_bytes());
        let props: Vec<String> = self
            .props
            .iter()
            .flat_map(|(k, v)| [k.clone(), v.clone()])
            .collect();
        out.extend(atom(b"HEAD", &atom(b"PROP", &strings(&props))));
        out.extend(atom(b"DEFN", &atom(b"NETW", &strings(&self.nets))));
        let mut geod = vec![];
        for (id, body) in &self.geod_extra {
            geod.extend(atom(id, body));
        }
        for (planes, scales, enc) in &self.pools {
            let count = planes.first().map_or(0, Vec::len) as u32;
            let mut po = count.to_le_bytes().to_vec();
            po.push(planes.len() as u8);
            for p in planes {
                po.extend(encode_plane(p, *enc));
            }
            geod.extend(atom(b"PO32", &po));
            let sc: Vec<u8> = scales
                .iter()
                .flat_map(|(s, o)| s.to_le_bytes().into_iter().chain(o.to_le_bytes()))
                .collect();
            geod.extend(atom(b"SC32", &sc));
        }
        out.extend(atom(b"GEOD", &geod));
        out.extend(atom(b"CMDS", &self.cmds));
        for (id, body) in &self.trailing {
            out.extend(atom(id, body));
        }
        out.extend([0u8; 16]);
        out
    }
}

/// Writes an X-Plane install under `root`: `Custom Scenery/scenery_packs.ini`
/// listing `packs` top to bottom (highest priority first), and each
/// pack's DSF at `Earth nav data/<block>/<tile>.dsf`. `None` bytes
/// writes `b"garbage"` (an unreadable DSF). Used by Tasks 9 and 10.
pub fn install(root: &std::path::Path, tile: (i32, i32), packs: &[(&str, Option<Vec<u8>>)]) {
    let cs = root.join("Custom Scenery");
    std::fs::create_dir_all(&cs).unwrap();
    let mut ini = String::from("I\n1000 Version\nSCENERY\n\n");
    for (name, bytes) in packs {
        ini.push_str(&format!("SCENERY_PACK Custom Scenery/{name}/\n"));
        let dir = cs
            .join(name)
            .join("Earth nav data")
            .join(Builder::block(tile.0, tile.1));
        std::fs::create_dir_all(&dir).unwrap();
        let file = format!("{}.dsf", crate::night::store::tile_name(tile.0, tile.1));
        let body = bytes.clone().unwrap_or_else(|| b"garbage".to_vec());
        std::fs::write(dir.join(file), body).unwrap();
    }
    std::fs::write(cs.join("scenery_packs.ini"), ini).unwrap();
}

/// `dsf` as the single entry of a 7z, as X-Plane ships a DSF.
pub fn sevenz(dsf: &[u8]) -> Vec<u8> {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
    let mut w = ArchiveWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.push_archive_entry(
        ArchiveEntry::new_file("tile.dsf"),
        Some(std::io::Cursor::new(dsf.to_vec())),
    )
    .unwrap();
    w.finish().unwrap().into_inner()
}

pub(crate) fn chain_cmd(b: &mut Builder, pool: u16, def: u8, subtype: u8, idx: &[u16]) {
    b.cmds.push(1); // PoolSelect
    b.cmds.extend(pool.to_le_bytes());
    b.cmds.push(3); // SetDefinition8
    b.cmds.push(def);
    b.cmds.push(6); // SetRoadSubtype8
    b.cmds.push(subtype);
    b.cmds.push(9); // NetworkChain
    b.cmds.push(idx.len() as u8);
    for i in idx {
        b.cmds.extend(i.to_le_bytes());
    }
}

pub(crate) fn two_segment_dsf(enc: u8) -> Builder {
    let mut b = Builder::new(46, 6);
    b.props.push(("sim/overlay".into(), "1".into()));
    b.props.push(("sim/exclude_net".into(), "6/46/7/47".into()));
    b.nets.push("lib/g10/roads_EU.net".into());
    // Junction, shape, junction (mid), shape, junction.
    let pool = b.pool(
        46,
        6,
        &[
            (6.10, 46.10, 1),
            (6.20, 46.20, 0),
            (6.30, 46.30, 2),
            (6.40, 46.40, 0),
            (6.50, 46.50, 3),
        ],
        enc,
    );
    chain_cmd(&mut b, pool, 0, 50, &[0, 1, 2, 3, 4]);
    b
}

mod tests {
    use super::*;
    use crate::night::roads::dsf::read_dsf;

    #[test]
    fn builder_block_folders() {
        assert_eq!(Builder::block(37, -123), "+30-130");
        assert_eq!(Builder::block(46, 6), "+40+000");
    }

    #[test]
    fn builder_road_reads_back_as_one_segment() {
        let mut b = Builder::new(46, 6);
        b.road(46, 6, 30, &[(6.1, 46.1), (6.2, 46.15), (6.3, 46.2)]);
        let r = read_dsf(&b.bytes(), |_| true).unwrap();
        assert_eq!(r.chains.subtype, vec![30]);
        assert_eq!(r.chains.points(0), 0..3);
    }

    #[test]
    fn builder_install_lays_out_packs_in_priority_order() {
        let dir = tempfile::tempdir().unwrap();
        let dsf = two_segment_dsf(0).bytes();
        install(
            dir.path(),
            (46, 6),
            &[("A", Some(dsf.clone())), ("B", None)],
        );
        let cs = dir.path().join("Custom Scenery");
        let ini = std::fs::read_to_string(cs.join("scenery_packs.ini")).unwrap();
        let a = ini.find("Custom Scenery/A/").unwrap();
        assert!(a < ini.find("Custom Scenery/B/").unwrap());
        let file = |p: &str| cs.join(p).join("Earth nav data/+40+000/+46+006.dsf");
        assert_eq!(std::fs::read(file("A")).unwrap(), dsf);
        assert_eq!(std::fs::read(file("B")).unwrap(), b"garbage");
    }
}

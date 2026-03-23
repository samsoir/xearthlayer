use super::parser::parse_terrain_def_zoom;

#[test]
fn test_parse_bing_zl16() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/22112_34224_BI16.ter"),
        Some(16)
    );
}

#[test]
fn test_parse_bing_zl18() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/88416_136896_BI18.ter"),
        Some(18)
    );
}

#[test]
fn test_parse_bing_zl18_sea() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/88416_136896_BI18_sea.ter"),
        Some(18)
    );
}

#[test]
fn test_parse_bing_zl18_sea_overlay() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/88416_136896_BI18_sea_overlay.ter"),
        Some(18)
    );
}

#[test]
fn test_parse_go2_zl18() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/38800_80512_GO218.ter"),
        Some(18)
    );
}

#[test]
fn test_parse_go2_zl16() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/25264_10368_GO216.ter"),
        Some(16)
    );
}

#[test]
fn test_parse_google_zl16() {
    assert_eq!(
        parse_terrain_def_zoom("terrain/25264_10368_GO16.ter"),
        Some(16)
    );
}

#[test]
fn test_parse_terrain_water() {
    assert_eq!(parse_terrain_def_zoom("terrain_Water"), None);
}

#[test]
fn test_parse_no_zoom() {
    assert_eq!(parse_terrain_def_zoom("terrain/unknown_format.ter"), None);
}

#[test]
fn test_parse_bare_filename_matches() {
    let full = format!("terrain/{}", "88416_136896_BI18_sea.ter");
    assert_eq!(parse_terrain_def_zoom(&full), Some(18));
}

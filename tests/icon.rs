use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{fs, io::Cursor};
use zcraft::{
    config::Config,
    handler::status::{
        StatusService,
        icon::{MAX_ICON_BYTES, ServerIcon},
    },
};

const ICON: &[u8] = include_bytes!("../server-icon.png");

#[test]
fn bundled_icon_is_64_square_rgba_and_roundtrips_as_data_uri() {
    let icon = ServerIcon::from_png(ICON).unwrap();
    let data = icon
        .data_uri()
        .strip_prefix("data:image/png;base64,")
        .unwrap();
    assert_eq!(STANDARD.decode(data).unwrap(), ICON);
    let reader = png::Decoder::new(Cursor::new(ICON)).read_info().unwrap();
    assert_eq!((reader.info().width, reader.info().height), (64, 64));
    assert_eq!(reader.info().color_type, png::ColorType::Rgba);
}

#[test]
fn status_includes_icon_only_when_supplied() {
    let icon = ServerIcon::from_png(ICON).unwrap();
    let service = StatusService::with_icon("ZCraft", 20, Some(&icon));
    let json: serde_json::Value = serde_json::from_str(&service.response().json).unwrap();
    assert_eq!(json["favicon"], icon.data_uri());
    let service = StatusService::new("ZCraft", 20);
    let json: serde_json::Value = serde_json::from_str(&service.response().json).unwrap();
    assert!(json.get("favicon").is_none());
}

#[test]
fn wrong_dimensions_are_rejected() {
    let mut wrong_size = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut wrong_size, 32, 32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&vec![255; 32 * 32 * 4]).unwrap();
    }
    assert!(
        ServerIcon::from_png(&wrong_size)
            .unwrap_err()
            .to_string()
            .contains("64x64")
    );
}

#[test]
fn corrupt_png_is_not_accepted_just_because_its_header_is_valid() {
    assert!(ServerIcon::from_png(b"not PNG").is_err());
    assert!(ServerIcon::from_png(&ICON[..33]).is_err());
    assert!(ServerIcon::from_png(&ICON[..ICON.len() - 12]).is_err());
    let mut corrupt = ICON.to_vec();
    corrupt[50] ^= 0xff;
    assert!(ServerIcon::from_png(&corrupt).is_err());
    assert!(ServerIcon::from_png(&vec![0; MAX_ICON_BYTES + 1]).is_err());
}

#[test]
fn missing_icon_is_optional_but_invalid_existing_icon_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("icon.png");
    assert!(ServerIcon::load_optional(&path).unwrap().is_none());
    fs::write(&path, "broken").unwrap();
    let error = match ServerIcon::load_optional(&path) {
        Err(error) => error,
        Ok(_) => panic!("expected invalid icon"),
    };
    assert!(error.to_string().contains("icon.png"));
    assert!(ServerIcon::load_optional(dir.path()).is_err());
}

#[test]
fn configured_icon_is_cached_and_can_be_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("custom.png");
    fs::write(&path, ICON).unwrap();
    let mut config = Config {
        server_icon: path.to_str().unwrap().into(),
        ..Config::default()
    };
    let service = StatusService::from_config(&config).unwrap();
    fs::remove_file(&path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&service.response().json).unwrap();
    assert_eq!(
        json["favicon"],
        ServerIcon::from_png(ICON).unwrap().data_uri()
    );
    for icon_path in [path.to_str().unwrap(), ""] {
        config.server_icon = icon_path.into();
        let service = StatusService::from_config(&config).unwrap();
        let json: serde_json::Value = serde_json::from_str(&service.response().json).unwrap();
        assert!(json.get("favicon").is_none());
    }
}

#[test]
fn oversized_combined_status_response_is_rejected_at_startup() {
    let config = Config {
        server_icon: String::new(),
        motd: "x".repeat(32767),
        ..Config::default()
    };
    assert!(StatusService::from_config(&config).is_err());
}

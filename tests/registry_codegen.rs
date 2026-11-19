// Exercise the same generator, extraction and cache code used by build.rs.
#[path = "../build_support/archive.rs"]
mod archive;
#[path = "../build_support/cache.rs"]
mod cache;
#[path = "../build_support/registry.rs"]
mod registry;
#[path = "../build_support/tags.rs"]
mod tags;

#[test]
fn verified_official_jar_can_regenerate_the_embedded_protocol_data() {
    // The build has already succeeded, so these generated packet contracts must
    // be available even when running this test without network access.
    assert_eq!(zcraft::registry_data::REGISTRY_COUNTS.len(), 11);
    if let Some(jar) = std::env::var_os("ZCRAFT_TEST_SERVER_JAR") {
        let jar = std::path::PathBuf::from(jar);
        cache::verify_jar(&jar).unwrap();
        let extracted = tempfile::tempdir().unwrap();
        archive::extract(&jar, extracted.path()).unwrap();
        let first = registry::generate(extracted.path()).unwrap();
        let second = registry::generate(extracted.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(zcraft::registry_data::TAG_COUNTS.iter().sum::<usize>(), 514);
        assert!(first.contains("pub const TAGS_PACKET:"));
        let directory = tempfile::tempdir().unwrap();
        cache::publish(directory.path(), "test-generator", &first).unwrap();
        assert_eq!(
            cache::read_source(directory.path(), "test-generator")
                .unwrap()
                .unwrap(),
            first
        );
    }
}

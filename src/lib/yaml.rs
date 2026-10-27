//! Shared parsing policy for repository and publication YAML files.

pub(crate) fn options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        // Preserve serde_yaml's YAML 1.2 booleans and literal merge keys.
        strict_booleans: true,
        merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
        // Keep source locations and causes without echoing adjacent config values.
        with_snippet: false,
    }
}

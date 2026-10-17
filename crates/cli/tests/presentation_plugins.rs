//! `/reload` and session start apply the server's presentation-plugin data in the client
//! (`createPresentationFacetLoaders`, plugins/bundled.ts; client-tui.ts:579-600): the data must be an object whose
//! `presentationFacetBundles`, when present, is a list. Literal expectations of the TypeScript's errors; bundles are
//! JavaScript facets this client cannot load (the Rust server never selects any).

use cli::interactive::apply_presentation_plugins;
use serde_json::json;

#[test]
fn cli_presentation_plugins_data_is_checked_like_create_presentation_facet_loaders() {
    for ok in [json!({}), json!({"presentationFacetBundles": []}), json!({"other": 1})] {
        assert_eq!(apply_presentation_plugins(&ok), Ok(()), "{ok}");
    }
    for invalid in [json!(null), json!([]), json!("x"), json!(3)] {
        assert_eq!(
            apply_presentation_plugins(&invalid),
            Err("Invalid presentation plugin data".into()),
            "{invalid}"
        );
    }
    for list in [
        json!({"presentationFacetBundles": {}}),
        json!({"presentationFacetBundles": "a"}),
    ] {
        assert_eq!(
            apply_presentation_plugins(&list),
            Err("Invalid presentation plugin bundle list".into()),
            "{list}"
        );
    }
    assert_eq!(
        apply_presentation_plugins(&json!({"presentationFacetBundles": [{"id": "a"}, {"id": "b"}]})),
        Err("2 presentation plugin bundle(s) cannot be loaded by this client".into())
    );
}

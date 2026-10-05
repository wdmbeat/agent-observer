//! Golden regression for the initial publication: the typed producer
//! (`ChallengeWorkflow::initial_publication` building `schema::InitialPublication`
//! and serializing with serde) must emit exactly the wire document the old
//! hand-built `json!` producer emitted. Goldens were captured from the
//! pre-refactor producer; regenerate with
//! `AGENT_OBSERVER_UPDATE_GOLDENS=1 cargo test --test initial_publication`
//! only after verifying a deliberate wire change.

mod common;

use common::{golden_dir, scenario_dir, SCENARIOS};

#[test]
fn initial_publication_matches_golden() {
    for name in SCENARIOS {
        let workflow = agent_observer::workflow::ChallengeWorkflow::new(&scenario_dir(name))
            .unwrap_or_else(|error| panic!("{name}: building workflow: {error}"));
        let publication = serde_json::to_value(
            workflow
                .initial_publication()
                .unwrap_or_else(|error| panic!("{name}: building initial publication: {error}")),
        )
        .unwrap();
        let golden_path = golden_dir(name).join("initial_publication.json");
        if std::env::var_os("AGENT_OBSERVER_UPDATE_GOLDENS").is_some() {
            let pretty = serde_json::to_string_pretty(&publication).unwrap() + "\n";
            std::fs::write(&golden_path, pretty).unwrap();
            continue;
        }
        let golden: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&golden_path)
                .unwrap_or_else(|error| panic!("{name}: reading golden: {error}")),
        )
        .unwrap();
        assert_eq!(
            publication, golden,
            "{name}: initial_publication drifted from the golden wire document"
        );
    }
}

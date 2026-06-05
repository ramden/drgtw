//! Integration tests against the real baseline model in models/ner-multilingual.
//! Slow (~seconds): loads a 178MB quantized BERT once per process.

use std::path::PathBuf;
use std::sync::OnceLock;

use drgtw_ner::{NerKind, NerModel, NerPool, NerPoolConfig, NerSpan};

fn model_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/ner-multilingual")
}

fn shared_model() -> &'static NerModel {
    static MODEL: OnceLock<NerModel> = OnceLock::new();
    MODEL.get_or_init(|| NerModel::load(&model_dir()).expect("baseline model loads"))
}

fn detect(text: &str) -> Vec<NerSpan> {
    let mut spans = shared_model().detect(text).expect("inference ok");
    spans.retain(|s| s.score >= 0.5);
    spans
}

fn texts_of(spans: &[NerSpan], text: &str, kind: NerKind) -> Vec<String> {
    spans
        .iter()
        .filter(|s| s.kind == kind)
        .map(|s| text[s.start..s.end].to_string())
        .collect()
}

#[test]
fn english_person_org_location() {
    let text = "Max Mustermann works at Example Corp in Munich.";
    let spans = detect(text);
    let persons = texts_of(&spans, text, NerKind::Person);
    let orgs = texts_of(&spans, text, NerKind::Org);
    let locs = texts_of(&spans, text, NerKind::Location);

    assert!(
        persons.iter().any(|p| p.contains("Max Mustermann")),
        "persons: {persons:?}"
    );
    assert!(orgs.iter().any(|o| o.contains("Example Corp")), "orgs: {orgs:?}");
    assert!(locs.iter().any(|l| l.contains("Munich")), "locs: {locs:?}");
}

#[test]
fn german_sentence() {
    let text = "Schreib eine Mail an Angela Schmidt von der Deutschen Bank in Berlin.";
    let spans = detect(text);
    let persons = texts_of(&spans, text, NerKind::Person);
    let locs = texts_of(&spans, text, NerKind::Location);

    assert!(
        persons.iter().any(|p| p.contains("Angela Schmidt")),
        "persons: {persons:?}"
    );
    assert!(locs.iter().any(|l| l.contains("Berlin")), "locs: {locs:?}");
}

#[test]
fn multibyte_spans_are_char_aligned() {
    let text = "Café-Besitzer François Dupont wohnt in Zürich.";
    let spans = detect(text);
    for s in &spans {
        assert!(text.is_char_boundary(s.start), "start not boundary: {s:?}");
        assert!(text.is_char_boundary(s.end), "end not boundary: {s:?}");
    }
    let persons = texts_of(&spans, text, NerKind::Person);
    assert!(
        persons.iter().any(|p| p.contains("François")),
        "persons: {persons:?}"
    );
}

#[test]
fn no_entities_in_plain_text() {
    let spans = detect("the weather is nice today and tomorrow it may rain");
    assert!(
        spans.is_empty(),
        "expected no entities ≥0.5, got: {spans:?}"
    );
}

#[test]
fn pool_matches_direct_inference_and_handles_concurrency() {
    let model = NerModel::load(&model_dir()).expect("model loads");
    let direct = {
        let text = "Max Mustermann works at Example Corp in Munich.";
        let mut s = model.detect(text).unwrap();
        s.retain(|x| x.score >= 0.5);
        s
    };

    let pool = std::sync::Arc::new(NerPool::new(
        NerModel::load(&model_dir()).unwrap(),
        NerPoolConfig::default(),
    ));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let text = "Max Mustermann works at Example Corp in Munich.";
                let mut s = pool.detect(text).expect("pool detect ok");
                s.retain(|x| x.score >= 0.5);
                s
            })
        })
        .collect();

    for h in handles {
        let pooled = h.join().unwrap();
        assert_eq!(pooled, direct, "pool result differs from direct inference");
    }
}

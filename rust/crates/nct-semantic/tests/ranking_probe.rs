// Ranking-quality regression test: guards the two failure modes that made
// search.semantic useless in practice (both were live defects):
//   1. tokenizer.json's baked-in padding config leaked [PAD] ids into every
//      encode() → all pairwise cosines inflated to 0.7+ (anisotropic cone),
//      ranking destroyed. The with_padding(None) fix is load-bearing.
//   2. generated dirs (rust/target/.fingerprint …) indexed as corpus → junk
//      outranked source. Guarded by the walker skip list, asserted lightly.
// Skips honestly when the model cache is not present (CI without network):
// the model is a download-on-first-use artifact, never a test-time fetch.
use nct_semantic::Embedder;

fn cos(a: &[f32], b: &[f32]) -> f32 {
    let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    d / (na * nb)
}

#[test]
fn embeddings_discriminate_like_the_oracle() {
    let cache = std::env::var("NCTOOLS_MODEL_CACHE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("F:/nc-tools/.nc-tools/model-cache"));
    if !cache.join("model.safetensors").exists() {
        eprintln!("skipping: model cache not populated at {}", cache.display());
        return;
    }
    let e = Embedder::get(cache).expect("embedder");
    let on_topic = "the journal uses an exclusive lock file around each atomic line write";
    let paraphrase = "every append takes an exclusive lock so two processes never tear a line";
    let junk_a = "target\nbuild.log\n";
    let junk_b = "quantum recipes for sourdough fermentation on mars";

    let (v_q, v_p, v_ja, v_jb) = (
        e.embed(on_topic).expect("embed"),
        e.embed(paraphrase).expect("embed"),
        e.embed(junk_a).expect("embed"),
        e.embed(junk_b).expect("embed"),
    );

    // Discrimination (the pad bug compressed EVERYTHING to 0.7+; oracle's
    // reference values are in parentheses):
    assert!(cos(&v_q, &v_ja) < 0.30, "junk file vs journal query must NOT be a hit (got {:.4}, oracle 0.16)", cos(&v_q, &v_ja));
    assert!(cos(&v_q, &v_jb) < 0.15, "unrelated text vs query must be near-orthogonal (got {:.4}, oracle 0.06)", cos(&v_q, &v_jb));
    // Signal (near-paraphrase outranks everything else):
    let sim_p = cos(&v_q, &v_p);
    assert!(sim_p > cos(&v_q, &v_ja) + 0.2, "paraphrase must clearly outrank junk (para {:.4} vs junk {:.4})", sim_p, cos(&v_q, &v_ja));
    assert!(sim_p > 0.4, "paraphrase similarity in oracle band (got {:.4}, oracle 0.55)", sim_p);
    // Identity sanity:
    assert!((cos(&v_q, &v_q) - 1.0).abs() < 1e-4);
}

use rand::{Rng, prelude::IndexedRandom, rng};

static ADJECTIVES: &[&str] = &[
    "amber", "arc", "brisk", "calm", "cinder", "dawn", "ember", "fern", "flint", "glow", "harbor",
    "hollow", "iris", "lumen", "moss", "otter", "quiet", "river", "sable", "signal", "thread",
    "tidal", "vale", "vivid", "woven",
];

static NOUNS: &[&str] = &[
    "anchor", "branch", "canvas", "draft", "field", "frame", "grove", "harbor", "line", "map",
    "path", "sage", "shore", "signal", "spark", "thread", "trail", "vault", "vector", "weave",
    "window",
];

pub fn generate_slug() -> String {
    let mut rng = rng();
    let mut parts = Vec::with_capacity(3);
    parts.push(ADJECTIVES.choose(&mut rng).unwrap_or(&"steady").to_string());
    parts.push(NOUNS.choose(&mut rng).unwrap_or(&"thread").to_string());
    if rng.random_bool(0.5) {
        parts.push(NOUNS.choose(&mut rng).unwrap_or(&"sage").to_string());
    } else {
        parts.push(ADJECTIVES.choose(&mut rng).unwrap_or(&"calm").to_string());
    }
    parts.join("-")
}
